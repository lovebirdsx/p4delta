//! clean 模式：`p4 clean`（`p4 reconcile -w`）的对等实现。
//!
//! 方向与 open 模式**相反** —— 不是拿工作区的改动去更新 depot，而是拿 depot 修正工作区：
//!
//! - `add` 类（工作区有、depot 无）：删掉工作区里的文件
//! - `edit` 类（已改动、未打开）：还原成上次 sync 的版本
//! - `delete` 类（depot 有、工作区缺失）：从 depot 写回上次 sync 的版本
//!
//! 其余五类都是**已打开**的文件。`p4 clean` 完全不碰它们（官方文档原话："files that are
//! opened for add, edit, delete, or integrate are not impacted by p4 clean"），所以
//! [`CleanChanges::project`] 直接丢弃：既不动作，也不汇报。

use std::io;
use std::path::Path;
use std::time::Instant;

use anyhow::{Result, anyhow, bail};

use rayon::prelude::*;

use super::changes::{Changes, render_title, report_group};
use crate::cli::Options;
use crate::model::DepotState;
use crate::p4::process::{FailureMode, run_p4_command_batched};
use crate::path::local_path_key;

/// 把文件还原到 have revision 时下发的 p4 命令。
///
/// `-f` 不能省：文件已经在 have list 上、时间戳也可能被 p4 认为新鲜，不强制就会跳过。
/// 不带 `-K`：`p4 clean` 默认展开 ktext 关键字，`-K` 才抑制，这里对上的是默认行为。
const RESTORE_ARGS: &[&str] = &["sync", "-f"];

/// 要还原到 have revision 的文件。
///
/// [`Changes`] 里只存了 client 路径，还原需要的 depot 路径与 have revision 得回 depot
/// 记录里取。
#[derive(Debug, PartialEq)]
pub(crate) struct RestoreFile {
    /// workspace 语法，`-l` 清单显示用。
    client_file: String,

    /// depot 语法，拼 p4 文件规格用。
    depot_file: String,

    /// 上次 sync 的版本，拼进规格里钉死。
    have_rev: u32,
}

/// clean 要做的三类动作。
#[derive(Debug)]
pub(crate) struct CleanChanges {
    /// 工作区有、depot 无：删掉工作区里的文件。
    delete: Vec<String>,

    /// 已改动、未打开：还原成 have revision。
    revert: Vec<RestoreFile>,

    /// depot 有、工作区缺失：从 depot 写回 have revision。
    restore: Vec<RestoreFile>,
}

/// 一类变更要执行的动作。
#[derive(Debug, Clone, Copy, PartialEq)]
enum CleanAction {
    /// 删掉工作区里的文件（`add` 类）。
    DeleteFromDisk,

    /// 用 `p4 sync -f` 写回 have revision（`edit` / `delete` 类）。
    RestoreToHave,
}

/// 一类变更在报告与执行上的全部差异。
struct CleanGroupSpec {
    /// `-l` 清单里每行的前缀，例如 `Delete` / `Revert`。
    label: &'static str,

    /// 标题模板，`{}` 由文件数量填充。前导空格是输出契约的一部分。
    title: &'static str,

    action: CleanAction,
}

/// 三类变更的处理方式。顺序即输出顺序，必须与 [CleanChanges::groups] 一一对应。
///
/// 文案刻意与 open 模式区分开：标题若写 "Adding 3 files" 而实际动作是**删掉**它们，
/// 在一个会毁数据的工具上就是主动误导。
const CLEAN_GROUPS: [CleanGroupSpec; 3] = [
    CleanGroupSpec {
        label: "Delete",
        title: "      Deleting {} files in workspace, not in depot or deleted at have revision, but not checked out for add.",
        action: CleanAction::DeleteFromDisk,
    },
    CleanGroupSpec {
        label: "Revert",
        title: "      Reverting {} files in workspace, changed from have revision, but not checked out for edit.",
        action: CleanAction::RestoreToHave,
    },
    CleanGroupSpec {
        label: "Restore",
        title: "      Restoring {} files not in workspace, but not checked out for delete.",
        action: CleanAction::RestoreToHave,
    },
];

