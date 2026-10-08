//! reconcile 找出的变更分类，以及它们的报告与应用。
//!
//! 八类变更在「打印标题、列清单、下发 p4 命令」上只有细节差异，
//! 集中成一张 [GROUPS] 表，避免同一套控制流重复八遍。

use std::time::Instant;

use anyhow::{Result, anyhow, bail};

use crate::cli::Options;
use crate::json::{FileRecord, Mode, count, emit_file, sayln};
use crate::model::DepotState;
use crate::p4::process::{FailureMode, run_p4_command_batched};
use crate::path::{escape_file_spec, local_path_key};

/// 一个新增文件（工作区里有、depot 里没有、也没打开过）。
///
/// 本地路径是扫描直接产出的；depot 路径要问过 client view 才知道（`p4 where`），
/// 所以它在 [`crate::workspace::map_new_paths`] 跑完之前是缺的。这个「先有本地、后有 depot」
/// 的两段式就是它值得单独一个类型的理由——[`Changes`] 的其余七类都是 depot 记录的回声，
/// 两个路径同时到手。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NewFile {
    /// 本地路径：下发 `p4 add`、删盘与 `-l` 清单都用它。
    pub(crate) client_file: String,

    /// `p4 where` 给出的 depot 路径。`None` 只出现在 analyze 与 map 之间的窗口里。
    pub(crate) depot_file: Option<String>,
}

impl NewFile {
    /// analyze 阶段只知道本地路径。
    pub(crate) fn unmapped(client_file: String) -> Self {
        NewFile {
            client_file,
            depot_file: None,
        }
    }
}

/// 一类变更的一行：`-l` 清单、JSON 记录与下发给 p4 的参数都看它。
#[derive(Debug)]
pub(crate) struct GroupRow<'a> {
    /// 本地路径。
    pub(crate) client_file: &'a str,

    /// depot 路径。契约里 `depotFile` 是必填项：新增文件走 `p4 where` 的映射，
    /// 其余七类走 depot 记录，两条来源在 [`Changes::groups`] 里收敛成同一个字段。
    pub(crate) depot_file: &'a str,

    /// have 版本；新增文件没有（`-Mj` 的 `rev` 字段同样缺席）。
    pub(crate) rev: Option<u32>,
}

/// 一次 reconcile 找出的全部变更，按处理方式分类。
/// 每个文件最多只落在其中一类里。
#[derive(Default)]
pub(crate) struct Changes {
    /// 工作区里有，但 depot 里没有或 have 版本已删除，且没有 open for add。
    pub(crate) add: Vec<NewFile>,

    /// 工作区里有，相对 have 版本有改动，但没有 open for edit。
    pub(crate) edit: Vec<String>,

    /// 工作区里有，相对 have 版本有改动，但已 open for delete（先 revert 再 edit）。
    pub(crate) reopen_edit: Vec<String>,

    /// 工作区里没有，且没有 open for delete。
    pub(crate) delete: Vec<String>,

    /// 工作区里没有，但已 open for edit（先 revert 再 delete）。
    pub(crate) reopen_delete: Vec<String>,

    /// 工作区里没有，但已 open for add。
    pub(crate) revert_add: Vec<String>,

    /// 工作区里有，相对 have 版本没改动，但已 open for edit。
    pub(crate) revert_edit: Vec<String>,

    /// 工作区里有，相对 have 版本没改动，但已 open for delete。
    pub(crate) revert_delete: Vec<String>,
}

/// 一类变更在报告与应用上的全部差异。
#[derive(Debug)]
struct GroupSpec {
    /// JSON 记录的 `class`：稳定英文枚举，见 `docs/json-contract.md`。
    ///
    /// 与 `label` 分开是刻意的：`label` 是给人看的（`Reopen Edit`，带空格带大写），
    /// 消费方解析的是这个。两者都改的时候要一起改，`every_group_has_a_distinct_id` 钉住。
    id: &'static str,

    /// `-l` 清单里每行的前缀，例如 `Add` / `Reopen Edit`。
    label: &'static str,

    /// 标题模板，`{}` 由文件数量填充。前导空格是输出契约的一部分。
    title: &'static str,

