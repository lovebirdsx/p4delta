//! 两阶段差异分析：把 depot 记录与工作区文件互相印证，决定每个文件落在哪一类变更里。
//!
//! 这里是纯逻辑——不碰文件系统、不起 p4 子进程、不读时钟。`reconcile_dir` 负责把
//! fstat / 工作区扫描 / have 三路结果凑齐，再把工作区补齐（已剪目录里 depot 已跟踪
//! 的文件必须回到 `workspace` 里，否则会被当成被删除），然后才交给 [`analyze`]。

use std::collections::HashSet;

use anyhow::{Result, bail};

use crate::model::{
    DepotFileRecord, DepotState, DigestType, FileAction, FileType, TargetMap, WorkspaceFile,
    WorkspaceState,
};

use super::changes::Changes;

/// 一次差异分析的全部产出。
pub(crate) struct Analysis<'a> {
    /// 已经确定分类的变更。
    pub(crate) changes: Changes,

    /// 需要算摘要才能定夺的文件。按判定方向分成三组，因为摘要结果的含义不同：
    /// 第一组摘要不等就是 edit，后两组摘要相等才是要 revert。
    pub(crate) check_edit: Vec<(&'a WorkspaceFile, DigestType)>,
    pub(crate) check_revert_edit: Vec<(&'a WorkspaceFile, DigestType)>,
    pub(crate) check_revert_delete_or_reopen_edit: Vec<(&'a WorkspaceFile, DigestType)>,

    /// 算不出摘要的文件，转交 `p4 reconcile` 处理。
    pub(crate) unsupported_files: Vec<&'a DepotFileRecord>,

    /// depot 里有、但这个客户端从没同步过的文件。
    pub(crate) unsynced_files: Vec<String>,

    /// head 是归档版本的文件。内容已经移出 depot，摘要无从谈起，只能跳过；
    /// 但要汇报一声，否则用户看到「一切正常」而文件其实没被检查过。
    pub(crate) archived_files: Vec<String>,
}

/// 守卫报错的统一构造：说清触发了哪条规则，再附上判断依据的记录状态。
///
/// `label` 是规则短名：`analyze` 用 `rule N`，编号沿用旧消息里的数字，测试按它把每条
/// 分支的失败行为逐条钉住；`analyze_at_target` 用 `sync`。`missing` 是该规则需要、而
/// 记录里没有的字段（形状本身未建模的规则传空）——旧消息只有路径和编号，日志里看不出
/// 记录是什么形状、缺了什么。
fn guard_error(
    label: &str,
    rule: &str,
    path: &str,
    record: &DepotFileRecord,
    missing: &[&str],
) -> String {
    let mut message = format!(
        "Cannot handle \"{path}\" ({label}: {rule}); action={:?} head_action={:?} \
         head_rev={:?} have_rev={:?}",
        record.action, record.head_action, record.head_rev, record.have_rev
    );

    if !missing.is_empty() {
        message.push_str(&format!("; missing {}", missing.join(", ")));
    }

    message
}

/// `(head_type, file_size)` 成对匹配的守卫需要这两个字段：缺哪个列哪个，
/// 作为 [`guard_error`] 的缺失字段列表。
fn missing_type_or_size(record: &DepotFileRecord) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if record.head_type.is_none() {
        missing.push("head_type");
    }
    if record.file_size.is_none() {
        missing.push("file_size");
    }
    missing
}

/// 两阶段分析：phase one 拿 depot 记录找工作区里缺了什么，phase two 反过来。
pub(crate) fn analyze<'a>(
    depot: &'a DepotState,
    workspace: &'a WorkspaceState,
    verbose: bool,
) -> Result<Analysis<'a>> {
    //
    // Known cases. Only files added here will actually be changed in the end.
    // Every file must be added to either no or ONE of the below categories.
    //

    let mut changes = Changes::default();

    //
    // Cases that need digest computation to decide whether we should add them above.
    // We want to get as few as possible files here, but it's not always that nice.
    //

    // Files in workspace, maybe changed from have revision, but not checked out for edit.
    let mut check_edit = Vec::new();

    // Files in workspace, maybe not changed from have revision, but checked out for edit.
    let mut check_revert_edit = Vec::new();

    // Files in workspace, maybe not changed from have revision, but checked out for delete.
    let mut check_revert_delete_or_reopen_edit = Vec::new();

    // Files in workspace, with file types we do not support calculating checksums for
    let mut unsupported_files = Vec::new();

    // Depot files this client never synced that a local file happens to share a name with.
    let mut unsynced_files = Vec::new();

    // Files whose head revision is archived; we have no way to check them.
    let mut archived_files = Vec::new();

    //
    // Analysis phase one: Check depot records against workspace files.
    //

    use FileAction::*;

    for record in &depot.file_records {
        // 类型名认不出来就没法算摘要。放在最前面，免得后面的分支因为 head_type
        // 为空而误判成「记录不完整」直接 bail 掉整轮。
        if record.unsupported_type.is_some() {
            unsupported_files.push(record);
            continue;
        }

        match record.head_action {
            // These are either irrelevant or will get caught in phase two, skip those.
            Some(Delete | MoveDelete) => (),
            // 归档版本的内容已经移出 depot，没有摘要可比。跳过而不是中止整轮：
            // 一个归档文件不该让整个工作区没法 reconcile。
            Some(Archive) => archived_files.push(record.client_file.clone()),
            // These exist in the depot, check if we still have them.
            Some(Add | Edit | MoveAdd | Branch | Integrate | Import | Purge) => {
                // The depot state may contain new files we haven't synced yet, skip those.
                if record.have_rev.is_some() {
                    match record.action {
                        // If we have them open for delete, but still have the file, revert that.
                        Some(Delete | MoveDelete) => {
                            if let Some(file) = workspace.get_filtered(&record.client_file_lower) {
                                if let (Some(file_type), Some(size)) =
                                    (record.head_type, record.file_size)
                                {
                                    if size == file.size || file_type != FileType::Binary {
                                        check_revert_delete_or_reopen_edit
                                            .push((file, file_type.digest_type()?));

                                        if verbose {
                                            println!(
                                                "         File \"{}\" needs digest check for revert delete or reopen edit",
                                                file.path
                                            );
                                        }
                                    } else {
                                        changes.reopen_edit.push(record.client_file.clone());

                                        if verbose {
                                            println!(
                                                "         File \"{}\" has different length for reopen edit",
                                                file.path
                                            );
                                        }
                                    }
                                } else {
                                    let error = guard_error(
                                        "rule 1",
                                        "open for delete while the file is still present locally; \
                                         a digest check needs head_type and file_size",
                                        &record.client_file,
                                        record,
                                        &missing_type_or_size(record),
                                    );
                                    bail!("{error}");
                                }
                            }
                        }
                        // If we have them open for edit in some way, but don't have the file, reopen as delete.
                        Some(Edit | Integrate) => {
                            // No filter, adding a new ignore rule should not cause edits to reopen as deletions.
                            if !workspace.has_file(&record.client_file_lower) {
                                changes.reopen_delete.push(record.client_file.clone());
                            }
                        }
                        // Otherwise, if we don't have the file, open as delete.
                        None => {
                            // No filter, adding a new ignore rule should not cause deletions.
                            if !workspace.has_file(&record.client_file_lower) {
                                changes.delete.push(record.client_file.clone());
                            }
                        }
                        _ => bail!(
                            "{}",
                            guard_error(
                                "rule 2",
                                "the file exists at the comparison revision and this client has it synced, but it \
                                 is open with an action the analysis does not model",
                                &record.client_file,
                                record,
                                &[],
                            )
                        ),
                    }
                }
            }
            // These don't exist in the depot and can only be here because we opened them for add.
            None => match record.action {
                Some(Add | MoveAdd | Branch) => {
                    // 本地既不存在、又是被忽略的文件时，这个 pending add 已经失去意义。
                    if !workspace.has_unfiltered_file(&record.client_file_lower) {
                        changes.revert_add.push(record.client_file.clone());
                    }
                }
                _ => bail!(
                    "{}",
                    guard_error(
                        "rule 3",
                        "the file is not in the depot at head, and is open with an action \
                         other than add/move-add/branch",
                        &record.client_file,
                        record,
                        &[],
                    )
                ),
            },
        }
    }

    //
    // Analysis phase two: Check workspace files against depot records.
    //

    for file in workspace.files.iter().filter(|f| !f.filtered) {
        // First check if the file is present in the depot state.
        if let Some(record) = depot.get_client_record(&file.path_lower) {
            // phase one 已经把它收进 unsupported_files 了，这里跳过，免得重复汇报。
            if record.unsupported_type.is_some() {
                continue;
            }

            // Skip files we don't know how to calculate the checksum of
            if let Some(FileType::Apple | FileType::Resource) = record.head_type {
                unsupported_files.push(record);
                continue;
            }

            // Check what the depot states the file should be.
            match record.head_action {
                // We already took care of files we opened for add in phase one.
                None => (),
                // 归档版本没有可比的内容，phase one 已经汇报过了。
                Some(Archive) => (),
                // It's deleted at head, but we have the file.
                Some(Delete | MoveDelete) => {
                    match record.action {
                        // We already have it marked for add, skip.
                        Some(Add | MoveAdd | Branch) => (),
                        // We don't have it marked for add yet.
                        None => {
                            changes.add.push(file.path.clone());
                        }
                        _ => bail!(
                            "{}",
                            guard_error(
                                "rule 5",
                                "deleted at the comparison revision while the file is still present locally, but \
                                 open with an action the analysis does not model",
                                &file.path,
                                record,
                                &[],
                            )
                        ),
                    }
                }
                // It already exists in the depot, check if we need to do something.
                Some(Add | Edit | MoveAdd | Branch | Integrate | Import | Purge) => {
                    // Phase one skips depot records the client never synced; phase two has to
                    // do the same from the other direction. A local file that merely shares the
                    // name is not a modification of the depot revision - `p4 edit` rejects it
                    // with "file(s) not on client" - so leave it for the user to sync or
                    // resolve, exactly as `p4 reconcile` does.
                    if record.have_rev.is_none() {
                        unsynced_files.push(record.client_file.clone());
                        continue;
                    }
                    match record.action {
                        // We should leave these alone as they can submit even with no changes made.
                        Some(Integrate) => (),
                        // We already took care of these in phase one.
                        Some(Delete | MoveDelete) => (),
                        // We have it open for edit, check if we reverted the change.
                        Some(Edit) => {
                            if let (Some(file_type), Some(size)) =
                                (record.head_type, record.file_size)
                            {
                                if size == file.size || file_type != FileType::Binary {
                                    check_revert_edit.push((file, file_type.digest_type()?));

                                    if verbose {
                                        println!(
                                            "         File \"{}\" needs digest check for revert edit",
                                            file.path
                                        );
                                    }
                                }
                            } else {
                                let error = guard_error(
                                    "rule 6",
                                    "open for edit; a digest check needs head_type and file_size",
                                    &file.path,
                                    record,
                                    &missing_type_or_size(record),
                                );
                                bail!("{error}");
                            }
                        }
                        // We don't have it open, check if we should.
                        None => {
                            if let (Some(file_type), Some(size)) =
                                (record.head_type, record.file_size)
                            {
                                if size != file.size && file_type == FileType::Binary {
                                    changes.edit.push(file.path.clone());

                                    if verbose {
                                        println!(
                                            "         File \"{}\" has different length for edit",
                                            file.path
                                        );
                                    }
                                } else {
                                    check_edit.push((file, file_type.digest_type()?));

                                    if verbose {
                                        println!(
                                            "         File \"{}\" needs digest check for edit",
                                            file.path
                                        );
                                    }
                                }
                            }
                        }
                        _ => bail!(
                            "{}",
                            guard_error(
                                "rule 7",
                                "the file exists at the comparison revision and this client has it synced, but it \
                                 is open with an action the analysis does not model",
                                &file.path,
                                record,
                                &[],
                            )
                        ),
                    }
                }
            }
        } else {
            // The file is not in the depot at all and not ignored, mark for add.
            changes.add.push(file.path.clone());
        }
    }

    Ok(Analysis {
        changes,
        check_edit,
        check_revert_edit,
        check_revert_delete_or_reopen_edit,
        unsupported_files,
        unsynced_files,
        archived_files,
    })
}