/// 一类变更名下的文件。两类的元素类型不同（要删的路径 vs 要还原的规格），
/// 用一个枚举把报告需要的公共部分取出来。
enum CleanGroupFiles<'a> {
    Delete(&'a [String]),
    Restore(&'a [RestoreFile]),
}

impl<'a> CleanGroupFiles<'a> {
    fn len(&self) -> usize {
        match self {
            CleanGroupFiles::Delete(files) => files.len(),
            CleanGroupFiles::Restore(files) => files.len(),
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `-l` 清单里的名字：一律是 client 路径，与 `p4 clean -l` 的 local syntax 口径一致。
    ///
    /// 返回的借用直接指向 `'a`（`CleanChanges`）而不是 `&self`：调用方常在遍历 `groups()`
    /// 的闭包里用这个名字，那里拿到的 `CleanGroupFiles` 是临时值。
    fn client_files(&self) -> Vec<&'a str> {
        match *self {
            CleanGroupFiles::Delete(files) => files.iter().map(String::as_str).collect(),
            CleanGroupFiles::Restore(files) => {
                files.iter().map(|file| file.client_file.as_str()).collect()
            }
        }
    }
}

impl CleanChanges {
    /// 从 open 模式的分类结果投影出 clean 要做的三类动作。
    ///
    /// `add` / `edit` / `delete` 之外的五类都是已打开的文件，`p4 clean` 完全不碰它们，
    /// 所以这里直接丢弃：既不动作，也不汇报。每个文件最多落进一类的不变量由 `analyze`
    /// 保证，这里不需要再去重。
    pub(crate) fn project(changes: &Changes, depot: &DepotState) -> Result<Self> {
        Ok(CleanChanges {
            delete: changes.add.clone(),
            revert: resolve(&changes.edit, depot)?,
            restore: resolve(&changes.delete, depot)?,
        })
    }

    /// 按 [CLEAN_GROUPS] 的顺序把每一类与它的文件配成对。
    fn groups(&self) -> impl Iterator<Item = (&'static CleanGroupSpec, CleanGroupFiles<'_>)> {
        CLEAN_GROUPS.iter().zip([
            CleanGroupFiles::Delete(self.delete.as_slice()),
            CleanGroupFiles::Restore(self.revert.as_slice()),
            CleanGroupFiles::Restore(self.restore.as_slice()),
        ])
    }

    /// 待清理的文件总数。
    pub(crate) fn total(&self) -> usize {
        self.delete.len() + self.revert.len() + self.restore.len()
    }
}

/// 把 client 路径翻成 `p4 sync -f` 需要的 depot 规格。
fn resolve(client_files: &[String], depot: &DepotState) -> Result<Vec<RestoreFile>> {
    client_files
        .iter()
        .map(|client_file| {
            let record = depot
                .get_client_record(&local_path_key(client_file))
                .ok_or_else(|| anyhow!("Failed to find depot record for {client_file}"))?;

            // `edit` / `delete` 两类只在 have_rev 有值时才可能被推入（analyze 先挡掉了
            // 没同步过的记录），这里缺失说明分析结果与 depot 状态对不上，响亮失败。
            let have_rev = record
                .have_rev
                .ok_or_else(|| anyhow!("Missing have revision for {}", record.depot_file))?;

            Ok(RestoreFile {
                client_file: client_file.clone(),
                depot_file: record.depot_file.clone(),
                have_rev,
            })
        })
        .collect()
}

/// `p4 sync` 的文件参数：`//depot/path#<haveRev>`。
///
/// 显式钉住 have revision 是 `-e` / `-d` 的全部要点：不带版本的 `-f` 会同步到 head，
/// 把用户还没同步过的新版本也拉下来，超出 clean 的语义。
fn restore_specs(files: &[RestoreFile]) -> Vec<String> {
    files
        .iter()
        .map(|file| format!("{}#{}", file.depot_file, file.have_rev))
        .collect()
}

/// 删除工作区里的一个条目。符号链接删的是链接本身，不跟随目标。
///
/// 幂等：文件已经不在了（或本来就不存在）算成功——大小写冲突下同一个文件可能被推入
/// 两次，第二次不能算失败。
fn remove_workspace_entry(path: &Path) -> io::Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        result => result?,
    };
    let is_symlink = meta.file_type().is_symlink();

    #[cfg(windows)]
    {
        // Windows 的 DeleteFile 拒绝只读文件，先清掉这一位。这是 Windows 独有的问题：
        // Unix 上删除权限由父目录决定，与文件自身的 mode 无关。
        // 符号链接不能走这步，set_permissions 会跟随链接改到目标文件的权限上去。
        #[allow(clippy::permissions_set_readonly_false)] // Windows 的只读位不是 Unix 的 mode
        if !is_symlink && meta.permissions().readonly() {
            let mut permissions = meta.permissions();
            permissions.set_readonly(false);
            std::fs::set_permissions(path, permissions)?;
        }
    }

    match std::fs::remove_file(path) {
        // 已经不在了：目标状态已达成。
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        // 指向目录的符号链接：symlink_metadata 的 is_dir() 是 false，而 Windows 上
        // 只有 RemoveDirectory 删得掉它（Unix 上 unlink 已经成功了，走不到这儿）。
        Err(_) if is_symlink => std::fs::remove_dir(path),
        result => result,
    }
}

