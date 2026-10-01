//! 两阶段差异分析：把 depot 记录与工作区文件互相印证，决定每个文件落在哪一类变更里。
//!
//! 这里是纯逻辑——不碰文件系统、不起 p4 子进程、不读时钟。`reconcile_dir` 负责把
//! fstat / 工作区扫描 / have 三路结果凑齐，再把工作区补齐（已剪目录里 depot 已跟踪
//! 的文件必须回到 `workspace` 里，否则会被当成被删除），然后才交给 [`analyze`]。

use anyhow::{Result, bail};

use crate::model::{
    DepotFileRecord, DepotState, DigestType, FileAction, FileType, WorkspaceFile, WorkspaceState,
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
                                    bail!("Cannot handle \"{}\" 1", record.client_file);
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
                        _ => bail!("Cannot handle \"{}\" 2", record.client_file),
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
                _ => bail!("Cannot handle \"{}\" 3", record.client_file),
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
                        _ => bail!("Cannot handle \"{}\" 5", file.path),
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
                                bail!("Cannot handle \"{}\" 6", file.path);
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
                        _ => bail!("Cannot handle \"{}\" 7", file.path),
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::UNIX_EPOCH;

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

    /// `(守卫编号, depot 记录, 工作区文件)`。
    type UnhandledCase = (
        &'static str,
        DepotFileRecord,
        &'static [(&'static str, u64, bool)],
    );

    /// 遇到没建模的记录形状要报错，而不是猜一个分类——猜错会直接改到用户的
    /// changelist。每条守卫都带上文件名，好知道是哪个文件触发的。
    ///
    /// 编号对应源码里的 `Cannot handle "{}" N`。7 号（phase two 里 action 是
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
            ),
            (
                "2",
                DepotFileRecord {
                    action: Some(FileAction::Add),
                    ..synced_record(PATH)
                },
                &[],
            ),
            (
                "3",
                DepotFileRecord {
                    action: Some(FileAction::Edit),
                    ..open_add_record(PATH)
                },
                &[],
            ),
            (
                "5",
                DepotFileRecord {
                    head_action: Some(FileAction::Delete),
                    action: Some(FileAction::Edit),
                    ..synced_record(PATH)
                },
                &[(PATH, SIZE, false)],
            ),
            (
                "6",
                DepotFileRecord {
                    action: Some(FileAction::Edit),
                    head_type: None,
                    ..synced_record(PATH)
                },
                &[(PATH, SIZE, false)],
            ),
        ];

        for (number, record, workspace_entries) in cases {
            let depot = depot_state(vec![record]);
            let workspace = workspace_state(workspace_entries);
            let error = analyze(&depot, &workspace, false)
                .err()
                .unwrap_or_else(|| panic!("case {number} must fail"))
                .to_string();

            assert!(
                error.contains(&format!("Cannot handle \"{PATH}\" {number}")),
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
}
