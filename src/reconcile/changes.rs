//! reconcile 找出的变更分类，以及它们的报告与应用。
//!
//! 八类变更在"打印标题、列清单、下发 p4 命令"上只有细节差异，
//! 集中成一张 [GROUPS] 表，避免同一套控制流重复八遍。

use std::time::Instant;

use anyhow::Result;

use crate::cli::Options;
use crate::p4::process::run_p4_command_batched;

/// 一次 reconcile 找出的全部变更，按处理方式分类。
/// 每个文件最多只落在其中一类里。
#[derive(Default)]
pub(crate) struct Changes {
    /// Files in workspace, not in depot or deleted at have revision, but not checked out for add.
    pub(crate) add: Vec<String>,

    /// Files in workspace, changed from have revision, but not checked out for edit.
    pub(crate) edit: Vec<String>,

    /// Files in workspace, changed from have revision, but checked out for delete.
    pub(crate) reopen_edit: Vec<String>,

    /// Files not in workspace, but not checked out for delete.
    pub(crate) delete: Vec<String>,

    /// Files not in workspace, but checked out for edit.
    pub(crate) reopen_delete: Vec<String>,

    /// Files not in workspace, but checked out for add.
    pub(crate) revert_add: Vec<String>,

    /// Files in workspace, not changed from have revision, but checked out for edit.
    pub(crate) revert_edit: Vec<String>,

    /// Files in workspace, not changed from have revision, but checked out for delete.
    pub(crate) revert_delete: Vec<String>,
}

/// 一类变更在报告与应用上的全部差异。
struct GroupSpec {
    /// `-l` 清单里每行的前缀，例如 `Add` / `Reopen Edit`。
    label: &'static str,

    /// 标题模板，`{}` 由文件数量填充。前导空格是输出契约的一部分。
    title: &'static str,

    /// 应用时依次执行的 p4 调用：参数，以及是否带上 changelist。
    commands: &'static [(&'static [&'static str], bool)],
}

/// 八类变更的处理方式。顺序即输出顺序，必须与 [Changes::groups] 一一对应。
const GROUPS: [GroupSpec; 8] = [
    // Files in workspace, not in depot or deleted at have revision, but not checked out for add.
    GroupSpec {
        label: "Add",
        title: "      Adding {} files in workspace, not in depot or deleted at have revision, but not checked out for add.",
        commands: &[(&["add"], true)],
    },
    // Files in workspace, changed from have revision, but not checked out for edit.
    GroupSpec {
        label: "Edit",
        title: "      Editing {} files in workspace, changed from have revision, but not checked out for edit.",
        commands: &[(&["edit"], true)],
    },
    // Files in workspace, changed from have revision, but checked out for delete.
    GroupSpec {
        label: "Reopen Edit",
        title: "      Revert+Editing {} files in workspace, changed from have revision, but checked out for delete.",
        commands: &[(&["revert", "-k"], false), (&["edit"], true)],
    },
    // Files not in workspace, but not checked out for delete.
    GroupSpec {
        label: "Delete",
        title: "      Deleting {} files not in workspace, but not checked out for delete.",
        commands: &[(&["delete", "-k"], true)],
    },
    // Files not in workspace, but checked out for edit.
    GroupSpec {
        label: "Reopen Delete",
        title: "      Revert+Deleting {} files not in workspace, but checked out for edit.",
        commands: &[(&["revert", "-k"], false), (&["delete", "-k"], true)],
    },
    // Files not in workspace, but checked out for add.
    GroupSpec {
        label: "Revert Add",
        title: "      Reverting {} files not in workspace, but checked out for add.",
        commands: &[(&["revert", "-k"], false)],
    },
    // Files in workspace, not changed from have revision, but checked out for edit.
    GroupSpec {
        label: "Revert Edit",
        title: "      Reverting {} files in workspace, not changed from have revision, but checked out for edit.",
        commands: &[(&["revert", "-k"], false)],
    },
    // Files in workspace, not changed from have revision, but checked out for delete.
    GroupSpec {
        label: "Revert Delete",
        title: "      Reverting {} files in workspace, not changed from have revision, but checked out for delete.",
        commands: &[(&["revert", "-k"], false)],
    },
];

impl GroupSpec {
    /// 渲染标题，把 `{}` 换成文件数量。
    fn title_with(&self, count: usize) -> String {
        render_title(self.title, count)
    }
}

/// 渲染标题模板，把 `{}` 换成文件数量。
pub(crate) fn render_title(template: &str, count: usize) -> String {
    template.replace("{}", &count.to_string())
}

/// 打印一类变更的标题，以及（`-l` 时的）逐文件清单。
///
/// 六格缩进的标题、`         {label} "{file}".` 的清单行都是输出契约的一部分，
/// open 模式与 clean 模式共用这一处，免得两边的文案各漂各的。
pub(crate) fn report_group<'a>(
    label: &str,
    title: &str,
    list: bool,
    files: impl IntoIterator<Item = &'a str>,
) {
    println!("{title}");

    if list {
        for file in files {
            println!("         {label} \"{file}\".");
        }
    }
}

