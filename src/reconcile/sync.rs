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

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use anyhow::{Result, anyhow, bail};

use super::analyze::{SyncAnalysis, SyncSource, analyze_at_target, guard_case_conflicts};
use super::changes::{GroupReport, GroupRow, render_title, report_group};
use super::{HashStats, delete_workspace_files};
use crate::cache::{CacheWriter, save_cache};
use crate::cli::Options;
use crate::digest::{CachePolicy, is_unchanged_since_sync, parallel_compute_digests};
use crate::json::{
    HandoffFile, Mode, P4Command, emit_progress, ignore_query_count, say_phase, sayln,
};
use crate::model::{
    DepotFileRecord, DepotState, HaveRecord, TargetMap, WorkspaceCache, WorkspaceState,
};
use crate::p4::process::{FailureMode, run_p4_command_batched};
use crate::path::local_path_key;
use crate::scope::SCOPE_FILE_NAME;
use crate::workspace::{IgnoreQueryMode, MappedPath, map_paths_strict, query_ignored_paths};

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

/// 要删掉的未跟踪文件：depot 里没有记录，但 client view 能映射到它。
///
/// 与 [`DeleteFile`] 的区别是**没有 have 记录可清**：这些路径从来不在 depot 里，所以
/// 只删磁盘，绝不下发 `#none`。
#[derive(Debug, PartialEq)]
pub(crate) struct UntrackedFile {
    /// workspace 语法，删文件与 `-l` 清单显示用。
    client_file: String,

    /// depot 语法。仅用于记录与人工核对——删除本身不需要 p4。
    depot_file: String,
}

/// 一类变更要执行的动作。
#[derive(Debug, Clone, Copy, PartialEq)]
enum SyncAction {
    /// `p4 sync -f //depot/f#<rev>`，把文件拉到目标版本。
    SyncToTarget,

    /// 删掉工作区里的文件（目标时刻该路径不在库）。
    DeleteFromDisk,

    /// 删掉 depot 不跟踪、client view 能映射、且未被 p4ignore 保护的本地文件。
    DeleteUntracked,
}

/// 一类变更在报告与执行上的全部差异。
struct SyncGroupSpec {
    /// JSON 记录的 `class`，见 `docs/json-contract.md`。
    id: &'static str,

    /// `-l` 清单里每行的前缀，例如 `Update` / `Revert`。
    label: &'static str,

    /// 标题模板，`{}` 由文件数量填充。前导空格是输出契约的一部分。
    title: &'static str,

    action: SyncAction,
}

/// 五类变更的处理方式。顺序即输出顺序，必须与 [SyncChanges::groups] 一一对应。
///
/// Update 与 Revert 分开是刻意的：前者是「本地落后了，正常拉取」，后者是「你在本地改了
/// 一个没打开编辑的文件，改动会被丢弃」——风险等级不同，混在一组就是在误导
/// （立场同 `clean.rs` 里那段「标题与动作必须一致」的注释）。
///
/// Untracked 只在 `--delete-untracked` 下非空：它删的不是 depot 在这个目标版本上的决定，
/// 而是用户自己放进工作区、p4 从来不管的东西，所以单列一组、单独一句警告。
const SYNC_GROUPS: [SyncGroupSpec; 5] = [
    SyncGroupSpec {
        id: "update",
        label: "Update",
        title: "      Updating {} files in workspace to the target depot revision.",
        action: SyncAction::SyncToTarget,
    },
    SyncGroupSpec {
        id: "revert",
        label: "Revert",
        title: "      Reverting {} files in workspace, changed from the target revision, but not checked out for edit.",
        action: SyncAction::SyncToTarget,
    },
    SyncGroupSpec {
        id: "restore",
        label: "Restore",
        title: "      Restoring {} files not in workspace, at the target depot revision.",
        action: SyncAction::SyncToTarget,
    },
    // 措辞不与 clean 的 Delete 共用「deleted at have revision」那套：sync 删的是「目标时刻
    // 该路径不在库」的文件，其中既可能是目标处已是删除版本，也可能是（`--to <CL>` 下）
    // 目标时刻它还没进 depot。「not present」把两种都盖住了。
    SyncGroupSpec {
        id: "delete",
        label: "Delete",
        title: "      Deleting {} files in workspace, not present in the depot at the target revision.",
        action: SyncAction::DeleteFromDisk,
    },
    SyncGroupSpec {
        id: "untracked",
        label: "Untracked",
        title: "      Deleting {} untracked files from the workspace.",
        action: SyncAction::DeleteUntracked,
    },
];

