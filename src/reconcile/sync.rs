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
use std::path::MAIN_SEPARATOR;
use std::time::Instant;

use anyhow::{Result, anyhow, bail};

use super::analyze::{SyncAnalysis, SyncSource, analyze_at_target, guard_case_conflicts};
use super::changes::{GroupReport, GroupRow, render_title, report_group};
use super::{HashStats, delete_workspace_files};
use crate::cache::{CacheWriter, save_cache};
use crate::cli::Options;
use crate::digest::{CachePolicy, is_unchanged_since_sync, parallel_compute_digests};
use crate::json::{HandoffFile, Mode, P4Command, ignore_query_count, say_phase, sayln};
use crate::model::{DepotState, HaveRecord, TargetMap, WorkspaceCache, WorkspaceState};
use crate::p4::process::{
    FailureMode, StdinBudget, command_line_ranges, command_line_safe,
    run_p4_command_batched_with_budget, say_planned_batches,
};
use crate::path::{directory_key, local_path_key, normalize_local_path};
use crate::workspace::{IgnoreQueryMode, query_ignored_paths};

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
    /// JSON 记录的 `class`，见 `docs/json-contract.md`。
    id: &'static str,

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

    /// 本地没有：写回来。
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
        //
        // 预算是 [`StdinBudget::FORCE_SYNC`]：这一组在真实工作区里是单个最大的一批
        // （实测 84,941 条 `#none` 规格）。条数上限 8192 给它定下至少 11 批的下界
        // （84941 / 8192），容量再大也压不到 8 批——8 是并发目标，不是批数的可达下界。
        run_p4_command_batched_with_budget(
            options,
            work_dir,
            P4Command::Sync,
            SYNC_ARGS,
            &specs,
            false,
            FailureMode::ExitCodeOrStderr,
            StdinBudget::FORCE_SYNC,
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

/// 一组文件在下发时用的规格：同步组是 `//depot/f#<rev>`，删除组是 `//depot/f#none`。
///
/// 预演的批数估算用它，所以那行估算与真跑时交给 p4 的那串参数同源——估算不是另写一套
/// 近似，而是同一份规划器在同一个输入上的输出。
fn group_specs(files: &SyncGroupFiles<'_>) -> Vec<String> {
    match files {
        SyncGroupFiles::Sync(files) => sync_specs(files),
        SyncGroupFiles::Delete(files) => delete_specs(files),
    }
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
    //
    // 预算是 [`StdinBudget::FORCE_SYNC`]：Update / Revert / Restore 三组共用这一条，
    // 真实工作区里 restore 单组就有 85,074 条规格。同样地，条数上限 8192 决定至少 11 批
    // 的下界，256 KiB → 1 MiB 的实际差别取决于规格的平均长度，不是「几十批 → 8 批」。
    run_p4_command_batched_with_budget(
        options,
        work_dir,
        P4Command::Sync,
        SYNC_ARGS,
        &specs,
        false,
        FailureMode::ExitCodeOrStderr,
        StdinBudget::FORCE_SYNC,
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
        sayln!("   Syncing the workspace to the target depot revision.");
    } else {
        sayln!("   Counting files to sync (dry run).")
    }
    // 预演也打这句（clean 只在 -a 时打）：sync 相对 clean 的卖点正是「不删未跟踪的文件」，
    // 而「会覆盖未打开文件的本地改动」是它的代价。授权之前就得看得见代价。
    sayln!(
        "      WARNING: this overwrites local changes to files that are not opened. It cannot be undone."
    );

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
            // 预演用**同一份**规划器报出这一组真跑时会切出几批：批数是调用数的直接来源，
            // 而调用数是这一档唯一可核对的下发规模。一个进程都不起、一次调用都不记
            // （`p4Calls` 因此不受影响），它报的就是真跑时该有的批数。
            say_planned_batches(
                &format!("plan: {} group (p4 {})", spec.label, SYNC_ARGS[0]),
                SYNC_ARGS,
                &group_specs(&files),
                StdinBudget::FORCE_SYNC,
            );
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

/// 删除候选的纯内存统计与估算。**只输出聚合数字，不含路径原文**——几万条候选的路径
/// 逐条打印就是刷屏，而且日志里出现工作区路径本身就是泄露。
///
/// 为什么要它：现场那一轮 `ignores` 332 次调用只留下了总数，没有候选的长度分布、父目录
/// 数量与聚集度，下一次「怎么把它降下来」只能靠猜。这里全部在内存里算完：不执行任何 p4
/// 调用、不改 cwd、不改忽略规则。
///
/// `estimated_*` 两项是**估算**，衡量两类还没实现的改动值不值得设计：一是保持 cwd、只把
/// 参数换成 client root 相对路径（`estimated_cwd_relative`），二是按共同祖先分组、每组
/// 各自换一次 cwd（`estimated_by_ancestor`）。它们不执行、也**不进实际调用计数**
/// （`--json` 的 `p4Calls` 只认真的 `spawn`，见 [`crate::json::count_p4_command`]）。
#[derive(Debug, PartialEq)]
struct DeleteCandidateStats {
    total: usize,

    /// 静态判定能在命令行上安全表示的候选（`command_line_safe`：Windows 上是 ASCII 的那
    /// 些）。不叫「能上命令行」是因为判定与**平台**无关：别的平台 UTF-8 命令行装得下这些
    /// 名字，这道限制只在会被 ANSI 代码页转码的那些机器上成立。
    statically_safe: usize,

    /// 绝对路径的字节长度分布（最小值 / 中位数 / 最大值）。
    absolute: Option<(usize, usize, usize)>,

    /// 文件名的字节长度分布。任何写法下每个候选至少要写上文件名那么长，所以这个分布是
    /// **理论乐观下界**：现场的文件名长度未知，分组方案再巧也压不到它以下。
    basename: Option<(usize, usize, usize)>,

    /// client root 相对路径的字节长度分布，只统计真的在 root 下的候选。
    cwd_relative: Option<(usize, usize, usize)>,

    /// 不在 client root 下的候选数（相对化不了，缺席于上面的分布与估算）。
    outside_root: usize,

    /// 候选所在的父目录数量与最密那个目录里的候选数（聚集度）。
    parents: usize,
    densest_parent: usize,

    /// 今天这条通道的批数：命令行预算下的绝对路径。`query_ignored_paths` 按静态可否上
    /// 命令行劈成两组（安全组走 `ignores -i`、不安全组走 `ignores -i -v`），**两组各切
    /// 各的批、成本相加**——只数安全组会把这条通道的规模报低。
    batches_absolute: usize,

    /// 估算：保持 cwd、参数改成 client root 相对路径时的理论批数。root 之外的候选保持
    /// 绝对写法（今天就是这样交给 p4 的），静态上不了命令行的候选另发一批，成本都计在内。
    estimated_cwd_relative: Option<usize>,

    /// 估算：(共同祖先深度, 该深度下的理论批数)。每项都加上那些**改不了写法**的候选
    /// （root 之外的全部，以及静态上不了命令行的那一组）的绝对回退成本——不然候选全在
    /// root 外、或全是非 ASCII 时这里会报 0 批，把「代价没算」读成「没有代价」。
    estimated_by_ancestor: Vec<(usize, usize)>,
}

/// 估算只在候选数不超过这个上限时做：那是「候选数 × (1 + 祖先深度)」份短路径副本，
/// 病态输入下不该让诊断本身变成开销。
const ESTIMATE_MAX_CANDIDATES: usize = 200_000;

/// 估算用的祖先深度：再深收益递减，而每加一层都要把全部候选的相对路径重切一遍。
const ESTIMATE_ANCESTOR_DEPTHS: [usize; 3] = [1, 2, 3];

impl DeleteCandidateStats {
    fn of(candidates: &[&str], work_dir: &str) -> Self {
        Self::of_with_cap(candidates, work_dir, ESTIMATE_MAX_CANDIDATES)
    }

    fn of_with_cap(candidates: &[&str], work_dir: &str, estimate_cap: usize) -> Self {
        let root = normalize_local_path(work_dir).into_owned();
        let root = root.trim_end_matches(MAIN_SEPARATOR);
        let root_key = local_path_key(root);
        let estimate = candidates.len() <= estimate_cap;

        let mut statically_safe = 0usize;
        let mut absolute = Vec::with_capacity(candidates.len());
        let mut basename = Vec::with_capacity(candidates.len());
        let mut relative_lengths = Vec::new();
        let mut outside_root = 0usize;
        let mut parents: HashMap<String, usize> = HashMap::new();
        // 估算的三份输入：`relative_args` 是能相对化的候选（静态安全且真在 root 下），
        // `absolute_args` 是静态安全、但只能在 root 之外保持绝对写法的那些，`unsafe_args`
        // 是静态上不了命令行的那一组——它们**两种写法都用不了**，只能按绝对回退单独发一批
        // （与真实通道的两组切分一致），成本必须计进每一条估算。
        let mut relative_args: Vec<String> = Vec::new();
        let mut absolute_args: Vec<String> = Vec::new();
        let mut unsafe_args: Vec<String> = Vec::new();

        for path in candidates {
            let safe = command_line_safe(path);
            if safe {
                statically_safe += 1;
            } else if estimate {
                unsafe_args.push((*path).to_owned());
            }
            absolute.push(path.len());

            let normalized = normalize_local_path(path);
            basename.push(basename_len(&normalized));
            let relative = relative_to_root(&normalized, root, &root_key);

            match relative {
                Some(rest) => {
                    relative_lengths.push(rest.len());
                    if estimate && safe {
                        relative_args.push(rest.to_owned());
                    }
                }
                None => {
                    outside_root += 1;
                    if estimate && safe {
                        absolute_args.push((*path).to_owned());
                    }
                }
            }

            let parent = std::path::Path::new(path)
                .parent()
                .map(|dir| directory_key(dir.to_string_lossy().as_ref()))
                .unwrap_or_default();
            *parents.entry(parent).or_insert(0) += 1;
        }

        let (estimated_cwd_relative, estimated_by_ancestor) = if estimate {
            let outside_absolute = command_line_batches(absolute_args.iter().map(String::as_str));
            let unsafe_absolute = command_line_batches(unsafe_args.iter().map(String::as_str));
            let relatives: Vec<&str> = relative_args.iter().map(String::as_str).collect();
            (
                // 安全组是一条命令（能相对的用相对、root 之外的保持绝对），不安全组另发一批。
                Some(
                    command_line_batches(
                        relative_args
                            .iter()
                            .chain(absolute_args.iter())
                            .map(String::as_str),
                    ) + unsafe_absolute,
                ),
                ESTIMATE_ANCESTOR_DEPTHS
                    .iter()
                    .map(|depth| {
                        (
                            *depth,
                            batches_by_ancestor(&relatives, *depth)
                                + outside_absolute
                                + unsafe_absolute,
                        )
                    })
                    .collect(),
            )
        } else {
            (None, Vec::new())
        };

        Self {
            total: candidates.len(),
            statically_safe,
            absolute: length_spread(&absolute),
            basename: length_spread(&basename),
            cwd_relative: length_spread(&relative_lengths),
            outside_root,
            parents: parents.len(),
            densest_parent: parents.values().copied().max().unwrap_or(0),
            // 两组各切各的批再相加：凑成一份列表切批会让批跨过分组的边界，比真实的
            // 两次查询少几批。
            batches_absolute: command_line_batches(
                candidates
                    .iter()
                    .copied()
                    .filter(|path| command_line_safe(path)),
            ) + command_line_batches(
                candidates
                    .iter()
                    .copied()
                    .filter(|path| !command_line_safe(path)),
            ),
            estimated_cwd_relative,
            estimated_by_ancestor,
        }
    }

    /// 一行给 `[diag]` 的说明。全是数字与估算，**一个路径原文都不含**。
    fn describe(&self) -> String {
        let spread = |values: Option<(usize, usize, usize)>| match values {
            Some((min, median, max)) => format!("{min}/{median}/{max}"),
            None => "n/a".to_owned(),
        };

        let mut text = format!(
            "{} delete candidate(s), {} statically safe for the p4 command line / {} not, absolute \
             path bytes min/median/max {}, client-root-relative {} ({} outside the root), basename \
             bytes {} (the optimistic floor for any regrouping), {} parent dir(s) (densest {}), \
             command-line batches today {}",
            self.total,
            self.statically_safe,
            self.total - self.statically_safe,
            spread(self.absolute),
            spread(self.cwd_relative),
            self.outside_root,
            spread(self.basename),
            self.parents,
            self.densest_parent,
            self.batches_absolute,
        );

        match self.estimated_cwd_relative {
            Some(estimate) => {
                let by_ancestor = self
                    .estimated_by_ancestor
                    .iter()
                    .map(|(depth, batches)| format!("d{depth}={batches}"))
                    .collect::<Vec<String>>()
                    .join(" ");
                text.push_str(&format!(
                    "; estimates, nothing executed: {estimate} if the arguments were \
                     client-root-relative, by shared ancestor {by_ancestor}; both keep the \
                     candidates outside the root absolute and add the statically unsafe ones \
                     as an absolute group of their own"
                ));
            }
            None => text.push_str(&format!(
                "; estimates skipped (over {ESTIMATE_MAX_CANDIDATES} candidates)"
            )),
        }

        text
    }
}

/// 一段长度分布的最小值 / 中位数 / 最大值。空集合返回 `None`。
fn length_spread(values: &[usize]) -> Option<(usize, usize, usize)> {
    if values.is_empty() {
        return None;
    }

    let mut sorted = values.to_vec();
    sorted.sort_unstable();

    Some((
        sorted[0],
        sorted[sorted.len() / 2],
        sorted[sorted.len() - 1],
    ))
}

/// 命令行预算（[`ARGUMENT_LENGTH_MAX`]）下的贪心批数：算式只在 [`command_line_ranges`]
/// 一处，这里只取它的批数（估算不需要 range，也不必把几万条候选复制成 `Vec<String>`——
/// 长度表是 `usize`）。
fn command_line_batches<'a>(paths: impl Iterator<Item = &'a str>) -> usize {
    let lengths: Vec<usize> = paths.map(str::len).collect();
    command_line_ranges(&lengths).len()
}

/// 路径最后一段（文件名）的字节长度。任何分组方案下每个候选至少要写上文件名那么长，
/// 所以这个分布是估算的理论乐观下界。
fn basename_len(path: &str) -> usize {
    match path.rfind(MAIN_SEPARATOR) {
        Some(index) => path.len() - index - 1,
        None => path.len(),
    }
}

/// `path` 在 `root` 之下时返回相对部分（不含衔接的分隔符），否则 `None`。
///
/// 组件边界按平台口径比：`C:\ws2` 不在根 `C:\ws` 底下，而 Windows/macOS 上
/// `c:\ws\x` 与 `C:\ws` 是同一个根（[`local_path_key`] 折大小写，与全仓的路径身份
/// 口径一致）。切原串仍按字节长度——`local_path_key` 折 Unicode 小写会改变长度，
/// 所以它只用来比较，`get(..)` 的边界检查兜住切分。
fn relative_to_root<'a>(path: &'a str, root: &str, root_key: &str) -> Option<&'a str> {
    let head = path.get(..root.len())?;
    if local_path_key(head) != root_key {
        return None;
    }

    path.get(root.len()..)?
        .strip_prefix(MAIN_SEPARATOR)
        .filter(|rest| !rest.is_empty())
}