/// sync 模式下要拉到目标版本的 depot 记录，连同目标修订。
///
/// 修订号单独带着而不是从记录里读：`head_rev` 是 head 的修订号，只有在 head 目标下
/// 它才恰好等于目标修订；`--to <CL>` 下两者是不同的东西。
#[derive(Clone, Copy)]
pub(crate) struct SyncSource<'a> {
    pub(crate) record: &'a DepotFileRecord,
    pub(crate) target_rev: u32,
}

/// 一个「目标版本没变，但本地内容可能已经不是它了」的文件：要靠摘要定夺。
pub(crate) struct DigestCheck<'a> {
    pub(crate) file: &'a WorkspaceFile,
    pub(crate) source: SyncSource<'a>,
    pub(crate) digest_type: DigestType,
}

/// 目标时刻该路径不在库、本地却有的文件：要删掉。
///
/// 两个路径都有用处，且都不是摆设：本地路径用来删文件，depot 路径用来让 p4 清掉 have
/// 记录（两者各有一种对方覆盖不到的情形，见 `sync::DeleteFile` 的注释）。
pub(crate) struct DeletedAtTarget<'a> {
    pub(crate) record: &'a DepotFileRecord,
    pub(crate) file: &'a WorkspaceFile,
}

/// [`analyze_at_target`] 的全部产出。
///
/// 与 [`Analysis`] 平行而不是复用：那八类对着 `p4 reconcile` 的分类学，被测试逐条钉死；
/// 这里的分法只服务于「把工作区拉到目标版本」，判据与动作都不一样。
#[derive(Default)]
pub(crate) struct SyncAnalysis<'a> {
    /// 本地有一份、但不是目标版本：拉到目标版本覆盖它，摘要在这里没有意义。
    pub(crate) update: Vec<SyncSource<'a>>,

    /// 二进制长度就已经不符，确定要还原的。不必再读一遍内容。
    pub(crate) revert: Vec<SyncSource<'a>>,

    /// 目标版本没变，本地内容要对摘要才知道对不对的。
    pub(crate) check: Vec<DigestCheck<'a>>,

    /// 本地没有（或被忽略规则盖着）：写回来。与 have 是哪一版无关。
    pub(crate) restore: Vec<SyncSource<'a>>,

    /// 目标时刻该路径不在库，本地却有的：删掉。
    pub(crate) delete: Vec<DeletedAtTarget<'a>>,

    /// 算不出摘要的类型，转交原生 `p4 sync`。
    pub(crate) unsupported: Vec<&'a DepotFileRecord>,

    /// 目标版本是归档版本的文件。内容已经移出 depot，摘要无从谈起，只能跳过；
    /// 但要汇报一声，否则用户看到「一切正常」而文件其实没被检查过。
    pub(crate) archived: Vec<&'a DepotFileRecord>,
}