/// 一类变更名下的文件。两类的元素类型不同（要同步的规格 vs 要删的路径），
/// 用一个枚举把报告需要的公共部分取出来。
enum SyncGroupFiles<'a> {
    Sync(&'a [SyncFile]),
    Delete(&'a [DeleteFile]),
    Untracked(&'a [UntrackedFile]),
}

impl<'a> SyncGroupFiles<'a> {
    fn len(&self) -> usize {
        match self {
            SyncGroupFiles::Sync(files) => files.len(),
            SyncGroupFiles::Delete(files) => files.len(),
            SyncGroupFiles::Untracked(files) => files.len(),
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 这一组要报告的行：两个路径都在元素上（要同步的那类还带着目标修订）。
    ///
    /// 借用指向 `'a`（`SyncChanges`）而不是 `&self`：调用方常在遍历 `groups()` 的闭包里
    /// 用它，那里拿到的 `SyncGroupFiles` 是临时值。
    ///
    /// 删除组没有 `rev`：那一行的意思是「目标时刻它不该在」，没有一个版本号说得通。
    fn rows(&self) -> Vec<GroupRow<'a>> {
        match self {
            SyncGroupFiles::Sync(files) => files
                .iter()
                .map(|file| GroupRow {
                    client_file: file.client_file.as_str(),
                    depot_file: file.depot_file.as_str(),
                    rev: Some(file.target_rev),
                })
                .collect(),
            SyncGroupFiles::Delete(files) => files
                .iter()
                .map(|file| GroupRow {
                    client_file: file.client_file.as_str(),
                    depot_file: file.depot_file.as_str(),
                    rev: None,
                })
                .collect(),
            SyncGroupFiles::Untracked(files) => files
                .iter()
                .map(|file| GroupRow {
                    client_file: file.client_file.as_str(),
                    depot_file: file.depot_file.as_str(),
                    rev: None,
                })
                .collect(),
        }
    }
}

/// sync 要做的五类动作。
#[derive(Debug, Default)]
pub(crate) struct SyncChanges {
    /// 本地有一份、但不是目标版本：拉到目标版本覆盖它。
    update: Vec<SyncFile>,

    /// 目标版本没变、本地内容却不是它了：还原回去。
    revert: Vec<SyncFile>,

    /// 本地没有：写回来。
    restore: Vec<SyncFile>,

    /// 目标时刻该路径不在库、本地却有：删掉。
    delete: Vec<DeleteFile>,

    /// `--delete-untracked`：depot 不跟踪、client view 能映射、p4ignore 不保护的本地文件。
    untracked: Vec<UntrackedFile>,
}

impl SyncChanges {
    /// 从分类结果与摘要结果投影出要下发的各类动作。
    ///
    /// `drifted` 是摘要判定为「不是目标版本」的那批，由调用方在算完摘要后传入：
    /// 摘要是 [`SyncAnalysis::check`] 的后续，这里只负责拼装。
    ///
    /// `untracked` 由调用方在扫盘之后单独发现（见 [`discover_untracked`]）：它不属于
    /// depot 与工作区的差异分析，而是「depot 根本不管」的那批。
    pub(crate) fn project(
        analysis: &SyncAnalysis<'_>,
        drifted: Vec<SyncSource<'_>>,
        untracked: Vec<UntrackedFile>,
    ) -> Self {
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
            untracked,
        }
    }

    /// 按 [SYNC_GROUPS] 的顺序把每一类与它的文件配成对。
    fn groups(&self) -> impl Iterator<Item = (&'static SyncGroupSpec, SyncGroupFiles<'_>)> {
        SYNC_GROUPS.iter().zip([
            SyncGroupFiles::Sync(self.update.as_slice()),
            SyncGroupFiles::Sync(self.revert.as_slice()),
            SyncGroupFiles::Sync(self.restore.as_slice()),
            SyncGroupFiles::Delete(self.delete.as_slice()),
            SyncGroupFiles::Untracked(self.untracked.as_slice()),
        ])
    }

    pub(crate) fn total(&self) -> usize {
        self.update.len()
            + self.revert.len()
            + self.restore.len()
            + self.delete.len()
            + self.untracked.len()
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
            P4Command::Sync,
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

/// 从 p4 的逐条应答里找出**没有**被回应的规格。
///
/// p4 对每条被它处理的规格打一行 `<规格> - <动作> <本地路径>`。规格名本身也可能含
/// `" - "`（例如 `//depot/a - b.txt#3`），所以不能在第一处就切：逐处试，取**最后**一个
/// 恰好等于已知规格的前缀。规格总在行首、后面紧跟分隔符；本地路径在它之后，那里切出来
/// 的前缀不会命中规格集合。
///
/// 有输出却一条都认不出时返回 `Err`：那说明输出形状与这里的假设不符（p4 换了版本或
/// 说法）。此时若把它当成「全都没同步」去补发，只会让整轮白跑一遍，而且下一轮还会同样
/// 白跑；直接失败，让问题落在「看不懂 p4 的输出」上。
fn missing_specs(specs: &[String], lines: &[String]) -> Result<Vec<String>> {
    let known: HashSet<&str> = specs.iter().map(String::as_str).collect();
    let mut acknowledged: HashSet<&str> = HashSet::new();

    for line in lines {
        let mut matched = None;
        for (at, _) in line.match_indices(" - ") {
            let candidate = &line[..at];
            if known.contains(candidate) {
                matched = Some(candidate);
            }
        }
        if let Some(spec) = matched {
            acknowledged.insert(spec);
        }
    }

    if acknowledged.is_empty() && !lines.is_empty() {
        bail!(
            "p4 sync wrote {} line(s), but none of them matched a requested file spec; p4delta \
             cannot tell which files were synced (the p4 output format may have changed). \
             Nothing was retried.",
            lines.len()
        );
    }

    Ok(specs
        .iter()
        .filter(|spec| !acknowledged.contains(spec.as_str()))
        .cloned()
        .collect())
}

/// 下发一批 `p4 sync` 规格，返回 p4 **没有**逐条应答的规格。
///
/// p4 对每条规格都会打一行 `<规格> - <动作> <本地路径>`。退出码 0 且 stderr 干净并不代表
/// 每条规格都真被动过：规格没送到 p4、或 p4 静默忽略了某几批时，整轮照报成功，而工作区
/// 停在一个既没拉全、也没人知道的状态上（实测遇到过一次：82,485 条规格里 17,215 条对应
/// 的文件根本没有被写，`have` 的 syncTime 还停在几轮同步之前）。
///
/// 输出非空却一条规格都认不出时直接失败，理由见 [`missing_specs`]。
async fn run_p4_sync_specs(
    options: &Options,
    work_dir: &str,
    specs: &[String],
) -> Result<Vec<String>> {
    let lines = run_p4_command_batched(
        options,
        work_dir,
        P4Command::Sync,
        SYNC_ARGS,
        specs,
        false,
        FailureMode::ExitCodeOrStderr,
    )
    .await?;

    missing_specs(specs, &lines)
}

/// 把一批文件拉到它们的目标版本。
///
/// 传 depot 路径而不是 client 路径：目标修订本来就长在 depot 记录上，而且同一个 client
/// 路径在大小写冲突下可能对应两条记录，depot 路径没有歧义。
async fn sync_to_target(options: &Options, work_dir: &str, files: &[SyncFile]) -> Result<()> {
    let specs = sync_specs(files);

    // 不带 changelist：`p4 sync -f` 不开文件、不产生 changelist，`-c` 也就没有意义。
    // 只在 -a 时被调用，所以永远是「真改状态」：sync 失败必须让整轮失败，
    // 否则工作区会停在一个既没拉全、也没人知道的状态上。
    // 判据同 [`crate::reconcile::changes::apply_changes`]：p4 的逐文件错误不改退出码。
    // 错误由 [`apply_sync`] 收集，等其余各类做完再一起报。
    let unacknowledged = run_p4_sync_specs(options, work_dir, &specs).await?;
    sayln!(
        "      p4 acknowledged {} of {} file spec(s).",
        specs.len() - unacknowledged.len(),
        specs.len()
    );

    if unacknowledged.is_empty() {
        return Ok(());
    }

    // 「Synced N files」必须等于 p4 真动过的文件数，所以缺的那些先补一次：只重发没被应答的
    // 规格，p4 这一轮的状态没人知道，重发是幂等的；补完还是不齐，才把这一组报成失败。
    sayln!(
        "      Retrying {} file spec(s) p4 did not act on.",
        unacknowledged.len()
    );
    let still_missing = run_p4_sync_specs(options, work_dir, &unacknowledged).await?;
    if still_missing.is_empty() {
        return Ok(());
    }

    let examples: Vec<&str> = still_missing
        .iter()
        .take(10)
        .map(|spec| spec.as_str())
        .collect();
    bail!(
        "p4 sync exited cleanly but did not act on {} of {} file spec(s), even after retrying \
         them; those files are still at their old revisions. First: {}",
        still_missing.len(),
        specs.len(),
        examples.join(", ")
    );
}

/// 从工作区里删掉一批未跟踪文件。它们没有 have 记录，所以只删磁盘，不下发 `#none`。
async fn delete_untracked_from_workspace(files: &[UntrackedFile]) -> Result<()> {
    let paths: Vec<String> = files.iter().map(|file| file.client_file.clone()).collect();
    delete_workspace_files(&paths)
}

/// 未跟踪核对的纯函数结果。
///
/// 只带报告需要的三个值，不塞任何性能统计字段——遍历次数由测试用可计数迭代器直接观测。
#[derive(Debug, Default, PartialEq)]
struct UntrackedCrossCheck {
    /// depot 里没有记录的映射结果，保持 `mapped` 的原顺序与重复项。
    files: Vec<UntrackedFile>,

    /// `p4 where` 没给出 depot 路径的候选数。
    outside_view: usize,

    /// client view 的排除行挡下的候选数；它们同时计入 [`outside_view`](Self::outside_view)。
    view_excluded: usize,
}

/// 把映射结果与 depot 记录对一遍，留下真正未跟踪的项。
///
/// 复杂度是 O(D + U)：先把候选的原始 depot 路径收进候选规模的 `HashSet`，**单趟**扫过
/// depot 记录把已跟踪项剔除（候选全部命中后立即停），再按映射顺序筛选。旧实现为每个
/// 候选 `any()` 一遍全部记录，候选多时是 O(U×D)。
///
/// 路径按**原始字符串**比较：depot 路径的身份由服务端定义，客户端不自行折叠大小写。
fn cross_check_untracked<'a>(
    mapped: &[MappedPath],
    depot_records: impl IntoIterator<Item = &'a DepotFileRecord>,
) -> UntrackedCrossCheck {
    let mut pending: HashSet<&str> = mapped
        .iter()
        .filter_map(|entry| entry.depot_file.as_deref())
        .collect();

    // 没有可映射的候选（零候选或全被忽略挡掉）时不必碰 depot：那是几百万条记录的固定成本。
    if !pending.is_empty() {
        for record in depot_records {
            pending.remove(record.depot_file.as_str());
            // 候选全部命中后，剩下的 depot 记录不再影响裁决。
            if pending.is_empty() {
                break;
            }
        }
    }

    let mut result = UntrackedCrossCheck::default();
    for entry in mapped {
        if entry.unmapped {
            result.view_excluded += 1;
        }
        let Some(depot_file) = entry.depot_file.as_deref() else {
            result.outside_view += 1;
            continue;
        };
        // 二次核对：映射出的 depot 路径已经有记录，说明前一步的本地键没对上（大小写或
        // 拼写差异）。宁可少删，不可把已跟踪文件当成未跟踪。
        if pending.contains(depot_file) {
            result.files.push(UntrackedFile {
                client_file: entry.client_file.clone(),
                depot_file: depot_file.to_owned(),
            });
        }
    }
    result
}

/// 发现未跟踪文件并做删除前的保护。
///
/// 判据是两条同时成立：`p4 where` 能把它映射进 client view，且 depot 记录里没有它。
/// 只有第一条不够——depot 记录为空时，任何本地文件都会被看成未跟踪；只有第二条也不够——
/// view 之外的文件 p4 根本不看，删它们是在动用户明确排除的东西（比如另一个工具的仓库）。
///
/// 顺序是「先忽略规则、后 where」：被 `.p4ignore` 盖住的候选根本不需要映射查询，而这一步
/// 在几万候选上的省下的是成批子进程。忽略查询仍用严格口径，任何失败整轮中止，绝不把失败
/// 读成「一个都没被忽略」；`p4 where` 失败同样中止。
async fn discover_untracked(
    options: &Options,
    work_dir: &str,
    depot: &DepotState,
    workspace: &WorkspaceState,
) -> Result<Vec<UntrackedFile>> {
    if !options.delete_untracked {
        return Ok(Vec::new());
    }

    if depot.file_records.is_empty() {
        bail!(
            "--delete-untracked refused: p4 returned no depot records at all, so p4delta \
             cannot tell untracked files apart from a broken scope or client mapping. \
             Nothing was deleted."
        );
    }

    // 汇总计时从发现阶段起点开始：候选准备、忽略查询、映射与本地核对都算保护成本。
    let protection_start = Instant::now();

    // 范围文件永远不是删除候选：正常路径下它由 `scope.rs` 的隐式排除挡在扫盘之外，但
    // `--no-scope-file`（用户确认「按所选执行」）根本不会登记那条排除。删掉它会让之后
    // 每一轮都静默退化成「没有范围配置」、范围放大到整个工作区——一次删除换来更大的
    // 后续破坏，所以这里无条件按 client root 下的固定位置剔除。
    let scope_file_key = local_path_key(
        &Path::new(work_dir)
            .join(SCOPE_FILE_NAME)
            .display()
            .to_string(),
    );
    let candidates: Vec<String> = workspace
        .files
        .iter()
        .filter(|file| file.path_lower != scope_file_key)
        .filter(|file| depot.get_client_record(&file.path_lower).is_none())
        .map(|file| file.path.clone())
        .collect();
    let scanned = candidates.len();

    let mut kept = 0;
    let mut queries = 0;
    let mut mapped = Vec::new();

    if !candidates.is_empty() {
        emit_progress(
            "untracked",
            Some("Screening untracked candidates against the ignore rules."),
        );
        let ignores_start = Instant::now();
        let queries_before = ignore_query_count();
        let ignored = query_ignored_paths(
            options,
            work_dir,
            candidates.iter(),
            IgnoreQueryMode::Strict,
        )
        .await?;
        queries = ignore_query_count() - queries_before;
        // 过滤只走一趟：remaining 的长度直接给出 kept，候选字符串原样保留。
        let remaining: Vec<String> = candidates
            .into_iter()
            .filter(|path| !ignored.contains(path))
            .collect();
        kept = scanned - remaining.len();
        say_phase(
            "untracked-ignores",
            &format!(
                "{kept} of {scanned} candidate(s) protected by the ignore rules with {queries} \
                 p4 query/queries"
            ),
            Some(ignores_start.elapsed().as_secs_f32()),
        );
        if kept > 0 {
            sayln!("Kept {kept} p4ignore-protected untracked file(s).");
        }

        if !remaining.is_empty() {
            emit_progress(
                "untracked",
                Some("Mapping untracked candidates into the client view."),
            );
            let where_start = Instant::now();
            mapped = map_paths_strict(options, work_dir, &remaining).await?;
            say_phase(
                "untracked-where",
                &format!("{} candidate(s) mapped", remaining.len()),
                Some(where_start.elapsed().as_secs_f32()),
            );
        }
    }

    emit_progress(
        "untracked",
        Some("Cross-checking mapped candidates against the depot records."),
    );
    let cross_start = Instant::now();
    let checked = cross_check_untracked(&mapped, depot.file_records.iter());
    say_phase(
        "untracked-cross-check",
        &format!(
            "{} mapped path(s) checked; {} outside the client view, {} excluded by the view; \
             {} untracked",
            mapped.len(),
            checked.outside_view,
            checked.view_excluded,
            checked.files.len()
        ),
        Some(cross_start.elapsed().as_secs_f32()),
    );
    if checked.outside_view > 0 {
        sayln!(
            "Skipped {} untracked candidate(s) outside the client view \
             ({} excluded by the view).",
            checked.outside_view,
            checked.view_excluded
        );
    }

    // 零候选、全被忽略、全部已跟踪都要留下这条汇总：否则删除档在长跑里看起来像没跑过，
    // 也分不清「保护花了多久」与「应用花了多久」。
    say_phase(
        "untracked-protection",
        &format!(
            "{scanned} untracked candidate(s) screened against the ignore rules, the client view \
             and the depot with {queries} ignore query/queries, {kept} kept"
        ),
        Some(protection_start.elapsed().as_secs_f32()),
    );

    Ok(checked.files)
}

/// 报告 sync 的五类变更，并在 `-a` 时执行它们。
pub(crate) async fn apply_sync(
    options: &Options,
    work_dir: &str,
    sync: &SyncChanges,
) -> Result<()> {
    emit_progress("apply", None);
    if options.apply {
        sayln!("   Syncing the workspace to the target depot revision.");
    } else {
        sayln!("   Counting files to sync (dry run).")
    }
    // 预演也打这句（clean 只在 -a 时打）：sync 相对 clean 的卖点正是「不删未跟踪的文件」，
    // 而「会覆盖未打开文件的本地改动」是它的代价。授权之前就得看得见代价。
    sayln!(
        "      WARNING: this overwrites local changes to files that are not opened. It cannot be undone."
    );
    if options.delete_untracked {
        sayln!(
            "      WARNING: --delete-untracked also removes local files the depot does not track \
             (p4ignore-protected files are kept). It cannot be undone."
        );
    }

    let start_time = Instant::now();
    let mut failures: Vec<String> = Vec::new();
    let mut acted_groups = 0usize;

    for (spec, files) in sync.groups() {
        if files.is_empty() {
            continue;
        }
        acted_groups += 1;

        report_group(
            &GroupReport {
                mode: Mode::Sync,
                id: spec.id,
                label: spec.label,
                title: &render_title(spec.title, files.len()),
                list: options.list,
                applied: options.apply,
                stage: None,
            },
            &files.rows(),
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
            (SyncAction::DeleteUntracked, SyncGroupFiles::Untracked(files)) => {
                delete_untracked_from_workspace(files).await
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

        sayln!(
            "      Synced {total} files in {} seconds.",
            start_time.elapsed().as_secs_f32()
        );
        say_phase(
            "apply",
            &format!("synced {total} file(s) across {acted_groups} group(s)"),
            Some(start_time.elapsed().as_secs_f32()),
        );
        // 只有 --verify-all 才敢说这句话：默认档靠 mtime 捷径与摘要缓存跳过了一部分文件，
        // 那些文件的「没变」是推断出来的，没被验证过。承诺必须与档位匹配。
        if options.verify_all {
            sayln!("The synced files match the target depot revision.");
        }
    } else {
        sayln!(
            "      Counted {total} files to sync in {} seconds.",
            start_time.elapsed().as_secs_f32()
        );
        say_phase(
            "apply",
            &format!("dry run: counted {total} file(s) to sync across {acted_groups} group(s)"),
            Some(start_time.elapsed().as_secs_f32()),
        );
        sayln!("Re-run with -a to sync the workspace.");
    }

    Ok(())
}

/// sync 一轮分析的产出：要下发的动作，加上两类不由分组报告承担的文件。
pub(crate) struct SyncPlan {
    pub(crate) changes: SyncChanges,

    /// 算不出摘要的文件，转交原生 `p4 sync` 处理。
    ///
    /// 交给调用方而不是在这里处理：那是一次真正的动作（预演时还要加 `-n`），
    /// 得和其余转交点一起走 `run_p4_command_batched` 那套失败判据。
    ///
    /// 两个路径都带着：下发命令只用 depot 语法（与其余转交点同一口径），而记录里的
    /// `clientFile` 只有本地路径拼得出来——client 语法要从它翻（见 [`HandoffFile`]）。
    pub(crate) unsupported: Vec<HandoffFile>,
}

/// 删除候选的保护：只对**这一批**候选查一次忽略规则，被规则盖着的从组里摘掉。
///
/// 强制修复不对整个工作区做忽略过滤（成本与理由见 [`crate::workspace::gather_workspace`]），
/// 但删除是唯一不可逆的一步，而 `p4 sync -f` 自己不看 `.p4ignore`：不查这一下，用户特意
/// 放在那儿、还没入库的东西会跟着目标态一起被删掉。查询范围因此从「工作区有多大」缩到
/// 「这一轮要删多少」。
///
/// 失败/输出不可靠时 [`IgnoreQueryMode::Strict`] 直接失败（fail-closed）：那条路上的空结果
/// 会被读成「一个都没被忽略」，而它的结论是「删不删」。摘掉的候选连 have 记录都不动
/// （它们不进删除组，`#none` 那一步自然不会发），用户那份文件从此照旧受工具管理。
async fn protect_delete_candidates(
    options: &Options,
    work_dir: &str,
    analysis: &mut SyncAnalysis<'_>,
) -> Result<()> {
    if analysis.delete.is_empty() {
        return Ok(());
    }

    let candidates = analysis.delete.len();
    let start_time = Instant::now();
    let queries_before = ignore_query_count();
    let ignored = query_ignored_paths(
        options,
        work_dir,
        analysis.delete.iter().map(|entry| &entry.file.path),
        IgnoreQueryMode::Strict,
    )
    .await?;
    let queries = ignore_query_count() - queries_before;

    let before = analysis.delete.len();
    analysis
        .delete
        .retain(|entry| !ignored.contains(&entry.file.path));
    let kept = before - analysis.delete.len();

    if kept > 0 {
        sayln!("Kept {kept} file(s) that the ignore rules cover out of the delete group.");
    }
    // 这一条的三个数就是「全盘过滤省了什么、又留下了什么风险」的全部答案：候选有多窄、
    // 为此付了几次 p4 查询、有几份文件靠着答案活下来。
    say_phase(
        "delete-protection",
        &format!(
            "{candidates} delete candidate(s) checked against the ignore rules \
             with {queries} p4 query/queries, {kept} kept"
        ),
        Some(start_time.elapsed().as_secs_f32()),
    );

    Ok(())
}

/// sync 模式的分析管线：按目标版本分类 → 保护删除候选 → 只对「目标版本没变」的文件算摘要
/// → 投影成动作。
///
/// 参数多了 `work_dir`（删除候选的保护要起 p4 子进程，client root 是运行时解析出来的）与
/// `cache` / `cache_writer` 这一对：后者不是可以合并的重复——一个是摘要表、一个是它的落盘
/// 句柄，生命周期各自独立，裹进结构体只会把借用关系藏起来。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn build_sync_changes(
    options: &Options,
    work_dir: &str,
    depot: &DepotState,
    workspace: &WorkspaceState,
    have_records: &HashMap<String, HaveRecord>,
    target: &TargetMap,
    cache: &mut WorkspaceCache,
    cache_writer: &mut Option<CacheWriter>,
) -> Result<SyncPlan> {
    emit_progress("analyze", None);
    sayln!("   Analyzing files against the target depot revision.");
    let start_time = Instant::now();
    let mut analysis = analyze_at_target(depot, workspace, target)?;
    sayln!(
        "      Analysis complete in {} seconds.",
        start_time.elapsed().as_secs_f32()
    );
    say_phase(
        "analyze",
        &format!(
            "{} deletion candidate(s), {} update(s), {} check candidate(s), {} archived",
            analysis.delete.len(),
            analysis.update.len(),
            analysis.check.len(),
            analysis.archived.len()
        ),
        Some(start_time.elapsed().as_secs_f32()),
    );

    if !analysis.archived.is_empty() {
        sayln!(
            "Skipped {} archived file(s): their contents live in an archive depot, so there is nothing to compare.",
            analysis.archived.len()
        );
    }

    // 顺序要紧：先把被忽略规则盖着的删除候选摘掉，再让大小写守卫对着**剩下的**候选判定
    // （摘掉的不再删，也就构不成误删），最后才是那几句可见度提示——它们数的是真要动的文件。
    protect_delete_candidates(options, work_dir, &mut analysis).await?;
    guard_case_conflicts(&analysis, depot, target)?;
    let untracked = discover_untracked(options, work_dir, depot, workspace).await?;

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
        sayln!(
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
        sayln!(
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
            sayln!(
                "   Timestamp optimization: Skipped {} of {} digest computations ({}%).",
                skipped.len(),
                total_candidates,
                percentage
            );
        }

        if !needs_digest.is_empty() {
            let message = format!("Checking digests for {} files.", needs_digest.len());
            sayln!("   {message}");
            emit_progress("digest", Some(&message));

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
                        sayln!("         File \"{}\" digest is wrong.", outcome.file.path);
                    }
                }
            }

            hashed.report();
        }
    }

    Ok(SyncPlan {
        changes: SyncChanges::project(&analysis, drifted, untracked),
        unsupported: analysis
            .unsupported
            .iter()
            .map(|record| HandoffFile {
                depot_file: record.depot_file.clone(),
                client_file: record.client_file.clone(),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;
    use std::time::UNIX_EPOCH;

    use async_global_executor as task;
    use clap::Parser;

    use crate::model::{DepotFileRecord, WorkspaceFile};
    use crate::path::local_path_key;
    use crate::reconcile::analyze::DeletedAtTarget;
    use crate::scope::SCOPE_FILE_NAME;

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

    fn untracked_file(client_file: &str, depot_file: &str) -> UntrackedFile {
        UntrackedFile {
            client_file: client_file.to_owned(),
            depot_file: depot_file.to_owned(),
        }
    }

    fn mapped(client_file: &str, depot_file: Option<&str>, unmapped: bool) -> MappedPath {
        MappedPath {
            client_file: client_file.to_owned(),
            depot_file: depot_file.map(str::to_owned),
            unmapped,
        }
    }

    /// 一条够用的 depot 记录：`SyncSource` 与投影只用到路径字段。
    fn record(client_file: &str, depot_file: &str) -> DepotFileRecord {
        DepotFileRecord {
            client_file: client_file.to_owned(),
            client_file_lower: local_path_key(client_file),
            depot_file: depot_file.to_owned(),
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

    /// 五类各放一个可区分的文件名，用来验证标签与文件的配对。
    fn sync_with_one_file_each() -> SyncChanges {
        SyncChanges {
            update: vec![sync_file("update.txt", "//depot/update.txt", 2)],
            revert: vec![sync_file("revert.txt", "//depot/revert.txt", 3)],
            restore: vec![sync_file("restore.txt", "//depot/restore.txt", 4)],
            delete: vec![delete_file("delete.txt", "//depot/delete.txt")],
            untracked: vec![untracked_file("junk.log", "//depot/junk.log")],
        }
    }

    // ---- 表与字段的配对 ----

    /// [SYNC_GROUPS] 与 [SyncChanges] 字段的对应关系全靠人工维护：条目数量对不上会
    /// 编译失败，但两条**对调**不会——那只会让标题、清单与实际动作错配。这里钉死整张表。
    #[test]
    fn sync_group_order_matches_the_five_sync_fields() {
        let sync = sync_with_one_file_each();

        let observed: Vec<(&str, Vec<String>)> = sync
            .groups()
            .map(|(spec, files)| {
                (
                    spec.label,
                    files
                        .rows()
                        .iter()
                        .map(|row| row.client_file.to_owned())
                        .collect(),
                )
            })
            .collect();

        assert_eq!(
            observed,
            vec![
                ("Update", vec!["update.txt".to_owned()]),
                ("Revert", vec!["revert.txt".to_owned()]),
                ("Restore", vec!["restore.txt".to_owned()]),
                ("Delete", vec!["delete.txt".to_owned()]),
                ("Untracked", vec!["junk.log".to_owned()]),
            ]
        );
        assert_eq!(sync.total(), 5);
    }

    /// 每一行都带齐两个路径；`rev` 只有删除组与未跟踪组缺席——那两行的意思是「它不该在」，
    /// 没有一个版本号说得通。
    #[test]
    fn every_sync_row_carries_both_paths() {
        for (spec, files) in sync_with_one_file_each().groups() {
            let rows = files.rows();
            assert_eq!(rows.len(), 1, "{}", spec.label);
            assert!(
                rows[0].depot_file.starts_with("//depot/"),
                "{}: {}",
                spec.label,
                rows[0].depot_file
            );

            let expected_rev = match spec.id {
                "update" => Some(2),
                "revert" => Some(3),
                "restore" => Some(4),
                _ => None,
            };
            assert_eq!(rows[0].rev, expected_rev, "{}", spec.label);
        }
    }

    /// 动作与字段类型必须一一配齐。配错了 `apply_sync` 会走 `bail!` 分支中止整轮，
    /// 所以这里正向证明那个分支到不了。
    #[test]
    fn every_group_is_paired_with_the_matching_action() {
        for (spec, files) in sync_with_one_file_each().groups() {
            match (spec.action, &files) {
                (SyncAction::SyncToTarget, SyncGroupFiles::Sync(_)) => {}
                (SyncAction::DeleteFromDisk, SyncGroupFiles::Delete(_)) => {}
                (SyncAction::DeleteUntracked, SyncGroupFiles::Untracked(_)) => {}
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
            assert!(rendered.contains('3'), "{}: {rendered}", spec.label);
            assert!(rendered.contains("files"), "{}: {rendered}", spec.label);
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

    // ---- p4 应答核对 ----

    /// p4 对每条规格回一行 `<规格> - <动作> <本地路径>`：逐条对上才算完成。
    #[test]
    fn sync_replies_acknowledge_every_spec() {
        let specs = vec!["//depot/a.txt#3".to_owned(), "//depot/b.txt#7".to_owned()];
        let lines = vec![
            r"//depot/a.txt#3 - updating C:\ws\a.txt".to_owned(),
            r"//depot/b.txt#7 - added as C:\ws\b.txt".to_owned(),
        ];

        assert_eq!(missing_specs(&specs, &lines).unwrap(), Vec::<String>::new());
    }

    /// 没被回应的规格要原样留出来，交给调用方补发。
    #[test]
    fn sync_replies_report_the_specs_p4_did_not_answer() {
        let specs = vec![
            "//depot/a.txt#3".to_owned(),
            "//depot/b.txt#7".to_owned(),
            "//depot/c.txt#1".to_owned(),
        ];
        let lines = vec![
            r"//depot/a.txt#3 - refreshing C:\ws\a.txt".to_owned(),
            r"//depot/c.txt#1 - refreshing C:\ws\c.txt".to_owned(),
        ];

        assert_eq!(
            missing_specs(&specs, &lines).unwrap(),
            ["//depot/b.txt#7".to_owned()]
        );
    }

    /// 规格名本身含 `" - "`：按第一处分隔会把规格截成 `//depot/a`，永远对不上，
    /// 于是一个已经同步好的文件会被误报成「没应答」并触发无谓的重发。
    #[test]
    fn a_spec_containing_the_separator_is_still_recognized() {
        let specs = vec!["//depot/a - b.txt#3".to_owned()];
        let lines = vec![r"//depot/a - b.txt#3 - updating C:\ws\a - b.txt".to_owned()];

        assert!(missing_specs(&specs, &lines).unwrap().is_empty());
    }

    /// 两条规格是前缀关系时，较长那条的应答不能记到较短那条头上。
    #[test]
    fn a_longer_spec_is_not_credited_to_its_shorter_prefix() {
        let specs = vec!["//depot/a#1".to_owned(), "//depot/a - b.txt#3".to_owned()];
        // 只回较长那条；较短那条必须仍算「没应答」。
        let lines = vec![r"//depot/a - b.txt#3 - updating C:\ws\a - b.txt".to_owned()];

        assert_eq!(
            missing_specs(&specs, &lines).unwrap(),
            ["//depot/a#1".to_owned()]
        );
    }

    /// 有输出却一条都认不出：这是输出格式与假设不符，不能当成「全都没同步」去补发，
    /// 否则每一轮都会先白跑一遍再失败。
    #[test]
    fn output_that_matches_no_spec_is_a_format_error() {
        let specs = vec!["//depot/a.txt#3".to_owned()];
        let lines = vec!["unexpected line shape".to_owned()];

        let error = missing_specs(&specs, &lines).expect_err("认不出任何规格必须失败");
        assert!(error.to_string().contains("output format"), "{error}");
    }

    /// p4 一句话都没说（例如整批没送到）：不是格式问题，全部记为待补发。
    #[test]
    fn empty_output_marks_every_spec_as_missing() {
        let specs = vec!["//depot/a.txt#3".to_owned()];

        assert_eq!(missing_specs(&specs, &[]).unwrap(), specs);
    }

    // ---- 未跟踪文件 ----

    /// 没开开关时一个查询都不发，空 depot 也照常返回空集。
    #[test]
    fn untracked_discovery_is_off_by_default() {
        let options = Options::parse_from(["p4delta", "--sync", "--force", "-w", "ws"]);
        let depot = DepotState::default();
        let workspace = WorkspaceState {
            files: vec![workspace_file(r"C:\ws\a.txt")],
            ..Default::default()
        };

        let untracked = task::block_on(discover_untracked(&options, "", &depot, &workspace))
            .expect("没开开关不该失败");
        assert!(untracked.is_empty());
    }

    /// depot 记录为空时无法区分「全部是未跟踪」与「范围/client 映射坏了」：删任何文件都是
    /// 在赌。校验必须发生在 `p4 where` 之前，连一个子进程都不该起。
    #[test]
    fn delete_untracked_refuses_an_empty_depot() {
        let options = Options::parse_from([
            "p4delta",
            "--sync",
            "--force",
            "--delete-untracked",
            "-w",
            "ws",
        ]);
        let depot = DepotState::default();
        let workspace = WorkspaceState {
            files: vec![workspace_file(r"C:\ws\a.txt")],
            ..Default::default()
        };

        let error = task::block_on(discover_untracked(&options, "", &depot, &workspace))
            .expect_err("空 depot 必须失败关闭");
        assert!(error.to_string().contains("no depot records"), "{error}");
    }

    /// `--no-scope-file` 不登记隐式排除，范围文件会作为扫盘结果留下来；但它永远不是
    /// 删除候选——否则一次「按所选执行」的删除档会删掉范围配置本身，之后每一轮都静默
    /// 退化成整个工作区。剔掉它之后没有别的候选，连 `p4 where` 都不该起。
    #[test]
    fn delete_untracked_never_treats_the_scope_file_as_untracked() {
        let options = Options::parse_from([
            "p4delta",
            "--sync",
            "--force",
            "--delete-untracked",
            "--no-scope-file",
            "-w",
            "ws",
        ]);
        let root = if cfg!(windows) { r"C:\ws" } else { "/ws" };
        let scope_path = Path::new(root).join(SCOPE_FILE_NAME);
        let mut depot = DepotState::default();
        depot.file_records = vec![record(
            &Path::new(root).join("tracked.txt").display().to_string(),
            "//depot/tracked.txt",
        )];
        let workspace = WorkspaceState {
            files: vec![workspace_file(&scope_path.display().to_string())],
            ..Default::default()
        };

        let untracked = task::block_on(discover_untracked(&options, root, &depot, &workspace))
            .expect("范围文件被剔除后没有候选，不该起 p4");
        assert!(untracked.is_empty());
    }

    // ---- 未跟踪核对：候选规模索引 ----

    /// depot 路径按**原始字符串**比较：`//depot/Main/a.txt` 与 `//depot/main/a.txt` 是两个
    /// 文件。depot 路径的身份由服务端定义，客户端不自行折叠大小写。
    #[test]
    fn untracked_cross_check_matches_the_raw_depot_path() {
        let records = [record("tracked.txt", "//depot/Main/tracked.txt")];
        let candidates = [
            mapped("same.txt", Some("//depot/Main/tracked.txt"), false),
            mapped("case.txt", Some("//depot/main/tracked.txt"), false),
        ];

        let checked = cross_check_untracked(&candidates, records.iter());

        assert_eq!(
            checked.files,
            [untracked_file("case.txt", "//depot/main/tracked.txt")]
        );
        assert_eq!(checked.outside_view, 0);
        assert_eq!(checked.view_excluded, 0);
    }

    /// 重复映射保持原样：两个本地路径映到同一个 depot 路径时，二者都跟着那条 depot 记录
    /// 走。已跟踪就都不返回；未跟踪就按请求顺序都返回。
    #[test]
    fn untracked_cross_check_preserves_order_and_duplicates() {
        let tracked = [record("one.txt", "//depot/one.txt")];
        let tracked_candidates = [
            mapped("one-a.txt", Some("//depot/one.txt"), false),
            mapped("one-b.txt", Some("//depot/one.txt"), false),
        ];
        assert!(
            cross_check_untracked(&tracked_candidates, tracked.iter())
                .files
                .is_empty()
        );

        let records: [DepotFileRecord; 0] = [];
        let untracked_candidates = [
            mapped("b.txt", Some("//depot/b.txt"), false),
            mapped("a.txt", Some("//depot/a.txt"), false),
            mapped("b-copy.txt", Some("//depot/b.txt"), false),
        ];
        assert_eq!(
            cross_check_untracked(&untracked_candidates, records.iter()).files,
            [
                untracked_file("b.txt", "//depot/b.txt"),
                untracked_file("a.txt", "//depot/a.txt"),
                untracked_file("b-copy.txt", "//depot/b.txt"),
            ]
        );
    }

    /// view 排除与「没有映射」分开计数：前者是排除行挡下的，后者是 p4 没给出映射。
    /// 两者都不删，但诊断要把两种原因区分开。
    #[test]
    fn untracked_cross_check_keeps_the_view_counters_separate() {
        let records: [DepotFileRecord; 0] = [];
        let candidates = [
            mapped("inside.txt", Some("//depot/inside.txt"), false),
            mapped("outside.txt", None, false),
            mapped("excluded.txt", None, true),
        ];

        let checked = cross_check_untracked(&candidates, records.iter());

        assert_eq!(
            checked.files,
            [untracked_file("inside.txt", "//depot/inside.txt")]
        );
        assert_eq!(checked.outside_view, 2);
        assert_eq!(checked.view_excluded, 1);
    }

    /// 已跟踪与未跟踪混在一起时，只留下 depot 里没有记录的那些，顺序不变。
    #[test]
    fn untracked_cross_check_mixes_tracked_and_untracked_files() {
        let records = [
            record("tracked-1.txt", "//depot/tracked-1.txt"),
            record("tracked-2.txt", "//depot/tracked-2.txt"),
        ];
        let candidates = [
            mapped("junk.txt", Some("//depot/junk.txt"), false),
            mapped("tracked-1.txt", Some("//depot/tracked-1.txt"), false),
            mapped("other-junk.txt", Some("//depot/other-junk.txt"), false),
            mapped("tracked-2.txt", Some("//depot/tracked-2.txt"), false),
        ];

        assert_eq!(
            cross_check_untracked(&candidates, records.iter()).files,
            [
                untracked_file("junk.txt", "//depot/junk.txt"),
                untracked_file("other-junk.txt", "//depot/other-junk.txt"),
            ]
        );
    }

    /// 性能缺陷的判据不是秒数，而是**遍历次数**：depot 记录只允许走一轮。旧实现对每个
    /// 候选 `any()` 一次，候选一多就是 O(U×D)。这里保留一个真正的未跟踪候选，保证
    /// 「记录走满一轮」是正常路径，而不是靠提前停走出来的。
    #[test]
    fn untracked_cross_check_walks_the_depot_records_once() {
        let records = [
            record("a.txt", "//depot/a.txt"),
            record("b.txt", "//depot/b.txt"),
            record("c.txt", "//depot/c.txt"),
        ];
        let candidates = [
            mapped("junk.txt", Some("//depot/junk.txt"), false),
            mapped("a.txt", Some("//depot/a.txt"), false),
            mapped("b.txt", Some("//depot/b.txt"), false),
        ];

        let mut visited = 0;
        let checked = cross_check_untracked(&candidates, records.iter().inspect(|_| visited += 1));

        assert_eq!(visited, records.len(), "depot 记录只该遍历一轮");
        assert_eq!(
            checked.files,
            [untracked_file("junk.txt", "//depot/junk.txt")]
        );
    }

    /// 候选全部已跟踪时，pending 一空就结束：后面的 depot 记录与裁决无关。
    #[test]
    fn untracked_cross_check_stops_once_every_candidate_is_tracked() {
        let records = [
            record("a.txt", "//depot/a.txt"),
            record("b.txt", "//depot/b.txt"),
            record("c.txt", "//depot/c.txt"),
            record("d.txt", "//depot/d.txt"),
        ];
        let candidates = [
            mapped("a.txt", Some("//depot/a.txt"), false),
            mapped("b.txt", Some("//depot/b.txt"), false),
        ];

        let mut visited = 0;
        let checked = cross_check_untracked(&candidates, records.iter().inspect(|_| visited += 1));

        assert_eq!(visited, 2, "候选全部命中后不该继续扫 depot");
        assert!(checked.files.is_empty());
    }

    /// 没有可映射的候选（零候选，或全被忽略规则挡掉）时不该碰 depot 记录：
    /// 那些记录本来就不在删除判据里，白扫几百万条只是给每一轮强制同步加固定成本。
    #[test]
    fn untracked_cross_check_skips_the_depot_when_nothing_was_mapped() {
        let records = [record("a.txt", "//depot/a.txt")];
        let candidates = [
            mapped("outside.txt", None, false),
            mapped("excluded.txt", None, true),
        ];

        let mut visited = 0;
        let checked = cross_check_untracked(&candidates, records.iter().inspect(|_| visited += 1));

        assert_eq!(visited, 0);
        assert!(checked.files.is_empty());
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
            Vec::new(),
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

        let changes = SyncChanges::project(&analysis, Vec::new(), Vec::new());

        assert_eq!(changes.total(), 0);
    }

    /// 未跟踪组独立于删除组：前者没有 have 记录可清，后者要下发 `#none`。
    /// 混进删除组会让 `delete_specs` 给它们拼一条 p4 无事可做的规格。
    #[test]
    fn projection_keeps_the_untracked_group_separate() {
        let analysis = SyncAnalysis::default();

        let changes = SyncChanges::project(
            &analysis,
            Vec::new(),
            vec![untracked_file("junk.log", "//depot/junk.log")],
        );

        assert_eq!(
            changes.untracked,
            [untracked_file("junk.log", "//depot/junk.log")]
        );
        assert!(changes.delete.is_empty());
        assert_eq!(changes.total(), 1);
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

        let changes = SyncChanges::project(&analysis, Vec::new(), Vec::new());

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

        let changes = SyncChanges::project(&analysis, Vec::new(), Vec::new());

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
