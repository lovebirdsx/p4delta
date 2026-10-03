//! sync 模式：把工作区拉到目标 depot 版本，只传真正需要传的文件。
//!
//! 与 clean 的关系：方向相同（都是拿 depot 修工作区），**目标不同**——clean 的目标是
//! have（把本地改动还原掉），sync 的目标是指定的 depot 版本（默认 head，可 `--to <CL>`）。
//! 还有一条更重要的分水岭：sync **不删** depot 里没有的本地文件，那些是用户自己的东西。
//!
//! 与原生 `p4 sync -f` 的关系：语义对齐（同样的目标版本、同样不碰已打开的文件、同样
//! 会覆盖未打开的本地改动），但只下发真正需要传的文件，而不是把整个工作区重传一遍。
//!
//! 判定与分类在 [`analyze_at_target`]，这里只管「报告什么」与「下发什么」。

use std::collections::HashMap;
use std::time::Instant;

use anyhow::{Result, anyhow, bail};

use super::analyze::{SyncAnalysis, SyncSource, analyze_at_target};
use super::changes::{render_title, report_group};
use super::{HashStats, delete_workspace_files};
use crate::cache::{CacheWriter, save_cache};
use crate::cli::Options;
use crate::digest::{CachePolicy, is_unchanged_since_sync, parallel_compute_digests};
use crate::model::{DepotState, HaveRecord, TargetMap, WorkspaceCache, WorkspaceState};
use crate::p4::process::{FailureMode, run_p4_command_batched};

/// 把文件拉到目标版本时下发的 p4 命令。
///
/// `-f` 不能省：Revert / Restore 两组的文件都还在 have list 上、时间戳也可能被 p4 认为
/// 新鲜，不强制就会跳过——而这两组恰恰是「本地内容不对」的那批。
/// 不带 `-K`：`p4 sync` 默认展开 ktext 关键字，`-K` 才抑制，这里对上的是默认行为。
const SYNC_ARGS: &[&str] = &["sync", "-f"];

/// 要拉到目标版本的文件。
///
/// Update / Revert / Restore 三组下发的命令是同一个形状
/// （`p4 sync -f //depot/path#<rev>`），差别只在报告里怎么称呼，所以共用一个结构。
#[derive(Debug, PartialEq)]
pub(crate) struct SyncFile {
    /// workspace 语法，`-l` 清单显示用。
    client_file: String,

    /// depot 语法，拼 p4 文件规格用。
    depot_file: String,

    /// 目标版本，拼进规格里钉死。
    target_rev: u32,
}

/// 要从工作区删掉的文件。
///
/// 两个路径都要带着，删这一组动作需要两条腿：
///
/// - **清 have 记录**：只能交给 p4（`#none` 的独家本事）。留下一条指向已消失文件的 have
///   记录，之后普通 `p4 sync` 会认为「已是最新」而永不写回——工作区从此和 p4 的看法对不上。
/// - **删文件**：p4 对有 have 记录的文件会顺带删掉；但对「本 client 从没同步过」的路径它
///   只回一句 `file(s) up-to-date`、本地文件原样留着——那种只能自己动手。
///
/// **先清记录、后删文件，顺序不能反。** `#none` 对「本地文件已不存在」的路径会做什么没有
/// 实测过，而对「本地存在 + 有 have 记录」的行为是实测过的（stdout `deleted as`、stderr 空、
/// 退出码 0）。让 p4 永远面对后者；删文件那一步对 p4 已经删掉的路径是幂等兜底
/// （[`remove_workspace_entry`] 对 `NotFound` 返回 `Ok`）。
///
/// [`remove_workspace_entry`]: crate::reconcile::remove_workspace_entry
#[derive(Debug, PartialEq)]
pub(crate) struct DeleteFile {
    /// workspace 语法，删文件与 `-l` 清单显示用。
    client_file: String,

    /// depot 语法，拼 `#none` 规格用。
    depot_file: String,

    /// 本 client 有没有这个文件的 have 记录。判据得跟着它走，见 [`delete_specs`]。
    has_have_record: bool,
}

/// 一类变更要执行的动作。
#[derive(Debug, Clone, Copy, PartialEq)]
enum SyncAction {
    /// `p4 sync -f //depot/f#<rev>`，把文件拉到目标版本。
    SyncToTarget,