    /// 应用时依次执行的 p4 调用：参数，以及是否带上 changelist。
    commands: &'static [(&'static [&'static str], bool)],
}

/// 八类变更的处理方式。顺序即输出顺序，必须与 [Changes::groups] 一一对应；
/// 每一类是什么意思，见 [Changes] 对应字段的文档。
const GROUPS: [GroupSpec; 8] = [
    GroupSpec {
        id: "add",
        label: "Add",
        title: "      Adding {} files in workspace, not in depot or deleted at have revision, but not checked out for add.",
        commands: &[(&["add"], true)],
    },
    GroupSpec {
        id: "edit",
        label: "Edit",
        title: "      Editing {} files in workspace, changed from have revision, but not checked out for edit.",
        commands: &[(&["edit"], true)],
    },
    GroupSpec {
        id: "reopen_edit",
        label: "Reopen Edit",
        title: "      Revert+Editing {} files in workspace, changed from have revision, but checked out for delete.",
        commands: &[(&["revert", "-k"], false), (&["edit"], true)],
    },
    GroupSpec {
        id: "delete",
        label: "Delete",
        title: "      Deleting {} files not in workspace, but not checked out for delete.",
        commands: &[(&["delete", "-k"], true)],
    },
    GroupSpec {
        id: "reopen_delete",
        label: "Reopen Delete",
        title: "      Revert+Deleting {} files not in workspace, but checked out for edit.",
        commands: &[(&["revert", "-k"], false), (&["delete", "-k"], true)],
    },
    GroupSpec {
        id: "revert_add",
        label: "Revert Add",
        title: "      Reverting {} files not in workspace, but checked out for add.",
        commands: &[(&["revert", "-k"], false)],
    },
    GroupSpec {
        id: "revert_edit",
        label: "Revert Edit",
        title: "      Reverting {} files in workspace, not changed from have revision, but checked out for edit.",
        commands: &[(&["revert", "-k"], false)],
    },
    GroupSpec {
        id: "revert_delete",
        label: "Revert Delete",
        title: "      Reverting {} files in workspace, not changed from have revision, but checked out for delete.",
        commands: &[(&["revert", "-k"], false)],
    },
];

/// 三个 revert 组。它们是 `p4 revert -a` 的语义，不在 `p4 reconcile -a -e -d` 里，
/// `--no-revert-groups` 按这份名单过滤（实测对照见 `docs/json-contract.md`）。
const REVERT_GROUP_IDS: [&str; 3] = ["revert_add", "revert_edit", "revert_delete"];

impl GroupSpec {
    fn title_with(&self, count: usize) -> String {
        render_title(self.title, count)
    }
}

pub(crate) fn render_title(template: &str, count: usize) -> String {
    template.replace("{}", &count.to_string())
}

/// 一类变更的报告参数。
pub(crate) struct GroupReport<'a> {
    pub(crate) mode: Mode,
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) title: &'a str,
    pub(crate) list: bool,
    pub(crate) applied: bool,
    /// 普通同步才有：`preview` 或 `apply`。其余模式是 `None`。
    pub(crate) stage: Option<&'static str>,
}

/// 打印一类变更的标题、逐文件清单，并发这一组的 JSON 记录。
///
/// 六格缩进的标题、`         {label} "{file}".` 的清单行都是输出契约的一部分，
/// open / clean / sync 三个模式共用这一处，免得几边的文案各漂各的。
///
/// 文本报告走 [`sayln!`]（`--json` 下改道 stderr），文件记录走 JSON sink（文本模式下是
/// 空操作）。两者在同一个循环里发，行序在两种模式下一致。
pub(crate) fn report_group(report: &GroupReport<'_>, rows: &[GroupRow<'_>]) {
    sayln!("{}", report.title);
    count(report.id, rows.len());

    for row in rows {
        if report.list {
            sayln!("         {} \"{}\".", report.label, row.client_file);
        }

        emit_file(&FileRecord {
            mode: report.mode,
            class: report.id,
            depot_file: row.depot_file,
            client_file: row.client_file,
            rev: row.rev,
            applied: report.applied,
            stage: report.stage,
            native_action: None,
        });
    }
}