/// 从磁盘删除 `add` 类的文件。
///
/// 全部尝试完再报错：一个文件删不掉（编辑器占着、权限不对）不该让其余几百个留在原地。
fn delete_workspace_files(files: &[String]) -> Result<()> {
    // par_iter 是 indexed，filter_map 保序，失败清单的顺序因此与扫描顺序一致。
    let failures: Vec<String> = files
        .par_iter()
        .filter_map(|file| {
            remove_workspace_entry(Path::new(file))
                .err()
                .map(|error| format!("\n  {file}: {error}"))
        })
        .collect();

    if failures.is_empty() {
        return Ok(());
    }

    bail!(
        "Failed to delete {} file(s):{}",
        failures.len(),
        failures.join("")
    )
}

/// 把一批文件写回它们的 have revision。
///
/// 传 depot 路径而不是 client 路径：have revision 本来就长在 depot 记录上，而且同一个
/// client 路径在大小写冲突下可能对应两条记录，depot 路径没有歧义。
async fn restore_to_have(options: &Options, work_dir: &str, files: &[RestoreFile]) -> Result<()> {
    let specs = restore_specs(files);

    // 不带 changelist：clean 不产生 changelist，`p4 sync` 也没有意义。
    // 只在 -a 时被调用，所以永远是「真改状态」：sync 失败必须让整轮失败，
    // 否则工作区会停在一个既没还原干净、也没人知道的状态上。
    // 判据同 [`crate::reconcile::changes::apply_changes`]：p4 的逐文件错误不改退出码。
    // 错误由 [`apply_clean`] 收集，等其余各类做完再一起报。
    run_p4_command_batched(
        options,
        work_dir,
        RESTORE_ARGS,
        &specs,
        false,
        FailureMode::ExitCodeOrStderr,
    )
    .await?;

    Ok(())
}