    /// 删掉工作区里的文件（目标时刻该路径不在库）。
    DeleteFromDisk,
}

/// 一类变更在报告与执行上的全部差异。
struct SyncGroupSpec {
    /// `-l` 清单里每行的前缀，例如 `Update` / `Revert`。
    label: &'static str,

    /// 标题模板，`{}` 由文件数量填充。前导空格是输出契约的一部分。
    title: &'static str,

    action: SyncAction,
}

/// 四类变更的处理方式。顺序即输出顺序，必须与 [SyncChanges::groups] 一一对应。
///
/// Update 与 Revert 分开是刻意的：前者是「本地落后了，正常拉取」，后者是「你在本地改了
/// 一个没打开编辑的文件，改动会被丢弃」——风险等级不同，混在一组就是在误导
/// （立场同 `clean.rs` 里那段「标题与动作必须一致」的注释）。
const SYNC_GROUPS: [SyncGroupSpec; 4] = [
    SyncGroupSpec {
        label: "Update",
        title: "      Updating {} files in workspace to the target depot revision.",
        action: SyncAction::SyncToTarget,
    },
    SyncGroupSpec {
        label: "Revert",
        title: "      Reverting {} files in workspace, changed from the target revision, but not checked out for edit.",
        action: SyncAction::SyncToTarget,
    },
    SyncGroupSpec {
        label: "Restore",
        title: "      Restoring {} files not in workspace, at the target depot revision.",
        action: SyncAction::SyncToTarget,
    },
    // 措辞不与 clean 的 Delete 共用「deleted at have revision」那套：sync 删的是「目标时刻
    // 该路径不在库」的文件，其中既可能是目标处已是删除版本，也可能是（`--to <CL>` 下）
    // 目标时刻它还没进 depot。「not present」把两种都盖住了。
    SyncGroupSpec {
        label: "Delete",
        title: "      Deleting {} files in workspace, not present in the depot at the target revision.",
        action: SyncAction::DeleteFromDisk,
    },
];