impl Changes {
    /// 待处理的变更总数。
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

    /// 按 [GROUPS] 的顺序把每一类与它的文件配成对。
    fn groups(&self) -> impl Iterator<Item = (&'static GroupSpec, &[String])> {
        GROUPS.iter().zip([
            self.add.as_slice(),
            self.edit.as_slice(),
            self.reopen_edit.as_slice(),
            self.delete.as_slice(),
            self.reopen_delete.as_slice(),
            self.revert_add.as_slice(),
            self.revert_edit.as_slice(),
            self.revert_delete.as_slice(),
        ])
    }
}

/// 报告全部变更，并在 `-a` 时把它们应用到 p4。
pub(crate) async fn apply_changes(
    options: &Options,
    work_dir: &str,
    changes: &Changes,
) -> Result<()> {
    if options.apply {
        println!("   Applying changes to p4.");
    } else {
        println!("   Counting changes (dry run).")
    }

    let start_time = Instant::now();

    for (spec, files) in changes.groups() {
        if files.is_empty() {
            continue;
        }

        report_group(
            spec.label,
            &spec.title_with(files.len()),
            options.list,
            files.iter().map(String::as_str),
        );

        if options.apply {
            for (args, use_changelist) in spec.commands {
                run_p4_command_batched(options, work_dir, args, files, *use_changelist, false)
                    .await?;
            }
        }
    }

    let total = changes.total();
    if options.apply {
        println!(
            "      Applied {} changes in {} seconds.",
            total,
            start_time.elapsed().as_secs_f32()
        );
        println!("Inconsistencies fixed.");
    } else {
        println!(
            "      Counted {} changes in {} seconds.",
            total,
            start_time.elapsed().as_secs_f32()
        );
        println!("Inconsistencies found. Re-run with -a to apply changes.");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 八类各放一个可区分的文件名，用来验证标签与文件的配对。
    fn changes_with_one_file_each() -> Changes {
        Changes {
            add: vec!["add.txt".to_owned()],
            edit: vec!["edit.txt".to_owned()],
            reopen_edit: vec!["reopen_edit.txt".to_owned()],
            delete: vec!["delete.txt".to_owned()],
            reopen_delete: vec!["reopen_delete.txt".to_owned()],
            revert_add: vec!["revert_add.txt".to_owned()],
            revert_edit: vec!["revert_edit.txt".to_owned()],
            revert_delete: vec!["revert_delete.txt".to_owned()],
        }
    }

    /// [GROUPS] 与 [Changes] 字段的对应关系全靠人工维护：条目数量对不上会编译失败，
    /// 但两条**对调**不会——那只会让 `-l` 的标签与文件列表错配。这里把整张配对表钉死。
    #[test]
    fn group_order_matches_the_changes_fields() {
        let changes = changes_with_one_file_each();

        let mut observed: Vec<(&str, &str)> = Vec::new();
        for (spec, files) in changes.groups() {
            for file in files {
                observed.push((spec.label, file.as_str()));
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
            add: vec![String::new()],
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