/// 报告 clean 的三类变更，并在 `-a` 时执行它们。
pub(crate) async fn apply_clean(
    options: &Options,
    work_dir: &str,
    clean: &CleanChanges,
) -> Result<()> {
    if options.apply {
        println!("   Cleaning the workspace to match the depot.");
        // 走 stdout 而不是 stderr：它是报告的一部分，和下面的标题、清单交织成一段话；
        // stderr 在这个项目里只用于「运行中出了意外」。
        println!("      WARNING: this deletes files that are not in the depot and discards local");
        println!("      changes to files that are not opened. It cannot be undone.");
    } else {
        println!("   Counting files to clean (dry run).")
    }

    let start_time = Instant::now();
    let mut failures: Vec<String> = Vec::new();

    for (spec, files) in clean.groups() {
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

        // 一类失败不拦下其余的类：一个文件删不掉（编辑器占着、权限不对）或还原不了，
        // 不该让另外几百个留在原地。做完能做的，最后一起报。
        let result = match (spec.action, &files) {
            (CleanAction::DeleteFromDisk, CleanGroupFiles::Delete(files)) => {
                delete_workspace_files(files)
            }
            (CleanAction::RestoreToHave, CleanGroupFiles::Restore(files)) => {
                restore_to_have(options, work_dir, files).await
            }
            // 表与字段的配对由 `every_group_is_paired_with_the_matching_action` 钉死，
            // 走不到这里；真走到了说明表被改坏了，不能静默按其中一边执行。
            (action, _) => bail!(
                "Mispaired clean group \"{}\" with action {action:?}",
                spec.label
            ),
        };

        if let Err(error) = result {
            failures.push(format!("{} ({} files): {error}", spec.label, files.len()));
        }
    }

    let total = clean.total();
    if options.apply {
        // 有失败就绝不打印 "Workspace matches the depot."——那是这个工具唯一的安全承诺。
        if !failures.is_empty() {
            bail!(
                "Failed to clean {} change group(s):\n  {}",
                failures.len(),
                failures.join("\n  ")
            );
        }

        println!(
            "      Cleaned {total} files in {} seconds.",
            start_time.elapsed().as_secs_f32()
        );
        println!("Workspace matches the depot.");
    } else {
        println!(
            "      Counted {total} files to clean in {} seconds.",
            start_time.elapsed().as_secs_f32()
        );
        println!("Re-run with -a to clean the workspace.");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::model::DepotFileRecord;
    use crate::test_util::{TempTree, depot_record, symlink_file};

    /// 建一个带好映射的 [DepotState]。两个索引字段是 model 私有的，只能这样填。
    fn depot_with(records: Vec<DepotFileRecord>) -> DepotState {
        let mut depot = DepotState::default();
        depot.file_records = records;
        depot.build_mapping();
        depot
    }

    /// 造一条「已同步、可还原」的记录。
    fn restorable(client_file: &str, depot_file: &str, have_rev: u32) -> DepotFileRecord {
        DepotFileRecord {
            depot_file: depot_file.to_owned(),
            have_rev: Some(have_rev),
            ..depot_record(client_file)
        }
    }

    fn restore_file(client_file: &str, depot_file: &str, have_rev: u32) -> RestoreFile {
        RestoreFile {
            client_file: client_file.to_owned(),
            depot_file: depot_file.to_owned(),
            have_rev,
        }
    }

    /// 三类各放一个可区分的文件名，用来验证标签与文件的配对。
    fn clean_with_one_file_each() -> CleanChanges {
        CleanChanges {
            delete: vec!["delete.txt".to_owned()],
            revert: vec![restore_file("revert.txt", "//depot/revert.txt", 1)],
            restore: vec![restore_file("restore.txt", "//depot/restore.txt", 2)],
        }
    }

    /// 八类各放一个可区分的文件名。
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

    // ---- 表与字段的配对 ----

    /// [CLEAN_GROUPS] 与 [CleanChanges] 字段的对应关系全靠人工维护：条目数量对不上会
    /// 编译失败，但两条**对调**不会——那只会让标题、清单与实际动作错配。这里钉死整张表。
    #[test]
    fn clean_group_order_matches_the_three_clean_fields() {
        let clean = clean_with_one_file_each();

        let observed: Vec<(&str, Vec<&str>)> = clean
            .groups()
            .map(|(spec, files)| (spec.label, files.client_files()))
            .collect();

        assert_eq!(
            observed,
            vec![
                ("Delete", vec!["delete.txt"]),
                ("Revert", vec!["revert.txt"]),
                ("Restore", vec!["restore.txt"]),
            ]
        );
    }

    /// 动作与字段类型必须一一配齐。配错了 `apply_clean` 会走 `bail!` 分支中止整轮，
    /// 所以这里正向证明那个分支到不了。
    #[test]
    fn every_group_is_paired_with_the_matching_action() {
        for (spec, files) in clean_with_one_file_each().groups() {
            match (spec.action, &files) {
                (CleanAction::DeleteFromDisk, CleanGroupFiles::Delete(_)) => {}
                (CleanAction::RestoreToHave, CleanGroupFiles::Restore(_)) => {}
                (action, _) => panic!("{}: mispaired with {action:?}", spec.label),
            }
        }
    }

    #[test]
    fn every_title_has_one_placeholder_and_the_output_indent() {
        for spec in &CLEAN_GROUPS {
            assert_eq!(spec.title.matches("{}").count(), 1, "{}", spec.label);
            // 前导空格是输出契约的一部分：标题要和 --list 的清单对齐。
            assert!(spec.title.starts_with("      "), "{}", spec.label);

            let rendered = render_title(spec.title, 3);
            assert!(!rendered.contains("{}"), "{}", spec.label);
            assert!(rendered.contains("3 files"), "{}: {rendered}", spec.label);
        }
    }

    /// clean 只下发 `p4 sync`，不需要任何 open 命令——`add` 类走文件系统删除。
    #[test]
    fn clean_never_uses_an_open_command() {
        assert_eq!(RESTORE_ARGS, ["sync", "-f"].as_slice());

        // `-K` 抑制 ktext 关键字展开，而 `p4 clean` 默认展开（`-K` 才抑制），
        // 所以刻意不带它——对上的是 p4 clean 的默认行为。
        assert!(!RESTORE_ARGS.contains(&"-K"));
    }

    // ---- 投影 ----

    /// 核心断言：五类已打开的文件在 clean 下必须完全缺席。
    #[test]
    fn projection_drops_every_opened_group() {
        let depot = depot_with(vec![
            restorable("edit.txt", "//depot/edit.txt", 3),
            restorable("delete.txt", "//depot/delete.txt", 4),
        ]);

        let clean = CleanChanges::project(&changes_with_one_file_each(), &depot).unwrap();

        assert_eq!(clean.total(), 3);
        assert_eq!(clean.delete, ["add.txt"]);
        assert_eq!(
            clean.revert,
            [restore_file("edit.txt", "//depot/edit.txt", 3)]
        );
        assert_eq!(
            clean.restore,
            [restore_file("delete.txt", "//depot/delete.txt", 4)]
        );
    }

    #[test]
    fn projection_fails_loudly_when_the_record_is_missing() {
        let error = CleanChanges::project(&changes_with_one_file_each(), &depot_with(vec![]))
            .expect_err("edit.txt 没有 depot 记录，必须报错");

        assert!(
            error.to_string().contains("edit.txt"),
            "错误消息该点名文件：{error}"
        );
    }

    #[test]
    fn projection_fails_loudly_without_a_have_revision() {
        // 记录在，但从没同步过：clean 无从知道该还原到哪个版本。
        let depot = depot_with(vec![depot_record("edit.txt")]);

        let error = CleanChanges::project(&changes_with_one_file_each(), &depot)
            .expect_err("缺少 have revision 时必须报错");

        assert!(
            error.to_string().contains("have revision"),
            "错误消息该说明原因：{error}"
        );
    }

    #[test]
    fn restore_specs_pin_the_have_revision() {
        let specs = restore_specs(&[
            restore_file("a.txt", "//depot/a.txt", 3),
            restore_file("b.txt", "//depot/b.txt", 7),
        ]);

        // 钉的是 have 而不是 head：不带版本的 -f 会把没同步过的新版本拉下来。
        assert_eq!(specs, ["//depot/a.txt#3", "//depot/b.txt#7"]);
    }

    // ---- 删除 ----

    #[test]
    fn deleting_a_workspace_file_removes_it() {
        let tree = TempTree::new("clean-delete");
        let file = tree.file("a.txt", "x");

        remove_workspace_entry(&file).unwrap();

        assert!(!file.exists());
    }

    /// 幂等：大小写冲突下同一个文件可能被推入两次，第二次不能算失败。
    #[test]
    fn deleting_a_missing_file_is_not_an_error() {
        let tree = TempTree::new("clean-delete-missing");

        remove_workspace_entry(&tree.root.join("never-existed.txt")).unwrap();
    }

    #[test]
    fn a_read_only_file_is_deleted() {
        let tree = TempTree::new("clean-delete-read-only");
        let file = tree.file("a.txt", "x");

        let mut permissions = std::fs::metadata(&file).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&file, permissions).unwrap();

        remove_workspace_entry(&file).unwrap();

        assert!(!file.exists());
    }

    #[test]
    fn deleting_a_symlink_removes_the_link_not_the_target() {
        let tree = TempTree::new("clean-delete-symlink");
        let target = tree.file("target.txt", "contents");
        let link = tree.root.join("link.txt");

        if let Err(error) = symlink_file(&target, &link) {
            eprintln!("skipping: this system does not allow symlinks: {error}");
            return;
        }

        remove_workspace_entry(&link).unwrap();

        assert!(!link.exists(), "链接本身该被删掉");
        assert!(target.exists(), "目标不该被碰");
    }

    /// 一个文件删不掉不该让其余文件留在原地——全部尝试完再报错，并逐个点名。
    #[test]
    fn every_file_is_attempted_even_when_one_fails() {
        let tree = TempTree::new("clean-delete-partial");
        let first = tree.file("first.txt", "x");
        let second = tree.file("second.txt", "x");
        // 拿一个非空目录冒充要删的文件：symlink_metadata 成功，remove_file 必定失败。
        let blocker = tree.dir("blocker");
        std::fs::write(blocker.join("inside.txt"), "x").unwrap();

        let files = vec![
            first.display().to_string(),
            blocker.display().to_string(),
            second.display().to_string(),
        ];

        let error = delete_workspace_files(&files).expect_err("非空目录删不掉，必须报错");

        let message = error.to_string();
        assert!(
            message.contains("Failed to delete 1 file(s)"),
            "该报告恰好一个失败：{message}"
        );
        assert!(message.contains("blocker"), "该点名失败的路径：{message}");
        assert!(!first.exists(), "失败之前的文件该已删除");
        assert!(!second.exists(), "失败之后的文件也该被尝试");
    }
}