/// 目标时刻不在库的路径：本地有一份就收进删除组。
///
/// 被忽略的本地文件视同不存在——与其余各组「filtered 当作本地没有」的口径一致，
/// 而对这一组来说，那个口径的含义正好是「不去删被忽略的东西」。
fn push_deleted<'a>(
    analysis: &mut SyncAnalysis<'a>,
    record: &'a DepotFileRecord,
    workspace: &'a WorkspaceState,
) {
    if let Some(file) = workspace.get_filtered(&record.client_file_lower) {
        analysis.delete.push(DeletedAtTarget { record, file });
    }
}

/// 按**目标版本**分类，而不是按 have 版本。
///
/// 与 [`analyze`] 是两条平行路径，刻意不合并（理由见 [`SyncAnalysis`]）。判定表：
///
/// | 条件 | 落点 |
/// |---|---|
/// | 已打开 | 不动作、不汇报（`p4 sync -f` 也不碰已打开的文件，这里更彻底） |
/// | 目标时刻该路径不在库 | `delete`（本地有才落） |
/// | 目标是归档版本 | `archived` |
/// | 本地缺失（或被忽略规则盖着） | `restore`——不论 have 是哪一版 |
/// | `have != target_rev` | `update` |
/// | 二进制长度不符 | `revert` |
/// | 其余 | `check`（摘要说了算） |
///
/// `target` 里查不到即视为目标时刻不在库。两种来源都归此列：目标时刻已是删除版本，
/// 或（只有 changelist 目标才可能）目标时刻它还没进 depot——实测过 `p4 sync -f ./...@CL`
/// 对这两种都报 `deleted as`，把本地文件删掉。
pub(crate) fn analyze_at_target<'a>(
    depot: &'a DepotState,
    workspace: &'a WorkspaceState,
    target: &TargetMap,
) -> Result<SyncAnalysis<'a>> {
    use FileAction::*;

    let mut analysis = SyncAnalysis::default();

    for record in &depot.file_records {
        // 已打开的文件一律不碰。`p4 sync -f` 的官方口径也是 "does not affect open files"，
        // 而这里更彻底：既不动作也不汇报——打开的文件归用户，工具不参与。
        if record.action.is_some() {
            continue;
        }

        // 大小写冲突下同一个 client 路径可能对上多条记录，只处理 build_mapping 选出的
        // 胜者。否则同一个本地文件会被两条 depot 路径各下发一次（`p4 sync -f` 来两遍）。
        let is_winner = depot
            .get_client_record(&record.client_file_lower)
            .is_some_and(|winner| winner.depot_file_lower == record.depot_file_lower);
        if !is_winner {
            continue;
        }

        // 类型名认不出来就算不了摘要，和其它算不出摘要的一起转交原生 sync。
        if record.unsupported_type.is_some() {
            analysis.unsupported.push(record);
            continue;
        }

        // 目标时刻该路径不在库：本地不该留着它。
        let Some(target_record) = target.get(&record.depot_file_lower) else {
            push_deleted(&mut analysis, record, workspace);
            continue;
        };
        match target_record.action {
            Delete | MoveDelete => {
                push_deleted(&mut analysis, record, workspace);
                continue;
            }
            // 内容已经移出 depot，摘要无从谈起。跳过并汇报，而不是中止整轮。
            Archive => {
                analysis.archived.push(record);
                continue;
            }
            Add | Edit | MoveAdd | Branch | Integrate | Import | Purge => {}
        }

        let source = SyncSource {
            record,
            target_rev: target_record.rev,
        };

        // 本地没有这一份（文件缺失，或被忽略规则盖着、按本模块的口径视同不存在）：
        // 从 depot 写回。判断放在 have 之前是刻意的——这四组按**本地状态**分，不按 have
        // 状态分：本地压根没有的文件报成「Updating ... in workspace」既不准确，也丢掉了
        // 「它不在你本地」这个信息。原生 `p4 sync -f` 那边对应的是 `added as`。
        //
        // 被忽略的文件也走这里，不另外单列：实测 p4 自己会照常写回这类文件
        // （`.p4ignore` 拦不住 `p4 sync -f`），交给它做，工具不单方面覆盖。
        let Some(file) = workspace.get_filtered(&record.client_file_lower) else {
            analysis.restore.push(source);
            continue;
        };

        // have 与目标不一致：本地这一份不是目标版本（更新的，或 `--to <CL>` 下更旧的），
        // 传就是了。摘要在这里没有意义——算出来「不一样」也还是要传。
        if record.have_rev != Some(target_record.rev) {
            analysis.update.push(source);
            continue;
        }

        let (Some(file_type), Some(size)) = (record.head_type, record.file_size) else {
            let error = guard_error(
                "sync",
                "at the target revision with the local file present; a digest check needs \
                 head_type and file_size",
                &record.client_file,
                record,
                &missing_type_or_size(record),
            );
            bail!("{error}");
        };

        // 二进制长度不符就已经能判定内容变了，不必再读一遍内容。
        // 文本不能这么判：换行归一化会改变字节数，长度不同不代表内容不同。
        if size != file.size && file_type == FileType::Binary {
            analysis.revert.push(source);
            continue;
        }

        let digest_type = match file_type.digest_type() {
            Ok(digest_type) => digest_type,
            // Apple / Resource 的摘要算不出来，和认不出类型名的记录一样转交。
            Err(_) => {
                analysis.unsupported.push(record);
                continue;
            }
        };

        analysis.check.push(DigestCheck {
            file,
            source,
            digest_type,
        });
    }

    guard_case_conflicts(&analysis, depot, target)?;

    Ok(analysis)
}