/// 估算：按共同祖先（相对路径的前 `depth` 个组件）分组，每组在自己的 cwd 下用**相对**
/// 参数发问，一共要多少个 argv 批次。分组本身只在这里算，不执行（见
/// [`DeleteCandidateStats`]）。
fn batches_by_ancestor(relative: &[&str], depth: usize) -> usize {
    let mut groups: HashMap<&str, Vec<&str>> = HashMap::new();

    for path in relative {
        let (ancestor, rest) = split_at_ancestor(path, depth);
        groups.entry(ancestor).or_default().push(rest);
    }

    groups
        .values()
        .map(|group| command_line_batches(group.iter().copied()))
        .sum()
}

/// 把相对路径切成「前 `depth` 个组件（共同祖先）」与「相对该祖先的剩余部分」。
///
/// 路径浅于 `depth` 时退到它的父目录：再往上就不是这个候选自己的共同祖先了，
/// 那等于把不相关的文件并进同一组。
fn split_at_ancestor(path: &str, depth: usize) -> (&str, &str) {
    let mut seen = 0usize;

    for (index, _) in path.match_indices(MAIN_SEPARATOR) {
        seen += 1;
        if seen == depth {
            return (&path[..index], &path[index + 1..]);
        }
    }

    match path.rfind(MAIN_SEPARATOR) {
        Some(index) => (&path[..index], &path[index + 1..]),
        None => ("", path),
    }
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

    // 候选的规模与形状先落进日志：它是「ignores 为什么是这么多次」唯一的直接证据，
    // 而且一个 p4 调用都不发、不改 cwd、不改忽略规则（见 [`DeleteCandidateStats`]）。
    let paths: Vec<&str> = analysis
        .delete
        .iter()
        .map(|entry| entry.file.path.as_str())
        .collect();
    say_phase(
        "delete-candidates",
        &DeleteCandidateStats::of(&paths, work_dir).describe(),
        None,
    );

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
            sayln!("   Checking digests for {} files.", needs_digest.len());

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
        changes: SyncChanges::project(&analysis, drifted),
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

    use std::path::MAIN_SEPARATOR_STR;
    use std::time::UNIX_EPOCH;

    use crate::model::{DepotFileRecord, WorkspaceFile};
    use crate::p4::process::{ARGUMENT_LENGTH_MAX, compute_batches};
    use crate::path::{local_path_key, path_identity_ignores_case};
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
            ]
        );
        assert_eq!(sync.total(), 4);
    }

    /// 每一行都带齐两个路径；`rev` 只有删除组缺席——那一行的意思是「目标时刻它不该在」，
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

    // ---- 删除候选的统计与估算 ----

    /// 估算用的贪心算式必须与真实规划器逐字一致：估算要是偏了，拿它判断「缩短路径表示
    /// 值不值得设计」就是在用一个错数当依据。
    #[test]
    fn the_estimate_counts_batches_exactly_like_the_planner() {
        let paths: Vec<String> = (0..5_000)
            .map(|index| format!(r"C:\ws\dir{:03}\sub\file{index:04}.txt", index % 97))
            .collect();
        assert_eq!(
            command_line_batches(paths.iter().map(String::as_str)),
            compute_batches(&paths).len()
        );

        // 「单个超载参数独占一批」这条规则两边都要有，否则估算会把超长路径按长度均摊。
        let odd = vec![
            "short".to_owned(),
            "x".repeat(ARGUMENT_LENGTH_MAX + 1),
            "another".to_owned(),
        ];
        assert_eq!(
            command_line_batches(odd.iter().map(String::as_str)),
            compute_batches(&odd).len()
        );
        assert_eq!(compute_batches(&odd).len(), 3);
    }

    /// 分组估算切的是「共同祖先 + 相对该祖先的剩余部分」：祖先不够深时退到父目录，
    /// 而不是把不相关的候选并进一组。
    #[test]
    fn the_ancestor_split_takes_the_depth_it_is_asked_for() {
        // 断言里用平台分隔符拼路径：切分器认的是 `MAIN_SEPARATOR`。
        let rest = |parts: &[&str]| parts.join(MAIN_SEPARATOR_STR);
        let path = rest(&["a", "b", "c", "f.txt"]);

        assert_eq!(
            split_at_ancestor(&path, 1),
            ("a", rest(&["b", "c", "f.txt"]).as_str())
        );
        assert_eq!(
            split_at_ancestor(&path, 2),
            (rest(&["a", "b"]).as_str(), rest(&["c", "f.txt"]).as_str())
        );
        assert_eq!(
            split_at_ancestor(&path, 3),
            (rest(&["a", "b", "c"]).as_str(), "f.txt")
        );
        // 比路径还深：退到最深那个目录，剩余部分是文件名。
        assert_eq!(
            split_at_ancestor(&path, 9),
            (rest(&["a", "b", "c"]).as_str(), "f.txt")
        );
        // 没有目录可退：整条进剩余部分，祖先为空。
        assert_eq!(split_at_ancestor("f.txt", 3), ("", "f.txt"));
    }

    /// 统计报的是规模与形状，**一个路径原文都不含**：这行要落盘，工作区路径不该跟着走。
    #[test]
    fn the_delete_candidate_stats_describe_shape_without_paths() {
        let root = if cfg!(windows) { r"C:\ws" } else { "/ws" };
        let sep = MAIN_SEPARATOR;
        let candidates = [
            format!("{root}{sep}a{sep}one.txt"),
            format!("{root}{sep}a{sep}two.txt"),
            format!("{root}{sep}b{sep}three.txt"),
            format!("{root}{sep}b{sep}four.txt"),
            format!("{root}{sep}b{sep}five.txt"),
        ];
        let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();

        let stats = DeleteCandidateStats::of(&refs, root);

        assert_eq!(stats.total, 5);
        assert_eq!(stats.statically_safe, 5);
        assert_eq!(stats.outside_root, 0);
        assert_eq!(stats.parents, 2, "a 与 b 两个目录");
        assert_eq!(stats.densest_parent, 3, "b 里有三个候选");
        assert_eq!(
            stats.absolute.unwrap().2,
            candidates.iter().map(|path| path.len()).max().unwrap()
        );
        // 文件名是理论乐观下界：它必然短于全路径，但不为零。
        assert_eq!(
            stats.basename.unwrap().2,
            "three.txt".len(),
            "最长的是 three.txt"
        );
        assert!(stats.cwd_relative.unwrap().2 < stats.absolute.unwrap().2);
        // 五条短路径各自成不了一整批：今天就是一批，相对化之后还是一批。
        assert_eq!(stats.batches_absolute, 1);
        assert_eq!(stats.estimated_cwd_relative, Some(1));
        assert_eq!(stats.estimated_by_ancestor, vec![(1, 2), (2, 2), (3, 2)]);

        let text = stats.describe();
        assert!(text.contains("5 delete candidate(s)"), "{text}");
        assert!(
            text.contains("5 statically safe for the p4 command line / 0 not"),
            "{text}"
        );
        assert!(text.contains("basename bytes"), "{text}");
        assert!(text.contains("2 parent dir(s) (densest 3)"), "{text}");
        assert!(text.contains("estimates, nothing executed:"), "{text}");
        for path in &candidates {
            assert!(!text.contains(path.as_str()), "路径原文不该落盘：{text}");
        }
    }

    /// 估算要真的随路径长度变化，否则它回答不了「缩短路径表示值不值得」这个问题：
    /// 一个很长的 client root，候选全挤在它的同一个子目录里——相对化省下整个根前缀，
    /// 按共同祖先分组再省下「相对根的那层子目录」。
    #[test]
    fn the_estimates_respond_to_how_long_the_paths_are() {
        let sep = MAIN_SEPARATOR;
        let root = if cfg!(windows) {
            format!(r"C:{sep}workspaces{sep}verylongclientrootname{sep}another{sep}long{sep}here")
        } else {
            format!("/workspaces{sep}verylongclientrootname{sep}another{sep}long{sep}here")
        };
        let candidates: Vec<String> = (0..20_000)
            .map(|index| format!(r"{root}{sep}projectdir{sep}file{index:06}.bin"))
            .collect();
        let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();

        let stats = DeleteCandidateStats::of(&refs, &root);

        assert!(
            stats.batches_absolute > 1,
            "夹具的绝对路径要长到切出多批：{}",
            stats.batches_absolute
        );
        let relative = stats.estimated_cwd_relative.expect("候选数远低于上限");
        assert!(
            relative < stats.batches_absolute,
            "相对写法没有变少：{} vs {}",
            stats.batches_absolute,
            relative
        );
        assert_eq!(
            stats
                .estimated_by_ancestor
                .iter()
                .map(|(depth, _)| *depth)
                .collect::<Vec<usize>>(),
            ESTIMATE_ANCESTOR_DEPTHS.to_vec()
        );
        assert!(
            stats
                .estimated_by_ancestor
                .iter()
                .all(|(_, batches)| *batches <= relative),
            "只有一个子目录时，分组不该比相对写法更贵：{:?} vs {relative}",
            stats.estimated_by_ancestor
        );

        // 反过来也一样重要：候选散在大量目录里时，按祖先分组是**每组一次调用**，比一份
        // 相对路径列表更贵。估算要如实报出这个代价——它给的是判断依据，不是省钱承诺。
        let scattered: Vec<String> = (0..500)
            .map(|index| format!(r"{root}{sep}d{index}{sep}f.txt"))
            .collect();
        let refs: Vec<&str> = scattered.iter().map(String::as_str).collect();
        let stats = DeleteCandidateStats::of(&refs, &root);
        assert!(
            stats.estimated_by_ancestor[0].1 > stats.estimated_cwd_relative.unwrap(),
            "散开的候选按祖先分组应当更贵：{:?} vs {:?}",
            stats.estimated_by_ancestor,
            stats.estimated_cwd_relative
        );
    }

    /// 候选太多时不做分组估算：那是「候选数 × 深度」份短路径副本，诊断本身不该变成开销。
    /// 与估算无关的那些统计照旧要给出来。
    #[test]
    fn the_estimates_are_skipped_when_there_are_too_many_candidates() {
        let root = r"C:\ws";
        let candidates: Vec<String> = (0..50)
            .map(|index| format!(r"{root}\d{index}\f.txt"))
            .collect();
        let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();

        let stats = DeleteCandidateStats::of_with_cap(&refs, root, 10);

        assert_eq!(stats.total, 50);
        assert_eq!(stats.parents, 50);
        assert_eq!(stats.estimated_cwd_relative, None);
        assert!(stats.estimated_by_ancestor.is_empty());
        assert!(
            stats.describe().contains("estimates skipped"),
            "{}",
            stats.describe()
        );
    }

    /// 交不到命令行上的候选（Windows 上的非 ASCII 名字，见 [`command_line_safe`]）单独
    /// 计数：它们走的是另一条判据（`p4 ignores -i -v` 逐字核对回显），规模决定那条路
    /// 的代价。空候选不该 panic——`protect_delete_candidates` 提前返回，但那是以「调用方
    /// 不调用」为前提的，统计函数自身要能处理空输入。
    #[test]
    fn the_stats_split_by_the_command_line_and_survive_an_empty_input() {
        let root = if cfg!(windows) { r"C:\ws" } else { "/ws" };
        let ascii = format!(r"{root}\plain.txt");
        let non_ascii = format!(r"{root}\使用说明.txt");
        let refs = [ascii.as_str(), non_ascii.as_str()];

        let stats = DeleteCandidateStats::of(&refs, root);

        assert_eq!(stats.total, 2);
        assert_eq!(
            stats.total - stats.statically_safe,
            if cfg!(windows) { 1 } else { 0 },
            "Windows 上非 ASCII 交不到命令行，别的平台没有这道限制"
        );

        let empty = DeleteCandidateStats::of(&[], root);
        assert_eq!(empty.total, 0);
        assert_eq!(empty.absolute, None);
        assert_eq!(empty.batches_absolute, 0);
        assert_eq!(empty.estimated_cwd_relative, Some(0));
        assert!(empty.describe().contains("0 delete candidate(s)"));
    }

    /// 候选不在 client root 下（AltRoots、盘符大小写不一致）时不算进相对长度分布，
    /// 也不算进相对化估算——那种候选只能照原样交给 p4。
    #[test]
    fn candidates_outside_the_client_root_are_counted_separately() {
        let root = if cfg!(windows) { r"C:\ws" } else { "/ws" };
        let sep = MAIN_SEPARATOR;
        let inside = format!("{root}{sep}a.txt");
        let outside = if cfg!(windows) {
            format!(r"C:{sep}other{sep}b.txt")
        } else {
            format!("/other{sep}b.txt")
        };
        // 前缀相同但不是一个目录：组件边界要挡住它。
        let sibling = if cfg!(windows) {
            format!(r"C:{sep}ws2{sep}c.txt")
        } else {
            format!("/ws2{sep}c.txt")
        };
        let refs = [inside.as_str(), outside.as_str(), sibling.as_str()];

        let stats = DeleteCandidateStats::of(&refs, root);

        assert_eq!(stats.total, 3);
        assert_eq!(stats.outside_root, 2, "{outside} 与 {sibling} 都不在根下");
        assert!(stats.cwd_relative.is_some());
        // root 之外的候选只能保持绝对写法，但它们的成本必须计进估算——否则一个全在
        // root 外的现场看到的是「按祖先分组 0 批」，把「没算」读成「不要钱」。
        assert!(
            stats
                .estimated_by_ancestor
                .iter()
                .all(|(_, batches)| *batches >= 1),
            "root 之外的候选也要付绝对写法的成本：{:?}",
            stats.estimated_by_ancestor
        );
    }

    /// 根的比对折大小写：`c:\ws\x` 与根 `C:\ws` 是同一个目录（Windows/macOS），
    /// 组件边界仍要挡住 `C:\ws2`。折过的键会改字节长度（土耳其语的 `İ` 折成两个
    /// 字符），所以切分是按原串的字节边界做的。
    #[test]
    fn the_root_prefix_comparison_folds_case_per_platform() {
        if !path_identity_ignores_case() {
            return;
        }

        let sep = MAIN_SEPARATOR;
        let inside = format!("c:{sep}ws{sep}sub{sep}file.txt");
        let root = format!("C:{sep}ws");
        let stats = DeleteCandidateStats::of(&[inside.as_str()], &root);

        assert_eq!(stats.outside_root, 0, "盘符/目录名的大小写是同一个根");
        assert!(stats.cwd_relative.is_some());

        // 折过的键仍要按组件边界比：`C:\ws2` 不在 `C:\ws` 底下。
        let sibling = format!("C:{sep}WS2{sep}file.txt");
        let stats = DeleteCandidateStats::of(&[sibling.as_str()], &root);
        assert_eq!(
            stats.outside_root, 1,
            "{sibling} 只是前缀相同，不是一个目录"
        );
    }

    /// 候选全在 client root 之外时，两条估算都必须报出绝对写法的成本（> 0 批）：
    /// 分组在这里帮不上任何忙，但「帮不上忙」不等于「不要钱」。
    #[test]
    fn the_estimates_charge_for_candidates_that_can_only_stay_absolute() {
        let root = if cfg!(windows) { r"C:\ws" } else { "/ws" };
        let outside = if cfg!(windows) { r"C:\other" } else { "/other" };
        // 路径长到足以切出多批：单批 30,719 字节，这里总量远超它。
        let candidates: Vec<String> = (0..20_000)
            .map(|index| {
                format!(
                    r"{outside}{MAIN_SEPARATOR}dir{index:05}{MAIN_SEPARATOR}name_{index:05}.txt"
                )
            })
            .collect();
        let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();

        let stats = DeleteCandidateStats::of(&refs, root);

        assert_eq!(stats.outside_root, stats.total);
        assert!(
            stats.batches_absolute > 1,
            "夹具的绝对路径要长到切出多批：{}",
            stats.batches_absolute
        );
        assert_eq!(
            stats.estimated_cwd_relative,
            Some(stats.batches_absolute),
            "没有候选能相对化时，相对化估算就是同一份绝对路径列表"
        );
        assert!(
            stats
                .estimated_by_ancestor
                .iter()
                .all(|(_, batches)| *batches == stats.batches_absolute),
            "分组帮不上忙，但成本要与绝对写法一致：{:?} vs {}",
            stats.estimated_by_ancestor,
            stats.batches_absolute
        );
    }

    /// `batches_absolute` 报的是今天这条通道的真实规模：`query_ignored_paths` 按静态可否
    /// 上命令行劈成两组、**各切各的批再相加**。只数安全组会把这条通道报低一批起。
    #[test]
    fn the_command_line_batches_sum_both_static_groups() {
        let root = if cfg!(windows) { r"C:\ws" } else { "/ws" };
        // 非 ASCII 名字造得又长又多：它们走的是逐字核对回显的另一条判据（`ignores -i -v`），
        // 但那也是一批 argv，成本要报出来。
        let mut candidates: Vec<String> = (0..10)
            .map(|index| format!(r"{root}{MAIN_SEPARATOR}ascii{index:03}.txt"))
            .collect();
        candidates.extend(
            (0..2_000).map(|index| format!(r"{root}{MAIN_SEPARATOR}说明文档{index:04}号.txt")),
        );
        let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();

        let stats = DeleteCandidateStats::of(&refs, root);

        let safe: Vec<&str> = candidates
            .iter()
            .map(String::as_str)
            .filter(|path| command_line_safe(path))
            .collect();
        let unreadable: Vec<&str> = candidates
            .iter()
            .map(String::as_str)
            .filter(|path| !command_line_safe(path))
            .collect();
        assert_eq!(stats.statically_safe, safe.len());
        assert_eq!(
            stats.batches_absolute,
            command_line_batches(safe.iter().copied())
                + command_line_batches(unreadable.iter().copied())
        );
        if cfg!(windows) {
            assert_eq!(stats.statically_safe, 10);
            assert!(
                command_line_batches(unreadable.iter().copied()) > 1,
                "不安全组自己要切出多批，相加才验证得到"
            );
            assert_ne!(
                stats.batches_absolute,
                command_line_batches(candidates.iter().map(String::as_str)),
                "两组各自切批，凑成一份列表会少算跨组的边界"
            );
        }
    }

    /// 混合候选里，静态上不了命令行的那一组**两种写法都用不了**：相对化省不下它们，祖先
    /// 分组也收不进任何一组，只能作为绝对回退单独一批。摘掉它们再算一次，三项数字都该
    /// 正好少掉那一组自己的批数——少算就是把这个代价读成「不要钱」。
    #[test]
    fn the_unsafe_candidates_are_charged_as_an_absolute_fallback_everywhere() {
        let root = if cfg!(windows) { r"C:\ws" } else { "/ws" };
        let candidates: Vec<String> = (0..10)
            .map(|index| format!(r"{root}{MAIN_SEPARATOR}dir{index:02}{MAIN_SEPARATOR}ascii.txt"))
            .chain(
                (0..2_000).map(|index| format!(r"{root}{MAIN_SEPARATOR}说明文档{index:04}号.txt")),
            )
            .collect();
        let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();

        let stats = DeleteCandidateStats::of(&refs, root);
        let safe: Vec<&str> = candidates
            .iter()
            .map(String::as_str)
            .filter(|path| command_line_safe(path))
            .collect();
        let unreadable: Vec<&str> = candidates
            .iter()
            .map(String::as_str)
            .filter(|path| !command_line_safe(path))
            .collect();
        let unsafe_absolute = command_line_batches(unreadable.iter().copied());
        let only_safe = DeleteCandidateStats::of(&safe, root);

        assert_eq!(
            stats.batches_absolute - only_safe.batches_absolute,
            unsafe_absolute
        );
        assert_eq!(
            stats.estimated_cwd_relative.unwrap() - only_safe.estimated_cwd_relative.unwrap(),
            unsafe_absolute
        );
        for ((depth, with), (_, without)) in stats
            .estimated_by_ancestor
            .iter()
            .zip(only_safe.estimated_by_ancestor.iter())
        {
            assert_eq!(
                with - without,
                unsafe_absolute,
                "深度 {depth} 的分组估算漏了不安全候选的绝对回退"
            );
        }

        if cfg!(windows) {
            assert!(unsafe_absolute > 1, "不安全组要切出多批：{unsafe_absolute}");
        }
    }

    /// 估算与规划器不只在「大致相等」上一致：切割的**边界**必须是同一条线。
    /// `compute_batches` 判断一格用不含分隔符的长度、累计一格含一个分隔符，
    /// [`command_line_ranges`] 是同一份实现，这里把边界两侧的批数都钉住。
    #[test]
    fn the_command_line_boundary_sits_where_the_planner_says_it_does() {
        // 第一个参数占 101 字节（含分隔符），第二个把它恰好顶到上限：顶到 = 不切，
        // 超一字节 = 切。
        let first = "a".repeat(100);
        let fits = "b".repeat(ARGUMENT_LENGTH_MAX - 100 - 1);
        let overflows = "b".repeat(ARGUMENT_LENGTH_MAX - 100);

        for (second, expected) in [(&fits, 1usize), (&overflows, 2)] {
            let args = vec![first.clone(), second.clone()];
            assert_eq!(
                compute_batches(&args).len(),
                expected,
                "规划器的边界：{} + {}",
                first.len(),
                second.len()
            );
            assert_eq!(
                command_line_batches(args.iter().map(String::as_str)),
                expected,
                "估算必须落在同一条边界上"
            );
        }
    }

    /// 实测入口的规格必须与执行路径一致：预演报的批数才等于真跑的批数。
    #[test]
    fn the_planned_specs_are_the_executed_specs() {
        let sync_group = [sync_file("a.txt", "//depot/a.txt", 3)];
        let delete_group = [delete_file("gone.txt", "//depot/gone.txt")];

        assert_eq!(
            group_specs(&SyncGroupFiles::Sync(&sync_group)),
            sync_specs(&sync_group)
        );
        assert_eq!(
            group_specs(&SyncGroupFiles::Delete(&delete_group)),
            delete_specs(&delete_group)
        );
        // 删除组里没有 have 记录的候选不产生规格（预演因此报 0 批，与真跑一致）。
        assert!(
            delete_specs(&[DeleteFile {
                client_file: r"C:\ws\b.txt".to_owned(),
                depot_file: "//depot/b.txt".to_owned(),
                has_have_record: false,
            }])
            .is_empty()
        );
    }
}