impl Changes {
    pub(crate) fn total(&self) -> usize {
        self.add.len()
            + self.edit.len()
            + self.reopen_edit.len()
            + self.delete.len()
            + self.reopen_delete.len()
            + self.revert_add.len()
            + self.revert_edit.len()
            + self.revert_delete.len()
    }

    /// 按 [GROUPS] 的顺序把每一类与它的行配成对。
    ///
    /// `no_revert_groups` 时三组 revert 让位给 `p4 reconcile -a -e -d` 的语义，
    /// 编辑器要与原生引擎逐行对齐就是靠这一档：
    ///
    /// - `revert_add` / `revert_edit` 整组消失（原生对这两类一个字都不报）；
    /// - `revert_delete` 并进 `reopen_edit`。原生 `-e` 对「文件还在、却被 open for
    ///   delete」的处理**与内容无关**，一律改开成 edit（实测），而 δ 默认把内容没变的
    ///   那种当成「过时的打开」撤销掉——那是 `p4 revert -a` 的立场。这一档按原生来：
    ///   报出来的类是 `reopen_edit`（动作 `edit`），下发的 `revert -k` + `edit` 也正是
    ///   「改开成 edit」。
    fn groups<'a>(
        &'a self,
        depot: &'a DepotState,
        no_revert_groups: bool,
    ) -> Result<Vec<(&'static GroupSpec, Vec<GroupRow<'a>>)>> {
        let mut groups = Vec::with_capacity(GROUPS.len());

        // 第一项是 add（走 `p4 where` 的映射），其余七项都是 depot 记录的回声。
        for (spec, records) in GROUPS.iter().zip([
            None,
            Some(self.edit.as_slice()),
            Some(self.reopen_edit.as_slice()),
            Some(self.delete.as_slice()),
            Some(self.reopen_delete.as_slice()),
            Some(self.revert_add.as_slice()),
            Some(self.revert_edit.as_slice()),
            Some(self.revert_delete.as_slice()),
        ]) {
            if no_revert_groups && REVERT_GROUP_IDS.contains(&spec.id) {
                continue;
            }

            let rows = match records {
                None => rows_from_new_files(&self.add)?,
                // `--no-revert-groups` 下 `revert_delete` 的那批文件并进这一组
                // （理由见本方法的文档）：两个来源的路径都是 `self` 的借出，拼在一起即可。
                Some(records) if no_revert_groups && spec.id == "reopen_edit" => {
                    let mut rows = rows_from_records(records, depot)?;
                    rows.extend(rows_from_records(&self.revert_delete, depot)?);
                    rows
                }
                Some(records) => rows_from_records(records, depot)?,
            };
            groups.push((spec, rows));
        }

        Ok(groups)
    }
}

/// 从 depot 记录回声出来的七类：两个路径与 have 版本都在记录上。
fn rows_from_records<'a>(files: &'a [String], depot: &'a DepotState) -> Result<Vec<GroupRow<'a>>> {
    files
        .iter()
        .map(|client_file| {
            // 这一类的文件必然在 depot 记录里（analyze 是从记录出发分类的）。查不到说明
            // 分析结果与 depot 状态对不上，这时「depot 路径是什么」没有正确答案，
            // 编一个出来只会把错的东西写进记录里。
            let record = depot
                .get_client_record(&local_path_key(client_file))
                .ok_or_else(|| anyhow!("Failed to find depot record for {client_file}"))?;

            Ok(GroupRow {
                client_file,
                depot_file: record.depot_file.as_str(),
                rev: record.have_rev,
            })
        })
        .collect()
}

/// 新增文件：depot 路径来自 [`crate::workspace::map_new_paths`]。
///
/// 缺了就是那一处漏了路径，让整轮响亮失败——契约里 `depotFile` 是必填项，
/// 发一条没有它的记录等于给消费方一个读不懂的答案。
pub(super) fn rows_from_new_files(files: &[NewFile]) -> Result<Vec<GroupRow<'_>>> {
    files
        .iter()
        .map(|file| {
            let depot_file = file.depot_file.as_deref().ok_or_else(|| {
                anyhow!(
                    "Missing the depot mapping for the new file {}",
                    file.client_file
                )
            })?;

            Ok(GroupRow {
                client_file: &file.client_file,
                depot_file,
                rev: None,
            })
        })
        .collect()
}