/// 一类变更名下的文件。两类的元素类型不同（要同步的规格 vs 要删的路径），
/// 用一个枚举把报告需要的公共部分取出来。
enum SyncGroupFiles<'a> {
    Sync(&'a [SyncFile]),
    Delete(&'a [DeleteFile]),
}

impl<'a> SyncGroupFiles<'a> {
    fn len(&self) -> usize {
        match self {
            SyncGroupFiles::Sync(files) => files.len(),
            SyncGroupFiles::Delete(files) => files.len(),
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `-l` 清单里的名字：一律是 client 路径，与 `p4 sync -l` 的 local syntax 口径一致。
    ///
    /// 返回的借用直接指向 `'a`（`SyncChanges`）而不是 `&self`：调用方常在遍历 `groups()`
    /// 的闭包里用这个名字，那里拿到的 `SyncGroupFiles` 是临时值。
    fn client_files(&self) -> Vec<&'a str> {
        match *self {
            SyncGroupFiles::Sync(files) => {
                files.iter().map(|file| file.client_file.as_str()).collect()
            }
            SyncGroupFiles::Delete(files) => {
                files.iter().map(|file| file.client_file.as_str()).collect()
            }
        }
    }
}

/// sync 要做的四类动作。
#[derive(Debug, Default)]
pub(crate) struct SyncChanges {
    /// 本地有一份、但不是目标版本：拉到目标版本覆盖它。
    update: Vec<SyncFile>,

    /// 目标版本没变、本地内容却不是它了：还原回去。
    revert: Vec<SyncFile>,

    /// 本地没有（或被忽略规则盖着）：写回来。
    restore: Vec<SyncFile>,

    /// 目标时刻该路径不在库、本地却有：删掉。
    delete: Vec<DeleteFile>,
}

impl SyncChanges {
    /// 从分类结果与摘要结果投影出要下发的四类动作。
    ///
    /// `drifted` 是摘要判定为「不是目标版本」的那批，由调用方在算完摘要后传入：
    /// 摘要是 [`SyncAnalysis::check`] 的后续，这里只负责拼装。
    ///
    /// 与 `CleanChanges::project` 不同，这里不会失败——需要的 depot 路径与目标修订在
    /// 分类时就已经跟着记录一起带出来了（[`SyncSource`]），没有「回头查不到」的余地。
    pub(crate) fn project(analysis: &SyncAnalysis<'_>, drifted: Vec<SyncSource<'_>>) -> Self {
        let mut revert = to_sync_files(&analysis.revert);
        revert.extend(to_sync_files(&drifted));

        SyncChanges {
            update: to_sync_files(&analysis.update),
            revert,
            restore: to_sync_files(&analysis.restore),
            delete: analysis
                .delete
                .iter()
                .map(|entry| DeleteFile {
                    client_file: entry.file.path.clone(),
                    depot_file: entry.record.depot_file.clone(),
                    has_have_record: entry.record.have_rev.is_some(),
                })
                .collect(),
        }
    }

    /// 按 [SYNC_GROUPS] 的顺序把每一类与它的文件配成对。
    fn groups(&self) -> impl Iterator<Item = (&'static SyncGroupSpec, SyncGroupFiles<'_>)> {
        SYNC_GROUPS.iter().zip([
            SyncGroupFiles::Sync(self.update.as_slice()),
            SyncGroupFiles::Sync(self.revert.as_slice()),
            SyncGroupFiles::Sync(self.restore.as_slice()),
            SyncGroupFiles::Delete(self.delete.as_slice()),
        ])
    }

    /// 待同步的文件总数。
    pub(crate) fn total(&self) -> usize {
        self.update.len() + self.revert.len() + self.restore.len() + self.delete.len()
    }
}

/// 把「记录 + 目标修订」翻成下发规格。
fn to_sync_files(sources: &[SyncSource<'_>]) -> Vec<SyncFile> {
    sources
        .iter()
        .map(|source| SyncFile {
            client_file: source.record.client_file.clone(),
            depot_file: source.record.depot_file.clone(),
            target_rev: source.target_rev,
        })
        .collect()
}

/// `p4 sync` 的文件参数：`//depot/path#<rev>`。
///
/// 显式钉住版本是全部要点：不带版本的 `-f` 会同步到 head，在 `--to <CL>` 下那正是要
/// 避免的——把比目标更新的版本拉下来。
fn sync_specs(files: &[SyncFile]) -> Vec<String> {
    files
        .iter()
        .map(|file| format!("{}#{}", file.depot_file, file.target_rev))
        .collect()
}

/// 从工作区删掉一批文件，并让 p4 清掉它们的 have 记录。
///
/// 两条腿**都要跑**，一条失败不该让另一条不跑：只删了文件却没清记录，留下的是一批指向
/// 已消失文件的 have 记录，而那个状态工具自己修不回来——下一轮这些文件已不在磁盘上，
/// `push_deleted` 要求本地存在才收进这一组。所以两条腿各自收集错误，都执行完再一起报。
async fn delete_from_workspace(
    options: &Options,
    work_dir: &str,
    files: &[DeleteFile],
) -> Result<()> {
    // 第一腿：清 have 记录（p4 顺带删掉它认得的那些文件）。规格为空说明这一批全是
    // 「从没同步过」的，p4 无事可做，不必起进程。
    let specs = delete_specs(files);
    let p4_error = if specs.is_empty() {
        None
    } else {
        // 判据用 ExitCodeOrStderr：发过去的都是**有** have 记录的路径，不会碰上
        // 「`... - file(s) up-to-date.`」那类良性 stderr，于是 protections 拒绝这种
        // 「退出码 0、错误只在 stderr」的真失败才暴露得出来（见 [`FailureMode`]）。
        run_p4_command_batched(
            options,
            work_dir,
            SYNC_ARGS,
            &specs,
            false,
            FailureMode::ExitCodeOrStderr,
        )
        .await
        .err()
    };

    // 第二腿：自己删。对 p4 刚删掉的那些是幂等兜底，对「从没同步过」的那批是唯一手段。
    let paths: Vec<String> = files.iter().map(|file| file.client_file.clone()).collect();
    let fs_error = delete_workspace_files(&paths).err();

    match (fs_error, p4_error) {
        (None, None) => Ok(()),
        (Some(error), None) | (None, Some(error)) => Err(error),
        // 两条腿都失败了：两条消息都要留着，它们指向不同的补救动作。
        (Some(fs_error), Some(p4_error)) => Err(anyhow!(
            "{fs_error}\n  and clearing the have records also failed: {p4_error}"
        )),
    }
}

/// 清 have 记录用的规格：`//depot/path#none`。
///
/// `#none` 在 p4 语法里就是「这个文件在工作区里不该有」：p4 会删掉本地文件，并把 have
/// 记录一并清掉。清记录是它相对 [`delete_workspace_files`] 的独家本事，见 [`DeleteFile`]。
///
/// **只给有 have 记录的文件拼**：对没有记录的路径，`#none` 是个 no-op，p4 只会往 stderr
/// 写一句 `file(s) up-to-date.`（退出码 0）。把它们混进来，那次调用的 stderr 就被这句良性
/// 提示占满，判据只能退到最弱的 `ExitCode`——而它看不见同一类 p4 行为里的真失败。
fn delete_specs(files: &[DeleteFile]) -> Vec<String> {
    files
        .iter()
        .filter(|file| file.has_have_record)
        .map(|file| format!("{}#none", file.depot_file))
        .collect()
}

/// 把一批文件拉到它们的目标版本。
///
/// 传 depot 路径而不是 client 路径：目标修订本来就长在 depot 记录上，而且同一个 client
/// 路径在大小写冲突下可能对应两条记录，depot 路径没有歧义。
async fn sync_to_target(options: &Options, work_dir: &str, files: &[SyncFile]) -> Result<()> {
    let specs = sync_specs(files);

    // 不带 changelist：sync 不产生 changelist，`p4 sync` 也没有意义。
    // 只在 -a 时被调用，所以永远是「真改状态」：sync 失败必须让整轮失败，
    // 否则工作区会停在一个既没拉全、也没人知道的状态上。
    // 判据同 [`crate::reconcile::changes::apply_changes`]：p4 的逐文件错误不改退出码。
    // 错误由 [`apply_sync`] 收集，等其余各类做完再一起报。
    run_p4_command_batched(
        options,
        work_dir,
        SYNC_ARGS,
        &specs,
        false,
        FailureMode::ExitCodeOrStderr,
    )
    .await?;

    Ok(())
}

/// 报告 sync 的四类变更，并在 `-a` 时执行它们。
pub(crate) async fn apply_sync(
    options: &Options,
    work_dir: &str,
    sync: &SyncChanges,
) -> Result<()> {
    if options.apply {
        println!("   Syncing the workspace to the target depot revision.");
    } else {
        println!("   Counting files to sync (dry run).")
    }
    // 预演也打这句（clean 只在 -a 时打）：sync 相对 clean 的卖点正是「不删未跟踪的文件」，
    // 而「会覆盖未打开文件的本地改动」是它的代价。授权之前就得看得见代价。
    println!(
        "      WARNING: this overwrites local changes to files that are not opened. It cannot be undone."
    );

    let start_time = Instant::now();
    let mut failures: Vec<String> = Vec::new();

    for (spec, files) in sync.groups() {
        if files.is_empty() {
            continue;
        }

        report_group(
            spec.label,
            &render_title(spec.title, files.len()),
            options.list,
            files.client_files(),
        );

        if !options.apply {
            continue;
        }

        // 一类失败不拦下其余的类：一个文件删不掉（编辑器占着、权限不对）或拉不下来，
        // 不该让另外几百个留在原地。做完能做的，最后一起报。
        let result = match (spec.action, &files) {
            (SyncAction::SyncToTarget, SyncGroupFiles::Sync(files)) => {
                sync_to_target(options, work_dir, files).await
            }
            (SyncAction::DeleteFromDisk, SyncGroupFiles::Delete(files)) => {
                delete_from_workspace(options, work_dir, files).await
            }
            // 表与字段的配对由 `every_group_is_paired_with_the_matching_action` 钉死，
            // 走不到这里；真走到了说明表被改坏了，不能静默按其中一边执行。
            (action, _) => bail!(
                "Mispaired sync group \"{}\" with action {action:?}",
                spec.label
            ),
        };

        if let Err(error) = result {
            failures.push(format!("{} ({} files): {error}", spec.label, files.len()));
        }
    }

    let total = sync.total();
    if options.apply {
        // 有失败就绝不打印「与目标一致」——那是这个工具唯一的安全承诺。
        if !failures.is_empty() {
            bail!(
                "Failed to sync {} change group(s):\n  {}",
                failures.len(),
                failures.join("\n  ")
            );
        }

        println!(
            "      Synced {total} files in {} seconds.",
            start_time.elapsed().as_secs_f32()
        );
        // 只有 --verify-all 才敢说这句话：默认档靠 mtime 捷径与摘要缓存跳过了一部分文件，
        // 那些文件的「没变」是推断出来的，没被验证过。承诺必须与档位匹配。
        if options.verify_all {
            println!("The synced files match the target depot revision.");
        }
    } else {
        println!(
            "      Counted {total} files to sync in {} seconds.",
            start_time.elapsed().as_secs_f32()
        );
        println!("Re-run with -a to sync the workspace.");
    }

    Ok(())
}

/// sync 一轮分析的产出：要下发的动作，加上两类不由分组报告承担的文件。
pub(crate) struct SyncPlan {
    pub(crate) changes: SyncChanges,

    /// 算不出摘要的文件（depot 语法），转交原生 `p4 sync` 处理。
    ///
    /// 交给调用方而不是在这里处理：那是一次真正的动作（预演时还要加 `-n`），
    /// 得和其余转交点一起走 `run_p4_command_batched` 那套失败判据。
    pub(crate) unsupported: Vec<String>,
}

/// sync 模式的分析管线：按目标版本分类 → 只对「目标版本没变」的文件算摘要 → 投影成动作。
pub(crate) async fn build_sync_changes(
    options: &Options,
    depot: &DepotState,
    workspace: &WorkspaceState,
    have_records: &HashMap<String, HaveRecord>,
    target: &TargetMap,
    cache: &mut WorkspaceCache,
    cache_writer: &mut Option<CacheWriter>,
) -> Result<SyncPlan> {
    println!("   Analyzing files against the target depot revision.");
    let start_time = Instant::now();
    let mut analysis = analyze_at_target(depot, workspace, target)?;
    println!(
        "      Analysis complete in {} seconds.",
        start_time.elapsed().as_secs_f32()
    );

    if !analysis.archived.is_empty() {
        println!(
            "Skipped {} archived file(s): their contents live in an archive depot, so there is nothing to compare.",
            analysis.archived.len()
        );
    }

    // 本地从没同步过、却已经有一份同名文件的那批：会被 depot 内容整份覆盖，而且与
    // 「落后了拉一把」不是一回事——没有旧版本可退回。它落在 Update 组里，光看组标题
    // 看不出来，单独说一句。`p4 sync -f` 的行为相同，区别只是它会逐条报 "added as"。
    let never_synced = analysis
        .update
        .iter()
        .filter(|source| {
            source.record.have_rev.is_none() && workspace.has_file(&source.record.client_file_lower)
        })
        .count();
    if never_synced > 0 {
        println!(
            "Found {never_synced} file(s) this client has never synced; their local copies will be overwritten."
        );
    }

    // 删除组里的同一类：动作上与「have 停在旧版、目标处已删除」一样是删，但删掉的是用户
    // 自己放进去的东西，而且 depot 里没有旧版本可退回。一样的风险，就得有一样的可见度。
    let never_synced_delete = analysis
        .delete
        .iter()
        .filter(|entry| entry.record.have_rev.is_none())
        .count();
    if never_synced_delete > 0 {
        println!(
            "Found {never_synced_delete} file(s) this client has never synced; their local copies will be deleted."
        );
    }

    // 只有「目标版本没变」的那批才需要摘要。其余三类（落后要拉、本地缺失要写回、目标
    // 时刻不在库要删）本就要动文件，算出来「内容不一样」也还是要动——这正是 sync 相对
    // clean 省下来的地方：不必把整个工作区读一遍。
    let mut drifted = Vec::new();
    let check_candidates = std::mem::take(&mut analysis.check);
    if !check_candidates.is_empty() {
        let total_candidates = check_candidates.len();

        let (skipped, needs_digest): (Vec<_>, Vec<_>) = if options.verify_all {
            // --verify-all：不信任时间戳捷径，目标版本没变的文件全部重算摘要。
            (Vec::new(), check_candidates)
        } else {
            check_candidates
                .into_iter()
                .partition(|check| is_unchanged_since_sync(check.file, have_records))
        };

        if !skipped.is_empty() {
            // 跳过的文件是候选的子集，所以除数非零；`checked_div` 只是不让它成为前提。
            let percentage = (skipped.len() * 100)
                .checked_div(total_candidates)
                .unwrap_or(0);
            println!(
                "   Timestamp optimization: Skipped {} of {} digest computations ({}%).",
                skipped.len(),
                total_candidates,
                percentage
            );
        }

        if !needs_digest.is_empty() {
            println!("   Checking digests for {} files.", needs_digest.len());

            let mut hashed = HashStats::new();

            let candidates = needs_digest
                .iter()
                .map(|check| (check.file, check.digest_type))
                .collect();
            // 结果与候选按位对应（rayon 收进 Vec 保序），所以能配回各自那条记录与目标修订。
            let policy = if options.verify_all {
                CachePolicy::Ignore
            } else {
                CachePolicy::Use
            };
            let results = parallel_compute_digests(candidates, cache, policy)?;
            // 边算边存：这一阶段占掉大部分运行时间，后面的失败不该把它整个丢掉。
            save_cache(cache_writer, cache, false)?;

            for (outcome, check) in results.into_iter().zip(&needs_digest) {
                hashed.record(outcome.from_cache, outcome.file.size);

                // 走到这里必然 have == target_rev，记录里的摘要就是目标版本的摘要
                // （补查只把 head_rev != have_rev 的记录换成 have 版本的摘要）。
                let expected_digest = check.source.record.digest.ok_or_else(|| {
                    anyhow!("Missing digest for {}", check.source.record.depot_file)
                })?;
                if outcome.digest != expected_digest {
                    drifted.push(check.source);

                    if options.verbose {
                        println!("         File \"{}\" digest is wrong.", outcome.file.path);
                    }
                }
            }

            hashed.report();
        }
    }

    Ok(SyncPlan {
        changes: SyncChanges::project(&analysis, drifted),
        unsupported: analysis
            .unsupported
            .iter()
            .map(|record| record.depot_file.clone())
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::UNIX_EPOCH;

    use crate::model::{DepotFileRecord, WorkspaceFile};
    use crate::path::local_path_key;
    use crate::reconcile::analyze::DeletedAtTarget;

    fn sync_file(client_file: &str, depot_file: &str, target_rev: u32) -> SyncFile {
        SyncFile {
            client_file: client_file.to_owned(),
            depot_file: depot_file.to_owned(),
            target_rev,
        }
    }

    /// 缺省是「有 have 记录」——删除组的典型情形（have 停在旧版、目标处已删除）。
    /// 没有记录的那种单独构造，见 `delete_specs_skip_the_files_without_a_have_record`。
    fn delete_file(client_file: &str, depot_file: &str) -> DeleteFile {
        DeleteFile {
            client_file: client_file.to_owned(),
            depot_file: depot_file.to_owned(),
            has_have_record: true,
        }
    }

    /// 一条够用的 depot 记录：`SyncSource` 与投影只用到路径字段。
    fn record(client_file: &str, depot_file: &str) -> DepotFileRecord {
        DepotFileRecord {
            client_file: client_file.to_owned(),
            client_file_lower: local_path_key(client_file),
            depot_file: depot_file.to_owned(),
            depot_file_lower: depot_file.to_ascii_lowercase(),
            ..Default::default()
        }
    }

    fn workspace_file(path: &str) -> WorkspaceFile {
        WorkspaceFile {
            path: path.to_owned(),
            path_lower: local_path_key(path),
            size: 3,
            date: UNIX_EPOCH,
            filtered: false,
        }
    }

    /// 四类各放一个可区分的文件名，用来验证标签与文件的配对。
    fn sync_with_one_file_each() -> SyncChanges {
        SyncChanges {
            update: vec![sync_file("update.txt", "//depot/update.txt", 2)],
            revert: vec![sync_file("revert.txt", "//depot/revert.txt", 3)],
            restore: vec![sync_file("restore.txt", "//depot/restore.txt", 4)],
            delete: vec![delete_file("delete.txt", "//depot/delete.txt")],
        }
    }

    // ---- 表与字段的配对 ----

    /// [SYNC_GROUPS] 与 [SyncChanges] 字段的对应关系全靠人工维护：条目数量对不上会
    /// 编译失败，但两条**对调**不会——那只会让标题、清单与实际动作错配。这里钉死整张表。
    #[test]
    fn sync_group_order_matches_the_four_sync_fields() {
        let sync = sync_with_one_file_each();

        let observed: Vec<(&str, Vec<&str>)> = sync
            .groups()
            .map(|(spec, files)| (spec.label, files.client_files()))
            .collect();

        assert_eq!(
            observed,
            vec![
                ("Update", vec!["update.txt"]),
                ("Revert", vec!["revert.txt"]),
                ("Restore", vec!["restore.txt"]),
                ("Delete", vec!["delete.txt"]),
            ]
        );
        assert_eq!(sync.total(), 4);
    }

    /// 动作与字段类型必须一一配齐。配错了 `apply_sync` 会走 `bail!` 分支中止整轮，
    /// 所以这里正向证明那个分支到不了。
    #[test]
    fn every_group_is_paired_with_the_matching_action() {
        for (spec, files) in sync_with_one_file_each().groups() {
            match (spec.action, &files) {
                (SyncAction::SyncToTarget, SyncGroupFiles::Sync(_)) => {}
                (SyncAction::DeleteFromDisk, SyncGroupFiles::Delete(_)) => {}
                (action, _) => panic!("{}: mispaired with {action:?}", spec.label),
            }
        }
    }

    #[test]
    fn every_title_has_one_placeholder_and_the_output_indent() {
        for spec in &SYNC_GROUPS {
            assert_eq!(spec.title.matches("{}").count(), 1, "{}", spec.label);
            // 前导空格是输出契约的一部分：标题要和 --list 的清单对齐。
            assert!(spec.title.starts_with("      "), "{}", spec.label);

            let rendered = render_title(spec.title, 3);
            assert!(!rendered.contains("{}"), "{}", spec.label);
            assert!(rendered.contains("3 files"), "{}: {rendered}", spec.label);
        }
    }

    /// sync 只下发 `p4 sync -f`：不开文件（不产生 changelist，`-c` 也就没有意义），
    /// 更不能用 `-k`——那是「只动 have list、不动文件内容」，与「把工作区拉到目标版本」
    /// 正好相反。
    #[test]
    fn sync_only_ever_syncs_files_into_place() {
        assert_eq!(SYNC_ARGS, ["sync", "-f"].as_slice());
        assert!(!SYNC_ARGS.contains(&"-c"));
        assert!(!SYNC_ARGS.contains(&"-k"));
        // `-K` 抑制 ktext 关键字展开，而 `p4 sync` 默认展开（`-K` 才抑制），
        // 所以刻意不带它——对上的是 p4 sync 的默认行为。
        assert!(!SYNC_ARGS.contains(&"-K"));
    }

    #[test]
    fn sync_specs_pin_the_target_revision() {
        let specs = sync_specs(&[
            sync_file("a.txt", "//depot/a.txt", 3),
            sync_file("b.txt", "//depot/b.txt", 7),
        ]);

        // 钉住版本是全部要点：不带版本的 `-f` 会同步到 head，`--to <CL>` 下就跑到目标前面去了。
        assert_eq!(specs, ["//depot/a.txt#3", "//depot/b.txt#7"]);
    }

    // ---- 投影 ----

    /// 摘要判定为「不是目标版本」的那批必须并进 Revert 组，而不是另开一类：
    /// 它们的动作与「二进制长度不符」那批完全一样，只是判据不同。
    #[test]
    fn projection_merges_the_digest_drifted_files_into_revert() {
        let records = [
            record("sized.txt", "//depot/sized.txt"),
            record("drifted.txt", "//depot/drifted.txt"),
        ];
        let analysis = SyncAnalysis {
            revert: vec![SyncSource {
                record: &records[0],
                target_rev: 5,
            }],
            ..Default::default()
        };

        let changes = SyncChanges::project(
            &analysis,
            vec![SyncSource {
                record: &records[1],
                target_rev: 5,
            }],
        );

        assert_eq!(
            changes.revert,
            [
                sync_file("sized.txt", "//depot/sized.txt", 5),
                sync_file("drifted.txt", "//depot/drifted.txt", 5),
            ]
        );
        assert_eq!(changes.total(), 2);
    }

    /// 转交与归档两类不产生任何动作：它们只是「这一轮没处理」的汇报，
    /// 走的是 [`SyncPlan::unsupported`] 与分类阶段那句归档提示。
    #[test]
    fn projection_produces_no_actions_from_the_handed_off_files() {
        let records = [
            record("weird.txt", "//depot/weird.txt"),
            record("archived.txt", "//depot/archived.txt"),
        ];
        let analysis = SyncAnalysis {
            unsupported: vec![&records[0]],
            archived: vec![&records[1]],
            ..Default::default()
        };

        let changes = SyncChanges::project(&analysis, Vec::new());

        assert_eq!(changes.total(), 0);
    }

    /// 删除组两个路径都要留着：本地路径用来删文件，depot 路径用来让 p4 清 have 记录。
    /// 少任何一个，这一组就有一半情形做不到（见 [`DeleteFile`]）。
    #[test]
    fn projection_keeps_both_paths_of_the_delete_group() {
        let records = [DepotFileRecord {
            have_rev: Some(1),
            ..record("gone.txt", "//depot/gone.txt")
        }];
        let files = [workspace_file(r"C:\ws\gone.txt")];
        let analysis = SyncAnalysis {
            delete: vec![DeletedAtTarget {
                record: &records[0],
                file: &files[0],
            }],
            ..Default::default()
        };

        let changes = SyncChanges::project(&analysis, Vec::new());

        assert_eq!(
            changes.delete,
            [delete_file(r"C:\ws\gone.txt", "//depot/gone.txt")]
        );
    }

    /// 「本 client 从没同步过」的那批同样进删除组（判据是「目标时刻不在库 + 本地有」，
    /// 与 have 记录无关），但 `has_have_record` 要为假——p4 那一步得跳过它们。
    #[test]
    fn projection_marks_the_files_without_a_have_record() {
        let records = [record("gone.txt", "//depot/gone.txt")]; // have_rev 缺省为 None
        let files = [workspace_file(r"C:\ws\gone.txt")];
        let analysis = SyncAnalysis {
            delete: vec![DeletedAtTarget {
                record: &records[0],
                file: &files[0],
            }],
            ..Default::default()
        };

        let changes = SyncChanges::project(&analysis, Vec::new());

        assert_eq!(
            changes.delete,
            [DeleteFile {
                client_file: r"C:\ws\gone.txt".to_owned(),
                depot_file: "//depot/gone.txt".to_owned(),
                has_have_record: false,
            }]
        );
    }

    #[test]
    fn delete_specs_pin_the_none_revision() {
        let specs = delete_specs(&[delete_file("a.txt", "//depot/a.txt")]);

        // `#none` 在 p4 语法里是「这个文件在工作区里不该有」：删文件之外还会清掉 have 记录。
        assert_eq!(specs, ["//depot/a.txt#none"]);
    }

    /// 没有 have 记录的路径不进 p4 调用：`#none` 对它们是 no-op，p4 只会回一句
    /// `file(s) up-to-date.` 到 stderr——那句良性提示会逼着判据退回看不见真失败的
    /// `ExitCode`（见 [`delete_specs`]）。这批文件靠删文件那一步处理。
    #[test]
    fn delete_specs_skip_the_files_without_a_have_record() {
        let never_synced = DeleteFile {
            client_file: r"C:\ws\b.txt".to_owned(),
            depot_file: "//depot/b.txt".to_owned(),
            has_have_record: false,
        };

        let specs = delete_specs(&[delete_file("a.txt", "//depot/a.txt"), never_synced]);

        assert_eq!(specs, ["//depot/a.txt#none"]);
    }
}