/// 挡住「大小写冲突 + 目标态与 head 态背离」这个会误删本地文件的组合。
///
/// [`build_mapping`] 选出的胜者是按 **head** 事实定的（在世的优先、有 have 记录的优先），
/// 而「该路径在不在目标处」由目标快照决定——两者可以背离。depot 里做过一次只改大小写的
/// 改名（`//D/Snow.uasset` 已删、`//D/snow.uasset` 在世），`--to` 又指向改名之前时：按 head
/// 选出的胜者在目标处不在库，于是本地文件被收进删除组，可目标处其实有另一条路径的内容
/// 该落下来。删掉它是错的，而且不可逆。
///
/// 不做自动择一——那要定义一套「按目标态重新决胜」的新语义，而这里只是不想**默不作声**
/// 地删错东西。罕见组合，所以只在真的出现删除候选时才回头查一遍。
///
/// [`build_mapping`]: crate::model::DepotState::build_mapping
fn guard_case_conflicts(
    analysis: &SyncAnalysis<'_>,
    depot: &DepotState,
    target: &TargetMap,
) -> Result<()> {
    use FileAction::*;

    if analysis.delete.is_empty() {
        return Ok(());
    }

    let doomed: HashSet<&str> = analysis
        .delete
        .iter()
        .map(|entry| entry.record.client_file_lower.as_str())
        .collect();

    for record in &depot.file_records {
        if !doomed.contains(record.client_file_lower.as_str()) {
            continue;
        }

        // 同一条本地路径上，只要还有另一条记录在目标处留着内容，就不能删。
        let alive_at_target = target
            .get(&record.depot_file_lower)
            .is_some_and(|target_record| !matches!(target_record.action, Delete | MoveDelete));
        if alive_at_target {
            bail!(
                "Refusing to sync: the local path \"{}\" maps to more than one depot path, \
                 and {} still has content at the target revision while the record chosen for \
                 this workspace has none. Syncing would delete the local file even though the \
                 depot has something to put there. Resolve the case conflict by hand.",
                record.client_file,
                record.depot_file
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::UNIX_EPOCH;

    use crate::model::TargetRecord;
    use crate::path::local_path_key;

    const PATH: &str = r"C:\ws\a.txt";
    const SIZE: u64 = 100;

    fn depot_path_of(client_file: &str) -> String {
        let name = client_file
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(client_file);
        format!("//depot/{name}")
    }

    /// 一条「在库、已同步、未被打开」的文本记录。用例用结构体更新覆盖关心的字段。
    fn synced_record(client_file: &str) -> DepotFileRecord {
        let depot_file = depot_path_of(client_file);
        DepotFileRecord {
            depot_file_lower: depot_file.to_ascii_lowercase(),
            depot_file,
            client_file: client_file.to_owned(),
            client_file_lower: local_path_key(client_file),
            head_type: Some(FileType::Text),
            head_action: Some(FileAction::Edit),
            head_rev: Some(1),
            have_rev: Some(1),
            file_size: Some(SIZE),
            digest: Some([0xAB; 16]),
            ..Default::default()
        }
    }

    /// 一条只存在于 pending changelist 里、depot 中还没有修订的记录。
    fn open_add_record(client_file: &str) -> DepotFileRecord {
        DepotFileRecord {
            head_action: None,
            have_rev: None,
            action: Some(FileAction::Add),
            ..synced_record(client_file)
        }
    }

    fn depot_state(records: Vec<DepotFileRecord>) -> DepotState {
        // 不能写成结构体更新语法：depot_map / client_map 对 model 之外是私有的。
        let mut state = DepotState::default();
        state.file_records = records;
        state.build_mapping();
        state
    }

    /// `(路径, 大小, 是否被忽略)`。
    fn workspace_state(entries: &[(&str, u64, bool)]) -> WorkspaceState {
        let mut state = WorkspaceState {
            files: entries
                .iter()
                .map(|(path, size, filtered)| WorkspaceFile {
                    path: (*path).to_owned(),
                    path_lower: local_path_key(path),
                    size: *size,
                    date: UNIX_EPOCH,
                    filtered: *filtered,
                })
                .collect(),
            ..Default::default()
        };
        state.build_mapping();
        state
    }

    /// 哪些类别非空——用来一眼看出一个文件有没有落进唯一的一类。
    fn non_empty_groups(changes: &Changes) -> Vec<&'static str> {
        let mut labels = Vec::new();
        for (label, files) in [
            ("add", &changes.add),
            ("edit", &changes.edit),
            ("reopen_edit", &changes.reopen_edit),
            ("delete", &changes.delete),
            ("reopen_delete", &changes.reopen_delete),
            ("revert_add", &changes.revert_add),
            ("revert_edit", &changes.revert_edit),
            ("revert_delete", &changes.revert_delete),
        ] {
            if !files.is_empty() {
                labels.push(label);
            }
        }
        labels
    }

    /// 三个待摘要列表的长度：edit、revert_edit、revert_delete。
    fn pending_digests(analysis: &Analysis) -> [usize; 3] {
        [
            analysis.check_edit.len(),
            analysis.check_revert_edit.len(),
            analysis.check_revert_delete_or_reopen_edit.len(),
        ]
    }

    // ---- phase one：拿 depot 记录找工作区里缺了什么 ----

    #[test]
    fn a_synced_file_missing_from_the_workspace_is_classified_for_delete() {
        let depot = depot_state(vec![synced_record(PATH)]);

        let workspace = workspace_state(&[]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert_eq!(non_empty_groups(&analysis.changes), ["delete"]);
        assert_eq!(analysis.changes.delete, vec![PATH.to_owned()]);
    }

    #[test]
    fn an_open_edit_of_a_missing_file_reopens_as_delete() {
        let depot = depot_state(vec![DepotFileRecord {
            action: Some(FileAction::Edit),
            ..synced_record(PATH)
        }]);

        let workspace = workspace_state(&[]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert_eq!(non_empty_groups(&analysis.changes), ["reopen_delete"]);
        assert_eq!(analysis.changes.reopen_delete, vec![PATH.to_owned()]);
    }

    /// 忽略规则不该让一个已经打开的 edit 翻成删除，否则加一条 .p4ignore 就会
    /// 把待提交的改动变成待提交的删除。
    #[test]
    fn an_ignored_file_is_not_treated_as_missing() {
        let depot = depot_state(vec![DepotFileRecord {
            action: Some(FileAction::Edit),
            ..synced_record(PATH)
        }]);

        let workspace = workspace_state(&[(PATH, SIZE, true)]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert!(non_empty_groups(&analysis.changes).is_empty());
        assert_eq!(pending_digests(&analysis), [0, 0, 0]);
    }

    #[test]
    fn an_open_add_of_a_missing_file_is_reverted() {
        let depot = depot_state(vec![open_add_record(PATH)]);

        let workspace = workspace_state(&[]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert_eq!(non_empty_groups(&analysis.changes), ["revert_add"]);
        assert_eq!(analysis.changes.revert_add, vec![PATH.to_owned()]);
    }

    #[test]
    fn an_open_add_of_a_present_unfiltered_file_is_left_alone() {
        let depot = depot_state(vec![open_add_record(PATH)]);

        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert!(non_empty_groups(&analysis.changes).is_empty());
        assert_eq!(pending_digests(&analysis), [0, 0, 0]);
    }

    /// 被忽略等同于「本地没有」：pending add 已经没有意义。
    #[test]
    fn an_open_add_of_an_ignored_file_is_reverted() {
        let depot = depot_state(vec![open_add_record(PATH)]);

        let workspace = workspace_state(&[(PATH, SIZE, true)]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert_eq!(non_empty_groups(&analysis.changes), ["revert_add"]);
        assert_eq!(analysis.changes.revert_add, vec![PATH.to_owned()]);
    }

    #[test]
    fn an_open_delete_of_a_present_file_needs_a_digest_check() {
        let depot = depot_state(vec![DepotFileRecord {
            action: Some(FileAction::Delete),
            ..synced_record(PATH)
        }]);

        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert!(non_empty_groups(&analysis.changes).is_empty());
        assert_eq!(pending_digests(&analysis), [0, 0, 1]);
        assert_eq!(
            analysis.check_revert_delete_or_reopen_edit[0].1,
            DigestType::Text
        );
    }

    /// 二进制文件大小不同就已经能判定内容变了，不必再算摘要。
    #[test]
    fn an_open_delete_of_a_binary_file_with_a_different_size_reopens_for_edit() {
        let depot = depot_state(vec![DepotFileRecord {
            action: Some(FileAction::Delete),
            head_type: Some(FileType::Binary),
            ..synced_record(PATH)
        }]);

        let workspace = workspace_state(&[(PATH, SIZE + 1, false)]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert_eq!(non_empty_groups(&analysis.changes), ["reopen_edit"]);
        assert_eq!(analysis.changes.reopen_edit, vec![PATH.to_owned()]);
        assert_eq!(pending_digests(&analysis), [0, 0, 0]);
    }

    #[test]
    fn an_open_integrate_is_left_alone() {
        let depot = depot_state(vec![DepotFileRecord {
            action: Some(FileAction::Integrate),
            ..synced_record(PATH)
        }]);

        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert!(non_empty_groups(&analysis.changes).is_empty());
        assert_eq!(pending_digests(&analysis), [0, 0, 0]);
    }

    // ---- phase two：拿工作区文件找 depot 里缺了什么 ----

    #[test]
    fn a_present_file_with_a_matching_size_needs_a_digest_check_before_editing() {
        let depot = depot_state(vec![synced_record(PATH)]);

        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert!(non_empty_groups(&analysis.changes).is_empty());
        assert_eq!(pending_digests(&analysis), [1, 0, 0]);
        assert_eq!(analysis.check_edit[0].1, DigestType::Text);
    }

    #[test]
    fn a_binary_file_with_a_different_size_is_edited_without_a_digest() {
        let depot = depot_state(vec![DepotFileRecord {
            head_type: Some(FileType::Binary),
            ..synced_record(PATH)
        }]);

        let workspace = workspace_state(&[(PATH, SIZE + 1, false)]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert_eq!(non_empty_groups(&analysis.changes), ["edit"]);
        assert_eq!(analysis.changes.edit, vec![PATH.to_owned()]);
        assert_eq!(pending_digests(&analysis), [0, 0, 0]);
    }

    /// 文本会被规范化，磁盘上的字节数和 depot 记的对不上不代表内容变了。
    #[test]
    fn a_text_file_with_a_different_size_still_needs_a_digest_check() {
        let depot = depot_state(vec![synced_record(PATH)]);

        let workspace = workspace_state(&[(PATH, SIZE + 1, false)]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert!(non_empty_groups(&analysis.changes).is_empty());
        assert_eq!(pending_digests(&analysis), [1, 0, 0]);
    }

    #[test]
    fn an_open_edit_of_a_present_file_is_left_for_the_digest_phase() {
        let depot = depot_state(vec![DepotFileRecord {
            action: Some(FileAction::Edit),
            ..synced_record(PATH)
        }]);

        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert!(non_empty_groups(&analysis.changes).is_empty());
        assert_eq!(pending_digests(&analysis), [0, 1, 0]);
    }

    #[test]
    fn a_local_file_absent_from_the_depot_is_classified_for_add() {
        let depot = depot_state(Vec::new());

        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert_eq!(non_empty_groups(&analysis.changes), ["add"]);
        assert_eq!(analysis.changes.add, vec![PATH.to_owned()]);
    }

    /// depot 里有、但这个客户端从没同步过：本地那个只是重名，不能当改动用。
    /// `p4 edit` 对这类文件会直接报 "file(s) not on client"。
    #[test]
    fn a_local_file_sharing_a_name_with_an_unsynced_depot_file_is_reported_not_edited() {
        let depot = depot_state(vec![DepotFileRecord {
            head_action: Some(FileAction::Add),
            have_rev: None,
            ..synced_record(PATH)
        }]);

        let workspace = workspace_state(&[(PATH, SIZE + 1, false)]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert_eq!(analysis.unsynced_files, vec![PATH.to_owned()]);
        assert!(non_empty_groups(&analysis.changes).is_empty());
        assert_eq!(pending_digests(&analysis), [0, 0, 0]);
    }

    #[test]
    fn an_apple_or_resource_file_is_reported_as_unsupported() {
        let depot = depot_state(vec![DepotFileRecord {
            head_type: Some(FileType::Apple),
            ..synced_record(PATH)
        }]);

        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert_eq!(analysis.unsupported_files.len(), 1);
        assert_eq!(analysis.unsupported_files[0].depot_file, "//depot/a.txt");
        assert!(non_empty_groups(&analysis.changes).is_empty());
    }

    // ---- 删除版本与在库版本的交叉 ----

    /// head 已经是删除版本，本地文件又被 open for add：两边都已经表过态，别再插一脚。
    #[test]
    fn an_open_add_on_a_deleted_head_is_left_alone() {
        let depot = depot_state(vec![DepotFileRecord {
            head_action: Some(FileAction::Delete),
            action: Some(FileAction::Add),
            ..synced_record(PATH)
        }]);

        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert!(non_empty_groups(&analysis.changes).is_empty());
        assert_eq!(pending_digests(&analysis), [0, 0, 0]);
    }

    /// head 是删除版本、本地又冒出了同名文件：这是重新创建，标为新增。
    #[test]
    fn a_recreated_file_whose_head_is_deleted_is_classified_for_add() {
        let depot = depot_state(vec![DepotFileRecord {
            head_action: Some(FileAction::Delete),
            action: None,
            ..synced_record(PATH)
        }]);

        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert_eq!(non_empty_groups(&analysis.changes), ["add"]);
        assert_eq!(analysis.changes.add, vec![PATH.to_owned()]);
    }

    // ---- 未建模的组合必须响亮失败 ----

    /// `(守卫编号, depot 记录, 工作区文件, 错误里必须说清的记录状态)`。
    type UnhandledCase = (
        &'static str,
        DepotFileRecord,
        &'static [(&'static str, u64, bool)],
        &'static str,
    );

    /// 遇到没建模的记录形状要报错，而不是猜一个分类——猜错会直接改到用户的
    /// changelist。每条守卫的报错都要带上：编号（把用例与源码里的守卫一一对上）、
    /// 规则描述，以及判断所依据的记录状态（action / head / have；缺字段的规则还要
    /// 列出缺了什么）——光有一个文件路径，日志里看不出为什么失败。
    ///
    /// 编号对应源码里 `guard_error` 的 `rule N`。7 号（phase two 里 action 是
    /// add/branch 之类）测不到：phase one 遍历全部 depot 记录，同一条记录会先在
    /// 那边以 2 号失败。
    #[test]
    fn unhandled_record_shapes_fail_loudly_instead_of_being_misclassified() {
        let cases: [UnhandledCase; 5] = [
            (
                "1",
                DepotFileRecord {
                    action: Some(FileAction::Delete),
                    head_type: None,
                    ..synced_record(PATH)
                },
                &[(PATH, SIZE, false)],
                // 记录里 file_size 还在，缺的只有 head_type。
                "missing head_type",
            ),
            (
                "2",
                DepotFileRecord {
                    action: Some(FileAction::Add),
                    ..synced_record(PATH)
                },
                &[],
                "action=Some(Add)",
            ),
            (
                "3",
                DepotFileRecord {
                    action: Some(FileAction::Edit),
                    ..open_add_record(PATH)
                },
                &[],
                "head_action=None",
            ),
            (
                "5",
                DepotFileRecord {
                    head_action: Some(FileAction::Delete),
                    action: Some(FileAction::Edit),
                    ..synced_record(PATH)
                },
                &[(PATH, SIZE, false)],
                "head_action=Some(Delete)",
            ),
            (
                "6",
                DepotFileRecord {
                    action: Some(FileAction::Edit),
                    head_type: None,
                    ..synced_record(PATH)
                },
                &[(PATH, SIZE, false)],
                "missing head_type",
            ),
        ];

        for (number, record, workspace_entries, shape) in cases {
            let depot = depot_state(vec![record]);
            let workspace = workspace_state(workspace_entries);
            let error = analyze(&depot, &workspace, false)
                .err()
                .unwrap_or_else(|| panic!("case {number} must fail"))
                .to_string();

            // 编号仍在：它把用例与源码里的守卫一一对上。
            assert!(
                error.contains(&format!("Cannot handle \"{PATH}\" (rule {number}:")),
                "case {number}: {error}"
            );
            // 光有编号没用：记录长什么样、这条规则缺了什么，都要写出来。
            assert!(error.contains(shape), "case {number}: {error}");
            assert!(
                error.contains("head_rev=") && error.contains("have_rev="),
                "case {number}: {error}"
            );
        }
    }

    /// 归档版本的内容已经移出 depot，没有摘要可比。跳过并汇报，而不是中止整轮——
    /// 否则一个归档文件就能让整个工作区没法 reconcile。
    #[test]
    fn an_archived_head_is_skipped_and_reported() {
        let depot = depot_state(vec![DepotFileRecord {
            head_action: Some(FileAction::Archive),
            ..synced_record(PATH)
        }]);

        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        assert_eq!(analysis.archived_files, vec![PATH.to_owned()]);
        assert!(non_empty_groups(&analysis.changes).is_empty());
        assert_eq!(pending_digests(&analysis), [0, 0, 0]);
    }

    /// 类型名认不出来时 head_type 是空的。这类记录不能被当成「字段不完整」而在
    /// phase one 的守卫上报错，也不该在 phase two 再被收一次，而应转交 p4 reconcile。
    #[test]
    fn an_unknown_file_type_is_handed_to_p4_reconcile() {
        let depot = depot_state(vec![DepotFileRecord {
            head_type: None,
            unsupported_type: Some("tempobj".to_owned()),
            ..synced_record(PATH)
        }]);

        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze(&depot, &workspace, false).unwrap();

        // 汇报一次，不是两次。
        assert_eq!(analysis.unsupported_files.len(), 1);
        assert_eq!(analysis.unsupported_files[0].depot_file, "//depot/a.txt");
        assert!(non_empty_groups(&analysis.changes).is_empty());
        assert_eq!(pending_digests(&analysis), [0, 0, 0]);
    }

    // ---- analyze_at_target：按目标版本分类 ----

    /// 目标版本快照里的一条记录。
    fn target_entry(depot_file: &str, rev: u32, action: FileAction) -> (String, TargetRecord) {
        (
            depot_file.to_ascii_lowercase(),
            TargetRecord { rev, action },
        )
    }

    /// 目标就是 head 时，快照与 head 记录同源。
    ///
    /// 刻意转调生产实现而不是在这里复刻一份：复刻的话，`snapshot_target` 取错了字段
    /// （比如把 `have_rev` 当成 `rev`）这套单测照样全绿——那正是「用实现的重述验证实现」。
    fn head_target(records: &[DepotFileRecord]) -> TargetMap {
        crate::p4::fstat::snapshot_target(records)
    }

    /// sync 分类下哪些组非空——用来一眼看出一个文件有没有落进唯一的一组。
    fn non_empty_sync_groups(analysis: &SyncAnalysis) -> Vec<&'static str> {
        let mut labels = Vec::new();
        for (label, len) in [
            ("update", analysis.update.len()),
            ("revert", analysis.revert.len()),
            ("check", analysis.check.len()),
            ("restore", analysis.restore.len()),
            ("delete", analysis.delete.len()),
            ("unsupported", analysis.unsupported.len()),
            ("archived", analysis.archived.len()),
        ] {
            if len > 0 {
                labels.push(label);
            }
        }
        labels
    }

    /// **「目标版本不需要摘要」的单测化身**：have 与目标不一致的文件直接进 `update`，
    /// 绝不能进 `check`——真进去的话，`check` 组会去拿 have 版本的摘要比对一个
    /// 将来才要下载的目标版本，得出一个没有意义的结论。
    #[test]
    fn a_file_behind_the_target_is_updated_without_a_digest() {
        let depot = depot_state(vec![synced_record(PATH)]);
        let target: TargetMap = [target_entry("//depot/a.txt", 2, FileAction::Edit)]
            .into_iter()
            .collect();

        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze_at_target(&depot, &workspace, &target).unwrap();

        assert_eq!(non_empty_sync_groups(&analysis), ["update"]);
        assert_eq!(analysis.update[0].target_rev, 2);
    }

    /// 从没同步过的文件同理：没有 have 摘要可比，只有「拉」这一个选项。
    #[test]
    fn a_file_never_synced_is_updated_without_a_digest() {
        let depot = depot_state(vec![DepotFileRecord {
            have_rev: None,
            ..synced_record(PATH)
        }]);
        let target: TargetMap = [target_entry("//depot/a.txt", 1, FileAction::Edit)]
            .into_iter()
            .collect();

        // 本地已经有一份同名文件：会被 depot 内容整份覆盖（已拍板照 -f 的行为）。
        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze_at_target(&depot, &workspace, &target).unwrap();

        assert_eq!(non_empty_sync_groups(&analysis), ["update"]);
    }

    /// have 与目标一致、本地在位：只有这一种情况需要对摘要。
    #[test]
    fn a_file_at_the_target_is_checked_with_a_digest() {
        let depot = depot_state(vec![synced_record(PATH)]);
        let target = head_target(&depot.file_records);

        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze_at_target(&depot, &workspace, &target).unwrap();

        assert_eq!(non_empty_sync_groups(&analysis), ["check"]);
        assert_eq!(analysis.check[0].digest_type, DigestType::Text);
    }

    /// 本地缺失压过 have 状态：不论 have 停在哪一版，本地没有就是「写回」。
    ///
    /// 判据按**本地状态**分而不是 have 状态分的理由见 `analyze_at_target` 的文档；
    /// 原生 `p4 sync -f` 那边对应的是 `added as` 而不是 `updating`。
    #[test]
    fn a_missing_local_file_is_restored_even_when_have_is_behind() {
        let depot = depot_state(vec![synced_record(PATH)]);
        let target: TargetMap = [target_entry("//depot/a.txt", 2, FileAction::Edit)]
            .into_iter()
            .collect();

        let workspace = workspace_state(&[]);
        let analysis = analyze_at_target(&depot, &workspace, &target).unwrap();

        assert_eq!(non_empty_sync_groups(&analysis), ["restore"]);
    }

    /// 二进制长度不符就已经能断定内容变了：不必再读一遍内容。
    #[test]
    fn a_binary_of_the_wrong_size_is_reverted_without_a_digest() {
        let depot = depot_state(vec![DepotFileRecord {
            head_type: Some(FileType::Binary),
            ..synced_record(PATH)
        }]);
        let target = head_target(&depot.file_records);

        let workspace = workspace_state(&[(PATH, SIZE - 1, false)]);
        let analysis = analyze_at_target(&depot, &workspace, &target).unwrap();

        assert_eq!(non_empty_sync_groups(&analysis), ["revert"]);
    }

    /// 文本不能这么判：换行归一化会改变字节数，长度不同不代表内容不同。
    #[test]
    fn a_text_of_the_wrong_size_is_still_checked() {
        let depot = depot_state(vec![synced_record(PATH)]);
        let target = head_target(&depot.file_records);

        let workspace = workspace_state(&[(PATH, SIZE - 1, false)]);
        let analysis = analyze_at_target(&depot, &workspace, &target).unwrap();

        assert_eq!(non_empty_sync_groups(&analysis), ["check"]);
    }

    #[test]
    fn a_file_missing_locally_is_restored() {
        let depot = depot_state(vec![synced_record(PATH)]);
        let target = head_target(&depot.file_records);

        let workspace = workspace_state(&[]);
        let analysis = analyze_at_target(&depot, &workspace, &target).unwrap();

        assert_eq!(non_empty_sync_groups(&analysis), ["restore"]);
    }

    /// 被忽略规则覆盖的本地文件视同不存在：实测 p4 自己会照常写回这类文件
    /// （`.p4ignore` 拦不住 `p4 sync -f`），交给它做，工具不单方面覆盖。
    #[test]
    fn an_ignored_local_copy_is_restored_not_checked() {
        let depot = depot_state(vec![synced_record(PATH)]);
        let target = head_target(&depot.file_records);

        let workspace = workspace_state(&[(PATH, SIZE, true)]);
        let analysis = analyze_at_target(&depot, &workspace, &target).unwrap();

        assert_eq!(non_empty_sync_groups(&analysis), ["restore"]);
    }

    /// 目标时刻该路径已是删除版本：本地那份要删掉。
    #[test]
    fn a_file_deleted_at_the_target_is_deleted_locally() {
        let depot = depot_state(vec![synced_record(PATH)]);
        let target: TargetMap = [target_entry("//depot/a.txt", 1, FileAction::Delete)]
            .into_iter()
            .collect();

        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze_at_target(&depot, &workspace, &target).unwrap();

        assert_eq!(non_empty_sync_groups(&analysis), ["delete"]);
    }

    /// 同一个「被忽略」标记，在删除组里的方向与 Restore 组**相反**：那里交给 p4 覆盖写回，
    /// 这里则放过它、不删。
    ///
    /// 这个不对称是刻意的——本地构建产物是用户明确表达过「别管它」的东西，而删除不可逆。
    /// 口径写反就是删用户的 `build/`，所以它必须有一条用例守着。
    #[test]
    fn an_ignored_local_file_is_not_deleted_at_the_target() {
        let depot = depot_state(vec![synced_record(PATH)]);
        let target: TargetMap = [target_entry("//depot/a.txt", 1, FileAction::Delete)]
            .into_iter()
            .collect();

        let workspace = workspace_state(&[(PATH, SIZE, true)]);
        let analysis = analyze_at_target(&depot, &workspace, &target).unwrap();

        assert!(analysis.delete.is_empty(), "被忽略的文件不该进删除组");
        assert!(non_empty_sync_groups(&analysis).is_empty());
    }

    /// `--to <CL>` 下「目标时刻还没创建」的形态：@CL 的结果里压根没有这条记录，
    /// 但 head 记录里有、本地也有。实测 p4 自己也是删（`deleted as`）。
    #[test]
    fn a_path_created_after_the_target_changelist_is_deleted_locally() {
        let depot = depot_state(vec![synced_record(PATH)]);
        let target: TargetMap = TargetMap::new();

        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze_at_target(&depot, &workspace, &target).unwrap();

        assert_eq!(non_empty_sync_groups(&analysis), ["delete"]);
    }

    /// 目标时刻不在库、本地也没有：没有动作可言。
    #[test]
    fn a_deleted_target_without_a_local_copy_has_nothing_to_do() {
        let depot = depot_state(vec![synced_record(PATH)]);
        let target: TargetMap = [target_entry("//depot/a.txt", 1, FileAction::Delete)]
            .into_iter()
            .collect();

        let workspace = workspace_state(&[]);
        let analysis = analyze_at_target(&depot, &workspace, &target).unwrap();

        assert!(non_empty_sync_groups(&analysis).is_empty());
    }

    /// 已打开的文件一律不碰（`p4 sync -f` 的官方口径同样如此），而且不汇报。
    #[test]
    fn an_opened_file_is_left_alone() {
        let depot = depot_state(vec![
            DepotFileRecord {
                action: Some(FileAction::Edit),
                ..synced_record(PATH)
            },
            open_add_record(r"C:\ws\added.txt"),
        ]);
        let target = head_target(&depot.file_records);

        // 本地内容与目标不同、也被改过，但对已打开的文件这些都不构成理由。
        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze_at_target(&depot, &workspace, &target).unwrap();

        assert!(non_empty_sync_groups(&analysis).is_empty());
    }

    /// 已打开 × 目标时刻不在库：仍然不碰。
    ///
    /// 这是「已打开优先」最要命的一种组合——用户 `p4 edit` 一个在 head 处已删除的文件
    /// （等于「我要把它加回来」），本地有他没提交的工作。S1 的判断若被挪到目标判断之后，
    /// 这份工作会被当成「目标时刻不在库」删掉。
    #[test]
    fn an_opened_file_is_left_alone_even_when_the_target_has_none() {
        let depot = depot_state(vec![DepotFileRecord {
            action: Some(FileAction::Edit),
            ..synced_record(PATH)
        }]);
        let target: TargetMap = [target_entry("//depot/a.txt", 2, FileAction::Delete)]
            .into_iter()
            .collect();

        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze_at_target(&depot, &workspace, &target).unwrap();

        assert!(analysis.delete.is_empty(), "已打开的文件不该进删除组");
        assert!(non_empty_sync_groups(&analysis).is_empty());
    }

    /// 未跟踪的本地文件（depot 里压根没有这个路径）不动作——这与 clean 是最要紧的分水岭。
    #[test]
    fn an_untracked_local_file_is_left_alone() {
        let depot = depot_state(vec![synced_record(PATH)]);
        let target = head_target(&depot.file_records);

        let workspace =
            workspace_state(&[(PATH, SIZE, false), (r"C:\ws\untracked.txt", 10, false)]);
        let analysis = analyze_at_target(&depot, &workspace, &target).unwrap();

        assert_eq!(non_empty_sync_groups(&analysis), ["check"]);
        assert!(
            analysis
                .delete
                .iter()
                .all(|entry| !entry.file.path.ends_with("untracked.txt")),
            "未跟踪的文件绝不能进删除组"
        );
    }

    #[test]
    fn an_archived_target_is_reported_not_synced() {
        let depot = depot_state(vec![synced_record(PATH)]);
        let target: TargetMap = [target_entry("//depot/a.txt", 1, FileAction::Archive)]
            .into_iter()
            .collect();

        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze_at_target(&depot, &workspace, &target).unwrap();

        assert_eq!(non_empty_sync_groups(&analysis), ["archived"]);
    }

    /// 类型名认不出来 → 转交；Apple / Resource 算不出摘要 → 同样转交。
    #[test]
    fn types_without_a_digest_are_handed_to_p4_sync() {
        let unknown = DepotFileRecord {
            head_type: None,
            unsupported_type: Some("tempobj".to_owned()),
            ..synced_record(PATH)
        };
        let apple = DepotFileRecord {
            head_type: Some(FileType::Apple),
            ..synced_record(r"C:\ws\b.txt")
        };
        let depot = depot_state(vec![unknown, apple]);
        let target = head_target(&depot.file_records);

        let workspace = workspace_state(&[(PATH, SIZE, false), (r"C:\ws\b.txt", SIZE, false)]);
        let analysis = analyze_at_target(&depot, &workspace, &target).unwrap();

        assert_eq!(non_empty_sync_groups(&analysis), ["unsupported"]);
        // 汇报一次，不是两次：每条记录只落一类。
        assert_eq!(analysis.unsupported.len(), 2);
    }

    /// 大小写冲突下同一个 client 路径对上两条记录，只处理 build_mapping 选出的胜者，
    /// 否则同一个本地文件会被两条 depot 路径各下发一次。
    #[test]
    fn a_case_collision_only_processes_the_winning_record() {
        let winner = DepotFileRecord {
            depot_file: "//depot/Snow.uasset".to_owned(),
            depot_file_lower: "//depot/snow.uasset".to_owned(),
            head_rev: Some(2),
            ..synced_record(PATH)
        };
        let loser = DepotFileRecord {
            depot_file: "//depot/snow_normal.uasset".to_owned(),
            depot_file_lower: "//depot/snow_normal.uasset".to_owned(),
            ..synced_record(PATH)
        };
        let depot = depot_state(vec![winner, loser]);
        let target = head_target(&depot.file_records);

        let workspace = workspace_state(&[(PATH, SIZE, false)]);
        let analysis = analyze_at_target(&depot, &workspace, &target).unwrap();

        assert_eq!(non_empty_sync_groups(&analysis), ["update"]);
        assert_eq!(analysis.update.len(), 1);
    }

    /// 本地在位、have 与目标一致，但记录缺 head_type：摘要算不了，必须响亮失败。
    /// 报错也要带上记录形状——旧消息只有一个文件路径。
    #[test]
    fn a_sync_record_without_a_type_fails_loudly_with_its_shape() {
        let depot = depot_state(vec![DepotFileRecord {
            head_type: None,
            ..synced_record(PATH)
        }]);
        let target = head_target(&depot.file_records);
        let workspace = workspace_state(&[(PATH, SIZE, false)]);

        let error = analyze_at_target(&depot, &workspace, &target)
            .err()
            .expect("a record without head_type must fail")
            .to_string();

        assert!(
            error.contains(&format!("Cannot handle \"{PATH}\" (sync:")),
            "{error}"
        );
        assert!(error.contains("missing head_type"), "{error}");
        assert!(error.contains("have_rev=Some(1)"), "{error}");
    }
}