/// 报告全部变更，并在 `-a` 时把它们应用到 p4。
pub(crate) async fn apply_changes(
    options: &Options,
    work_dir: &str,
    changes: &Changes,
    depot: &DepotState,
) -> Result<()> {
    if options.apply {
        sayln!("   Applying changes to p4.");
    } else {
        sayln!("   Counting changes (dry run).")
    }

    let start_time = Instant::now();
    let mut failures: Vec<String> = Vec::new();

    for (spec, rows) in changes.groups(depot, options.no_revert_groups)? {
        if rows.is_empty() {
            continue;
        }

        report_group(
            &GroupReport {
                mode: Mode::Open,
                id: spec.id,
                label: spec.label,
                title: &spec.title_with(rows.len()),
                list: options.list,
                applied: options.apply,
                stage: None,
            },
            &rows,
        );

        if !options.apply {
            continue;
        }

        // 交给 p4 的是 **file spec**：本地路径里的 `#`/`@`/`%`/`*`/`?` 要转义一次，否则
        // p4 会把 `notes#1.txt` 读成「notes 的第 1 版」。分析侧查得到、这里发不下去的话，
        // 报告出来就是「发现了一条改动，然后失败」——而路径本身完全合法。
        let files: Vec<String> = rows
            .iter()
            .map(|row| escape_file_spec(row.client_file))
            .collect();

        for (args, use_changelist) in spec.commands {
            // 这一支只在 -a 时走到，所以永远是「真改状态」：任何一批失败都必须让整轮失败。
            // 以前这里是宽松模式，p4 拒绝开文件时只留一行 stderr 警告，程序照样打印
            // "Inconsistencies fixed." 并退出 0——把失败伪装成了成功。
            //
            // 判据要比退出码更严：p4 对逐文件错误（protections 拒绝 open、"not on
            // client"）返回的退出码是 0，只用 ExitCode 仍会把「一个文件都没打开」报成
            // 成功，见 [FailureMode::ExitCodeOrStderr]。
            let result = run_p4_command_batched(
                options,
                work_dir,
                args,
                &files,
                *use_changelist,
                FailureMode::ExitCodeOrStderr,
            )
            .await;

            if let Err(error) = result {
                // 这一组剩下的命令不再执行：「先 revert 再重开」这类组合半途而废没有意义。
                // 但**其余各组照跑**：一个文件名被 p4 拒掉（名字里有 `@`、`#`、`%` 的那类）
                // 不该让整轮停摆，把能做的都做完再一起报错。
                failures.push(format!("{} ({} files): {error}", spec.label, files.len()));
                break;
            }
        }
    }

    let total = changes.total();
    if options.apply {
        if !failures.is_empty() {
            bail!(
                "Failed to apply {} change group(s):\n  {}",
                failures.len(),
                failures.join("\n  ")
            );
        }

        sayln!(
            "      Applied {} changes in {} seconds.",
            total,
            start_time.elapsed().as_secs_f32()
        );
        sayln!("Inconsistencies fixed.");
    } else {
        sayln!(
            "      Counted {} changes in {} seconds.",
            total,
            start_time.elapsed().as_secs_f32()
        );
        sayln!("Inconsistencies found. Re-run with -a to apply changes.");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::model::DepotFileRecord;
    use crate::test_util::depot_record;

    /// 建一个带好映射的 [DepotState]。两个索引字段是 model 私有的，只能这样填。
    fn depot_with(records: Vec<DepotFileRecord>) -> DepotState {
        let mut depot = DepotState::default();
        depot.file_records = records;
        depot.build_mapping();
        depot
    }

    /// 与七个 depot 回声类同名的一条记录，have 版本取 3。
    fn record(client_file: &str) -> DepotFileRecord {
        DepotFileRecord {
            depot_file: format!("//depot/{client_file}"),
            have_rev: Some(3),
            ..depot_record(client_file)
        }
    }

    /// 八类各放一个可区分的文件名，用来验证标签与文件的配对。
    fn changes_with_one_file_each() -> Changes {
        Changes {
            add: vec![NewFile {
                client_file: "add.txt".to_owned(),
                depot_file: Some("//depot/add.txt".to_owned()),
            }],
            edit: vec!["edit.txt".to_owned()],
            reopen_edit: vec!["reopen_edit.txt".to_owned()],
            delete: vec!["delete.txt".to_owned()],
            reopen_delete: vec!["reopen_delete.txt".to_owned()],
            revert_add: vec!["revert_add.txt".to_owned()],
            revert_edit: vec!["revert_edit.txt".to_owned()],
            revert_delete: vec!["revert_delete.txt".to_owned()],
        }
    }

    fn depot_for_one_file_each() -> DepotState {
        depot_with(
            [
                "edit.txt",
                "reopen_edit.txt",
                "delete.txt",
                "reopen_delete.txt",
                "revert_add.txt",
                "revert_edit.txt",
                "revert_delete.txt",
            ]
            .iter()
            .map(|name| record(name))
            .collect(),
        )
    }

    /// [GROUPS] 与 [Changes] 字段的对应关系全靠人工维护：条目数量对不上会编译失败，
    /// 但两条**对调**不会——那只会让 `-l` 的标签与文件列表错配。这里把整张配对表钉死。
    #[test]
    fn group_order_matches_the_changes_fields() {
        let changes = changes_with_one_file_each();
        let depot = depot_for_one_file_each();

        let mut observed: Vec<(&str, &str)> = Vec::new();
        for (spec, rows) in changes.groups(&depot, false).unwrap() {
            for row in rows {
                observed.push((spec.label, row.client_file));
            }
        }

        assert_eq!(
            observed,
            vec![
                ("Add", "add.txt"),
                ("Edit", "edit.txt"),
                ("Reopen Edit", "reopen_edit.txt"),
                ("Delete", "delete.txt"),
                ("Reopen Delete", "reopen_delete.txt"),
                ("Revert Add", "revert_add.txt"),
                ("Revert Edit", "revert_edit.txt"),
                ("Revert Delete", "revert_delete.txt"),
            ]
        );
    }

    /// 每一行都要有 depot 路径：契约里 `depotFile` 是必填项。新增文件那一类的来源与
    /// 其余七类不同（`p4 where` 而不是 depot 记录），这条用例把两条来源都覆盖了。
    #[test]
    fn every_row_carries_both_paths() {
        let changes = changes_with_one_file_each();

        for (spec, rows) in changes.groups(&depot_for_one_file_each(), false).unwrap() {
            assert_eq!(rows.len(), 1, "{}", spec.label);
            let row = &rows[0];
            assert!(
                row.depot_file.starts_with("//depot/"),
                "{}: {}",
                spec.label,
                row.depot_file
            );
            // 新增文件没有 have 版本，其余七类都是 3。
            let expected_rev = (spec.id != "add").then_some(3);
            assert_eq!(row.rev, expected_rev, "{}", spec.label);
        }
    }

    /// `--no-revert-groups` 让出来的必须**恰好**是那三组：多让一组会静默丢掉
    /// `p4 reconcile -a -e -d` 该做的事，少让一组会把 `p4 revert -a` 的事做进去。
    ///
    /// 其中 `revert_delete` 不是消失而是**并入** `reopen_edit`——原生 `-e` 对「文件还在、
    /// 却被 open for delete」一律改开成 edit，与内容无关。
    #[test]
    fn no_revert_groups_hands_the_three_revert_groups_over_to_native_semantics() {
        let changes = changes_with_one_file_each();
        let depot = depot_for_one_file_each();

        let groups = changes.groups(&depot, true).unwrap();
        let ids: Vec<&str> = groups.iter().map(|(spec, _)| spec.id).collect();

        assert_eq!(
            ids,
            ["add", "edit", "reopen_edit", "delete", "reopen_delete"]
        );
        assert!(!ids.contains(&"revert_edit"));
        assert!(!ids.contains(&"revert_add"));

        // 两个来源的文件都在 `reopen_edit` 名下，顺序是「本来就是 reopen 的」在前。
        let rows = changes
            .groups(&depot, true)
            .unwrap()
            .into_iter()
            .find(|(spec, _)| spec.id == "reopen_edit")
            .map(|(_, rows)| rows)
            .expect("reopen_edit 组必须在");
        assert_eq!(
            rows.iter().map(|row| row.client_file).collect::<Vec<_>>(),
            ["reopen_edit.txt", "revert_delete.txt"]
        );

        // 默认档不受影响：三类各归各的组。
        let default_ids: Vec<&str> = changes
            .groups(&depot, false)
            .unwrap()
            .iter()
            .map(|(spec, _)| spec.id)
            .collect();
        assert!(default_ids.contains(&"revert_delete"));
        assert_eq!(default_ids.len(), 8);
    }

    /// `id` 是线协议上的东西，重复一个就会让消费方把两类变更读成一类。
    #[test]
    fn every_group_has_a_distinct_id() {
        let mut ids: Vec<&str> = GROUPS.iter().map(|spec| spec.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();

        assert_eq!(ids.len(), count, "组 id 有重复：{ids:?}");
        // id 是给程序读的：小写下划线，不许出现空格与大写。
        for id in &ids {
            assert!(
                id.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{id} 不像个机器读的 id"
            );
        }
    }

    /// 新增文件少一个 depot 映射时必须响亮失败：契约里 `depotFile` 是必填项，
    /// 发一条没有它的记录等于给消费方一个读不懂的答案。
    #[test]
    fn a_new_file_without_a_mapping_fails_the_round() {
        let changes = Changes {
            add: vec![NewFile::unmapped("brand-new.txt".to_owned())],
            ..Default::default()
        };

        let error = changes
            .groups(&DepotState::default(), false)
            .expect_err("缺 depot 映射时必须报错");

        assert!(error.to_string().contains("brand-new.txt"), "{error}");
    }

    /// 七类里任何一类查不到 depot 记录同样是「分析结果与 depot 状态对不上」，
    /// 不许拼一个假的 depot 路径糊过去。
    #[test]
    fn a_row_without_a_depot_record_fails_the_round() {
        let changes = Changes {
            edit: vec!["ghost.txt".to_owned()],
            ..Default::default()
        };

        let error = changes
            .groups(&DepotState::default(), true)
            .expect_err("缺 depot 记录时必须报错");

        assert!(error.to_string().contains("ghost.txt"), "{error}");
    }

    #[test]
    fn every_title_has_one_placeholder_and_the_output_indent() {
        for spec in &GROUPS {
            assert_eq!(spec.title.matches("{}").count(), 1, "{}", spec.label);
            // 前导空格是输出契约的一部分：标题要和 --list 的清单对齐。
            assert!(spec.title.starts_with("      "), "{}", spec.label);

            let rendered = spec.title_with(3);
            assert!(!rendered.contains("{}"), "{}", spec.label);
            assert!(rendered.contains("3 files"), "{}: {rendered}", spec.label);
        }
    }

    #[test]
    fn total_counts_all_eight_groups() {
        assert_eq!(Changes::default().total(), 0);

        let changes = Changes {
            add: vec![NewFile::unmapped(String::new())],
            edit: vec![String::new(); 2],
            reopen_edit: vec![String::new(); 3],
            delete: vec![String::new(); 4],
            reopen_delete: vec![String::new(); 5],
            revert_add: vec![String::new(); 6],
            revert_edit: vec![String::new(); 7],
            revert_delete: vec![String::new(); 8],
        };
        assert_eq!(changes.total(), 36);
    }

    /// revert 只撤销本地的打开状态，不该把结果送进某个 pending changelist；
    /// 反过来，真正改动文件的命令必须显式带上 changelist。
    #[test]
    fn revert_commands_never_target_a_changelist() {
        for spec in &GROUPS {
            for (args, use_changelist) in spec.commands {
                let is_revert = args.first() == Some(&"revert");
                assert_eq!(*use_changelist, !is_revert, "{}: {args:?}", spec.label);
            }
        }

        // 「先 revert 再重开」的组合必须保持顺序，否则 revert 会抹掉刚重开的结果。
        for spec in GROUPS
            .iter()
            .filter(|spec| spec.label.starts_with("Reopen"))
        {
            assert_eq!(spec.commands.len(), 2, "{}", spec.label);
            assert_eq!(
                spec.commands[0].0,
                ["revert", "-k"].as_slice(),
                "{}",
                spec.label
            );
            assert!(!spec.commands[0].1, "{}", spec.label);
        }
    }
}
