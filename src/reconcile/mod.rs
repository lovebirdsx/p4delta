//! 范围的 reconcile 编排。

mod analyze;
pub(crate) mod changes;
mod clean;
mod sync;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::time::Instant;

use anyhow::{Result, anyhow, bail};

use humansize::{BINARY, format_size};
use rayon::prelude::*;

use analyze::{Analysis, analyze};
use changes::apply_changes;
use clean::{CleanChanges, apply_clean};
use sync::{apply_sync, build_sync_changes};

use crate::cache::{CacheWriter, save_cache};
use crate::cli::Options;
use crate::digest::{CachePolicy, is_unchanged_since_sync, parallel_compute_digests};
use crate::json::{
    HandoffFile, Mode, emit_handoff_files, emit_native_reconcile_records, emit_progress,
    emit_unmatched, sayln, set_reason, set_scope_matched,
};
use crate::model::{
    DepotState, DigestType, HaveRecord, TargetMap, WorkspaceCache, WorkspaceFile, WorkspaceState,
};
use crate::p4::fstat::{run_p4_fstat_all, run_p4_fstat_at_revision};
use crate::p4::process::{FailureMode, run_p4_command_batched, run_p4_have};
use crate::path::path_is_under_key;
use crate::prune::PrunePlan;
use crate::scope::{EntryKind, ExcludeSet, Scope};
use crate::workspace::{gather_workspace, map_new_paths, rescan_tracked_pruned_dirs};

/// 一轮 reconcile 的编排：拉齐 fstat / 工作区 / have 三路数据，分析、算摘要，
/// 最后把变更下发给 p4。多个范围入口合并成这一轮——一次操作、一个视图。
pub(crate) async fn reconcile_scope(
    options: &Options,
    scope: &Scope,
    cache: &mut WorkspaceCache,
    cache_writer: &mut Option<CacheWriter>,
) -> Result<()> {
    // cwd 取第一个入口的所在目录：`.p4config` / P4IGNORE 的发现跟着它走。
    let work_dir = scope.first_dir.as_str();

    sayln!(
        "Processing {} scope entr{}.",
        scope.includes.len(),
        if scope.includes.len() == 1 {
            "y"
        } else {
            "ies"
        }
    );
    if options.verbose {
        for entry in &scope.includes {
            sayln!("         Entry \"{}\"", entry.path);
        }
    }

    let specs = scope.file_specs();

    let (maybe_depot, maybe_workspace, maybe_have) = futures::join!(
        run_p4_fstat_all(options, work_dir, &specs),
        gather_workspace(options, scope),
        run_p4_have(options, work_dir, &specs)
    );

    // 第二项是 head 目标快照，只有 sync 模式才建（`head_action` 会被 fstat 的补查覆盖，
    // 只能在补查之前取）。
    let (mut depot, head_target): (DepotState, Option<TargetMap>) = maybe_depot?;
    let (mut workspace, prune_plan): (WorkspaceState, PrunePlan) = maybe_workspace?;
    let have_records: HashMap<String, HaveRecord> = maybe_have?;

    // 范围排除：depot 记录与目标快照用同一份名单、同时过滤，且都在分析之前。
    // 只过滤扫盘一侧的话，排除目录里已跟踪的文件会因为「本地扫不到」被误判成待删除。
    warn_unmatched_entries(scope, &depot)?;
    let excluded_depot_keys = exclude_from_depot(&mut depot, &scope.excludes);
    if !excluded_depot_keys.is_empty() {
        sayln!(
            "   Left {} depot-tracked file(s) outside the scope untouched.",
            excluded_depot_keys.len()
        );
    }
    let head_target = head_target.map(|mut target| {
        target.retain(|key, _| !excluded_depot_keys.contains(key));
        target
    });

    // 差异分析之前补回已剪目录里 depot 已跟踪的文件，否则它们会被当成被删除。
    rescan_tracked_pruned_dirs(
        options,
        work_dir,
        &depot,
        &prune_plan,
        &scope.excludes,
        &mut workspace,
    )
    .await?;

    if depot.file_records.is_empty() && workspace.num_files == 0 {
        sayln!("The folder contains no files that need checking.");
        return Ok(());
    }

    // sync 是另一套分类学与另一套动作（`analyze_at_target` / `SyncChanges`），在这里就
    // 分叉：下面 open / clean 那条路径一个字节都不动。
    if options.sync {
        let target = match options.to {
            Some(changelist) => {
                let mut target =
                    run_p4_fstat_at_revision(options, work_dir, changelist, &specs).await?;
                // 目标快照与 depot 记录用同一份排除名单，否则范围外文件会被当成
                // 「目标时刻不在库」而遭删除。
                target.retain(|key, _| !excluded_depot_keys.contains(key));
                // 空结果在分析层的意思是「目标时刻什么都不存在」，落到动作上就是删光本地
                // 每一个被跟踪的文件。depot 里明明有记录却查出空目标，那是查询坏了而不是
                // 用户的意图——真的一无所有的工作区，depot 记录本来就是空的。立场同
                // `--to 0` 被解析层拒绝：这个结论不该被静默执行。
                if target.is_empty() && !depot.file_records.is_empty() {
                    bail!(
                        "Changelist {changelist} has no files in this client's view, so syncing \
                         to it would delete all {num} local files this client tracks. \
                         Pick a later changelist.",
                        num = depot.file_records.len()
                    );
                }
                target
            }
            // 目标是 head：快照在取 depot 状态时顺手取好了。拿不到说明 fstat 那一步没按
            // sync 模式跑，是 bug 而不是用户输入问题。
            None => head_target
                .ok_or_else(|| anyhow!("Missing the head revision snapshot for sync mode"))?,
        };

        let plan = build_sync_changes(
            options,
            &depot,
            &workspace,
            &have_records,
            &target,
            cache,
            cache_writer,
        )
        .await?;

        // 算不出摘要的那批转交原生 `p4 sync`。两处细节：
        //
        // - 预演带着 `-f`：不带 `-f` 时 p4 会跳过已在 have list 上的文件，预演出来的
        //   集合比实际要小，而预演必须预告真实动作；
        // - `--to <CL>` 下把版本说明符钉在参数上，否则转交的这一批会跑到 head 去。
        if !plan.unsupported.is_empty() {
            sayln!(
                "Found {num_files} file(s) that are not supported by p4delta, running a manual sync",
                num_files = plan.unsupported.len()
            );

            let specs = handoff_specs(&plan.unsupported, options.to);

            let args: &[&str] = if options.apply {
                &["sync", "-f"]
            } else {
                &["sync", "-f", "-n"]
            };

            // 真跑时用 ExitCode 而不是 ExitCodeOrStderr：这里是 p4delta 把文件**转交**给
            // p4 自己做，而 p4 对「这批文件没什么可做的」也往 stderr 写字（退出码 0），
            // 那是正常结果而不是失败。判据同 open / clean 的转交点。
            let mode = if options.apply {
                FailureMode::ExitCode
            } else {
                FailureMode::Warn
            };
            run_p4_command_batched(options, work_dir, args, &specs, false, mode).await?;

            // 转交出去的文件必须出现在记录流里：消费方读到的是「这批我处理不了，
            // 交给原生 p4 了」，而不是「这批不存在」。
            emit_handoff_files(
                Mode::Sync,
                "sync",
                plan.unsupported.iter().cloned(),
                options.apply,
            );
        }

        if plan.changes.total() == 0 {
            // 转交出去的那批不算在 `changes` 里，但 `-a` 下它刚刚真的被执行过——这时说
            // 「一切与目标版本一致」就是自相矛盾，只能说「除此之外没有别的」。
            if plan.unsupported.is_empty() {
                sayln!("No files to sync, everything up to date.");
            } else {
                sayln!("No other files to sync.");
                // 这句话只在有东西可落地时才说：刚跑的那次转交是预演（带 `-n`）。
                if !options.apply {
                    sayln!("Re-run with -a to sync the workspace.");
                }
            }
            return Ok(());
        }

        return apply_sync(options, work_dir, &plan.changes).await;
    }

    sayln!("   Analyzing files for inconsistencies.");
    let start_time = Instant::now();

    let Analysis {
        mut changes,
        check_edit,
        check_revert_edit,
        check_revert_delete_or_reopen_edit,
        unsupported_files,
        unsynced_files,
        archived_files,
    } = analyze(&depot, &workspace, options.verbose)?;

    sayln!(
        "      Analysis complete in {} seconds.",
        start_time.elapsed().as_secs_f32()
    );
    emit_progress("analyze", None);

    // client view 的排除行会让 p4 完全看不见某些路径（例如 `-//aki/....tmp`），
    // 扫盘时它们却长得像新增文件。不剔掉的话，open 模式会去 add 一个 p4 拒绝的文件，
    // clean 模式会删掉一个 p4 根本不管的文件。
    // 顺手补上它们的 depot 路径：新增文件是唯一没有 depot 记录可查的一类，而契约里
    // `depotFile` 是必填项（`p4 where` 一次批量查询同时给出映射与排除名单）。
    changes.add = map_new_paths(options, work_dir, &changes.add).await?;

    // clean 模式不消费「已打开」的两组候选：`p4 clean` 不碰已打开的文件，
    // [`CleanChanges::project`] 把这两组的结论整个丢弃——为它们读文件算摘要、写缓存
    // 都是白烧在最贵的一批候选上。在任何摘要工作之前摘掉，open 模式一个字节不动。
    let (check_revert_edit, check_revert_delete_or_reopen_edit) = if options.clean {
        (Vec::new(), Vec::new())
    } else {
        (check_revert_edit, check_revert_delete_or_reopen_edit)
    };

    // 三组候选的时间戳快筛：判据相同，快筛与统计共用；「未改动」那批的归宿不同，
    // 留在各组自己手里（见下面三段）。
    let mut timestamps = TimestampStats::new(options.verbose);
    let edit = timestamps.filter(check_edit, &have_records, "check_edit");
    let revert_edit = timestamps.filter(check_revert_edit, &have_records, "check_revert_edit");
    let revert_delete = timestamps.filter(
        check_revert_delete_or_reopen_edit,
        &have_records,
        "check_revert_delete",
    );

    if let Some(summary) = timestamps.summary() {
        sayln!("{summary}");
    }

    // 进度提示只发这一处：三组摘要各自有独立的一行「Checking digests for N files」，
    // 但它们是同一段工作，消费方要看的是「还要读多少盘」这个总量。
    let digest_total =
        edit.needs_digest.len() + revert_edit.needs_digest.len() + revert_delete.needs_digest.len();
    if digest_total > 0 {
        emit_progress(
            "digest",
            Some(&format!("Checking digests for {digest_total} files.")),
        );
    }

    // 算摘要，看是否需要 open for edit。
    if !edit.needs_digest.is_empty() {
        sayln!("   Checking digests for {} files.", edit.needs_digest.len());

        let mut hashed = HashStats::new();
        let results = parallel_compute_digests(edit.needs_digest, cache, CachePolicy::Use)?;
        // 边算边存：这个阶段占了绝大部分运行时间，后面某步失败不该把它的成果整个丢掉。
        save_cache(cache_writer, cache, false)?;
        for outcome in results {
            hashed.record(outcome.from_cache, outcome.file.size);

            // 摘要不等就是 edit：工作区这一份已经不是 have 那一版了。
            if outcome.digest != expected_digest(&depot, outcome.file)? {
                changes.edit.push(outcome.file.path.clone());

                if options.verbose {
                    sayln!("         File \"{}\" digest is wrong.", outcome.file.path);
                }
            }
        }
        hashed.report();
    }

    // 未改动的那批直接 revert，其余算摘要定夺。
    for (file, _) in revert_edit.unchanged {
        changes.revert_edit.push(file.path.clone());
        if options.verbose {
            sayln!(
                "         File \"{}\" unchanged since sync (revert).",
                file.path
            );
        }
    }

    if !revert_edit.needs_digest.is_empty() {
        sayln!(
            "   Checking digests for {} files.",
            revert_edit.needs_digest.len()
        );

        let mut hashed = HashStats::new();
        let results = parallel_compute_digests(revert_edit.needs_digest, cache, CachePolicy::Use)?;
        save_cache(cache_writer, cache, false)?;
        for outcome in results {
            hashed.record(outcome.from_cache, outcome.file.size);

            // 这一组反过来：摘要与 have 相等才是「打开了编辑却没改」，该 revert。
            if outcome.digest == expected_digest(&depot, outcome.file)? {
                changes.revert_edit.push(outcome.file.path.clone());

                if options.verbose {
                    sayln!("         File \"{}\" digest is correct.", outcome.file.path);
                }
            }
        }
        hashed.report();
    }

    // 未改动的那批直接 revert delete，其余算摘要定夺。
    for (file, _) in revert_delete.unchanged {
        changes.revert_delete.push(file.path.clone());
        if options.verbose {
            sayln!(
                "         File \"{}\" unchanged since sync (revert delete).",
                file.path
            );
        }
    }

    if !revert_delete.needs_digest.is_empty() {
        sayln!(
            "   Checking digests for {} files.",
            revert_delete.needs_digest.len()
        );

        let mut hashed = HashStats::new();
        let results =
            parallel_compute_digests(revert_delete.needs_digest, cache, CachePolicy::Use)?;
        save_cache(cache_writer, cache, false)?;
        for outcome in results {
            hashed.record(outcome.from_cache, outcome.file.size);

            // 打开删除、本地文件却还在：摘要与 have 相等说明内容没动过，撤销那次删除；
            // 不等则是本地又改了，重新打开成 edit 才跟得上内容。
            if outcome.digest == expected_digest(&depot, outcome.file)? {
                changes.revert_delete.push(outcome.file.path.clone());

                if options.verbose {
                    sayln!("         File \"{}\" digest is correct.", outcome.file.path);
                }
            } else {
                changes.reopen_edit.push(outcome.file.path.clone());

                if options.verbose {
                    sayln!("         File \"{}\" digest is wrong.", outcome.file.path);
                }
            }
        }
        hashed.report();
    }

    // clean 模式下只有「未打开」的三类要动作，投影之后的数目才是这一轮真正要处理的。
    let clean_changes = if options.clean {
        Some(CleanChanges::project(&changes, &depot)?)
    } else {
        None
    };
    let sum_changes = match &clean_changes {
        Some(clean) => clean.total(),
        None => changes.total(),
    };

    if !unsynced_files.is_empty() {
        sayln!(
            "Found {} file(s) in the depot this client has never synced; sync or resolve them, then re-run.",
            unsynced_files.len()
        );
    }

    if !archived_files.is_empty() {
        sayln!(
            "Skipped {} archived file(s): their contents live in an archive depot, so there is nothing to compare.",
            archived_files.len()
        );
    }

    if !unsupported_files.is_empty() {
        sayln!(
            "Found {num_files} file(s) that are not supported by p4delta, running a manual {command}",
            command = if options.clean { "clean" } else { "reconcile" },
            num_files = unsupported_files.len()
        );
        let unsupported_paths: Vec<_> = unsupported_files
            .iter()
            .map(|rec| rec.depot_file.to_owned())
            .collect();

        // 预演的方向必须和实际动作一致：`p4 clean -n` 与 `p4 reconcile -n` 对同一批文件
        // 给出的预告正好相反，用错方向的预演比没有预演更糟。
        //
        // open 模式的预演额外要 `-Mj -Ztag`：那批文件的逐条动作只有 p4 自己知道，而预演
        // 的全部意义就是预告真实动作，不能只报一句「这批转交了」就把它们从清单里抹掉。
        // `-Ztag` 不能省——只给 `-Mj` 时 p4 回的是 `{"data":"//depot/… - opened for edit",
        // "level":0}` 这种给人读的行，没有 `depotFile` / `clientFile` 字段，翻译层会把每一
        // 行都丢掉（那正是这段代码曾经整个死掉的原因）。
        // 两个都是全局选项，得走在命令名前面（与 `p4 where` / `p4 info` 的调用同款）。
        let args: &[&str] = match (options.clean, options.apply) {
            (true, true) => &["clean"],
            (true, false) => &["clean", "-n"],
            (false, true) => &["reconcile"],
            (false, false) => &["-Mj", "-Ztag", "reconcile", "-n"],
        };

        // clean 的两支不能带 changelist：`p4 clean` 不接受 `-c`。
        // 预演（`-n`）沿用宽松契约——它不改状态，报错只告警；真跑时失败就是失败。
        //
        // 真跑时用 ExitCode 而不是 ExitCodeOrStderr：这里是 p4delta 把文件**转交**给 p4
        // 自己做，而 p4 对「这批文件没什么可做的」也往 stderr 写
        // `<path> - no file(s) to reconcile.`（退出码 0），那是正常结果而不是失败。
        let mode = if options.apply {
            FailureMode::ExitCode
        } else {
            FailureMode::Warn
        };
        let lines = run_p4_command_batched(
            options,
            work_dir,
            args,
            &unsupported_paths,
            !options.clean,
            mode,
        )
        .await?;

        // 转交出去的文件必须出现在记录流里，绝不静默丢。open 模式的预演带回了逐条记录，
        // 翻译成正常文件记录；其余三支只能说清「这批交给谁了」。
        match (options.clean, options.apply) {
            (false, false) => {
                emit_native_reconcile_records(Mode::Open, &lines, options.apply);
            }
            (clean, _) => emit_handoff_files(
                if clean { Mode::Clean } else { Mode::Open },
                if clean { "clean" } else { "reconcile" },
                unsupported_files.iter().map(|rec| HandoffFile {
                    depot_file: rec.depot_file.clone(),
                    client_file: rec.client_file.clone(),
                }),
                options.apply,
            ),
        }
    }

    emit_progress("report", None);

    if sum_changes == 0 {
        sayln!(
            "{}",
            if options.clean {
                "No files to clean, everything up to date."
            } else {
                "No changes to apply, everything up to date."
            }
        );
        return Ok(());
    }

    match &clean_changes {
        Some(clean) => apply_clean(options, work_dir, clean).await?,
        None => apply_changes(options, work_dir, &changes, &depot).await?,
    }

    Ok(())
}

/// 入口在 depot 与本地都找不到任何东西时提示一句：多半是路径拼错了。
///
/// 判据要求「本地不存在」且「depot 无记录」，所以空目录这类合法情况（还没同步过，
/// 或纯本地的全新目录）同时满足两者，也会被提示——因此只要还有别的入口能干活，
/// 就只是警告，不影响它们。本地存在的入口一定有东西可查，直接跳过，省掉大工作区里的
/// 整表扫描。
fn warn_unmatched_entries(scope: &Scope, depot: &DepotState) -> Result<()> {
    let unmatched: Vec<&str> = scope
        .includes
        .iter()
        .filter(|entry| !Path::new(&entry.path).exists())
        .filter(|entry| match entry.kind {
            // 文件入口是一次建好索引的查表；目录入口要问「这棵子树里有没有」，只能扫。
            EntryKind::File => depot.get_client_record(&entry.path_lower).is_none(),
            EntryKind::Directory => !depot.file_records.iter().any(|record| {
                record.client_file_lower == entry.path_lower
                    || path_is_under_key(&record.client_file_lower, &entry.path_lower)
            }),
        })
        .map(|entry| entry.path.as_str())
        .collect();

    if unmatched.is_empty() {
        set_scope_matched(scope.includes.len());
        return Ok(());
    }

    set_scope_matched(scope.includes.len() - unmatched.len());
    for path in &unmatched {
        emit_unmatched(path);
    }

    // 一条都没匹配上就是「什么都没做」，不能报成功：用户会把 exit 0 当成「处理完了」，
    // 而真相多半是路径拼错了。部分匹配则是另一回事，其余入口照常处理。
    if unmatched.len() == scope.includes.len() {
        // 消费方要能把这档与「跑挂了」分开：入口全落空是一个**完整的答案**（这里确实
        // 什么都没有），编辑器把成对的 `[<path>, <path>/...]` 发过来时就靠它下结论。
        set_reason("no-entry-matched");
        bail!(
            "Nothing to work on: {} scope entr{} matched nothing in the depot or on disk \
             (misspelled?):\n  {}",
            unmatched.len(),
            if unmatched.len() == 1 { "y" } else { "ies" },
            unmatched.join("\n  ")
        );
    }

    eprintln!(
        "Warning: {} scope entr{} matched nothing in the depot or on disk (misspelled?):",
        unmatched.len(),
        if unmatched.len() == 1 { "y" } else { "ies" }
    );
    for path in unmatched {
        eprintln!("  {path}");
    }

    Ok(())
}

/// 按范围排除过滤 depot 记录，返回被剔除的 depot 键（目标快照要用同一份名单过滤）。
///
/// 这一步与扫盘侧对 exclude 的处理是一对，缺一不可：少了它，排除目录里已跟踪的
/// 文件会因为「本地扫不到」被误判成待删除。
fn exclude_from_depot(depot: &mut DepotState, excludes: &ExcludeSet) -> HashSet<String> {
    if excludes.is_empty() {
        return HashSet::new();
    }

    let mut excluded = HashSet::new();
    depot.retain_records(|record| {
        if excludes.excludes_key(&record.client_file_lower) {
            excluded.insert(record.depot_file_lower.clone());
            false
        } else {
            true
        }
    });

    excluded
}

// ---- 摘要阶段的小工具 ----
//
// open / clean 的三组候选（edit / revert edit / revert delete or reopen edit）判据与动作
// 各不相同，刻意不合并；只抽出真正一样的两块：时间戳快筛（含统计与文案），
// 以及「期望摘要 + 读盘计价」。

/// 一组候选经过时间戳快筛后的两半。
struct FilteredCandidates<'a> {
    /// 需要真算摘要的。
    needs_digest: Vec<(&'a WorkspaceFile, DigestType)>,

    /// 被时间戳捷径**视为**未改动的：不读文件就能下结论。
    ///
    /// 捷径只保证 mtime 与 have 的 syncTime 相差不超过一秒，那不是证明。后两组里
    /// 这批直接成为 revert 动作，edit 组里则只是被丢掉——归宿不同，由调用方按组处理。
    unchanged: Vec<(&'a WorkspaceFile, DigestType)>,
}

/// 时间戳快筛的累计账目：三组共用一份，末尾合成一行汇总。
struct TimestampStats {
    verbose: bool,
    candidates: usize,
    skipped: usize,
}

impl TimestampStats {
    fn new(verbose: bool) -> Self {
        TimestampStats {
            verbose,
            candidates: 0,
            skipped: 0,
        }
    }

    /// 快筛一组候选。`phase` 是 verbose 文案里的组名，三组各自保留自己的名字
    /// （第三组的字段叫 `check_revert_delete_or_reopen_edit`，文案里是 `check_revert_delete`）。
    fn filter<'a>(
        &mut self,
        candidates: Vec<(&'a WorkspaceFile, DigestType)>,
        have_records: &HashMap<String, HaveRecord>,
        phase: &str,
    ) -> FilteredCandidates<'a> {
        self.candidates += candidates.len();

        let (unchanged, needs_digest): (Vec<_>, Vec<_>) = candidates
            .into_iter()
            .partition(|(file, _)| is_unchanged_since_sync(file, have_records));

        self.skipped += unchanged.len();

        if !unchanged.is_empty() && self.verbose {
            sayln!(
                "   Skipped {} file(s) unchanged since sync ({phase}).",
                unchanged.len()
            );
        }

        FilteredCandidates {
            needs_digest,
            unchanged,
        }
    }

    /// 汇总行；一个都没跳过时不作声。跳过的文件是候选的子集，所以除数非零，
    /// `checked_div` 只是不让它成为前提。
    fn summary(&self) -> Option<String> {
        (self.skipped > 0).then(|| {
            let percentage = (self.skipped * 100)
                .checked_div(self.candidates)
                .unwrap_or(0);
            format!(
                "   Timestamp optimization: Skipped {} of {} digest computations ({}%).",
                self.skipped, self.candidates, percentage
            )
        })
    }
}

/// 一条候选的期望摘要：从 depot 记录里取。取不到就是分析结果与 depot 状态对不上，
/// 期望值不存在时「相等」与「不等」两个结论都是假的，必须响亮失败。
fn expected_digest(depot: &DepotState, file: &WorkspaceFile) -> Result<[u8; 16]> {
    let record = depot
        .get_client_record(&file.path_lower)
        .ok_or_else(|| anyhow!("Failed to find depot record for {}", file.path))?;

    record
        .digest
        .ok_or_else(|| anyhow!("Missing digest for {}", record.depot_file))
}

/// 一段摘要阶段的读盘账目，收尾时合成 `Hashed` 那行。open / clean 与 sync 共用。
struct HashStats {
    total_size: u64,
    start: Instant,
}

impl HashStats {
    fn new() -> Self {
        HashStats {
            total_size: 0,
            start: Instant::now(),
        }
    }

    /// 记一条摘要结果。缓存命中没有产生任何读盘，不能算进这一段的计价里。
    fn record(&mut self, from_cache: bool, size: u64) {
        if !from_cache {
            self.total_size += size;
        }
    }

    /// 一个字都没读时不作声——缓存全命中时那行本来就该缺席。
    fn report(&self) {
        if self.total_size > 0 {
            sayln!(
                "      Hashed {} in {} seconds.",
                format_size(self.total_size, BINARY),
                self.start.elapsed().as_secs_f32()
            );
        }
    }
}

// ---- 从工作区删除文件 ----
//
// 放在这里而不是留在 clean 里：sync 也有「从工作区删文件」这一类动作（它删的是
// 目标版本里没有的文件，clean 删的是 depot 里没有的），动作本身的实现只该有一份。

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

/// 从磁盘删除一批工作区文件。
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

/// 转交给原生 p4 的文件规格：`--to <CL>` 下把版本说明符钉在参数上。
///
/// 不钉的话这一批会跑到 head 去——`--to` 下那正是要避免的（拉到比目标更新的版本），
/// 而且从输出上看不出来：文件确实被「转交」了，只是转交错了地方。
fn handoff_specs(files: &[HandoffFile], to: Option<u32>) -> Vec<String> {
    files
        .iter()
        .map(|file| match to {
            Some(changelist) => format!("{}@{changelist}", file.depot_file),
            None => file.depot_file.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::{Duration, UNIX_EPOCH};

    use crate::model::DepotFileRecord;
    use crate::path::local_path_key;
    use crate::test_util::{TempTree, depot_record, symlink_file};

    // ---- 摘要阶段的小工具 ----

    /// 一个 mtime 距 UNIX_EPOCH 指定秒数的工作区文件。
    fn dated_file(path: &str, modified: u64) -> WorkspaceFile {
        WorkspaceFile {
            path: path.to_owned(),
            path_lower: local_path_key(path),
            date: UNIX_EPOCH + Duration::from_secs(modified),
            ..Default::default()
        }
    }

    /// 一条带 syncTime 的 have 记录，键与 [`dated_file`] 同口径。
    fn have_synced_at(path: &str, sync_time: u64) -> HashMap<String, HaveRecord> {
        HashMap::from([(
            local_path_key(path),
            HaveRecord {
                sync_time: Some(sync_time),
            },
        )])
    }

    /// 时间戳快筛把候选切成「要算摘要」与「未改动」两半，并把两组数都记进整轮账目。
    #[test]
    fn the_timestamp_filter_splits_candidates_and_counts_them() {
        let untouched = dated_file(r"C:\ws\untouched.txt", 100);
        let touched = dated_file(r"C:\ws\touched.txt", 100);
        // 只有 touched 的 syncTime 差得远：untouched 与 have 同一时刻，走得掉捷径。
        let mut have_records = have_synced_at(&untouched.path, 100);
        have_records.extend(have_synced_at(&touched.path, 3_600));

        let mut stats = TimestampStats::new(false);
        let filtered = stats.filter(
            vec![(&untouched, DigestType::Text), (&touched, DigestType::Text)],
            &have_records,
            "check_edit",
        );

        assert_eq!(filtered.unchanged.len(), 1);
        assert_eq!(filtered.unchanged[0].0.path, untouched.path);
        assert_eq!(filtered.needs_digest.len(), 1);
        assert_eq!(filtered.needs_digest[0].0.path, touched.path);

        // 账目按整轮算：跳过的与候选的总数都留着给末尾那行汇总用。
        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.candidates, 2);
    }

    /// 汇总行只在确实跳过过东西时出现；百分比按整轮的候选数算。
    #[test]
    fn the_timestamp_summary_reports_the_skipped_share() {
        let mut stats = TimestampStats::new(false);
        assert_eq!(stats.summary(), None, "一个都没跳过时不该有汇总行");

        stats.candidates = 4;
        stats.skipped = 1;
        let summary = stats.summary().expect("跳过了就该有汇总行");
        assert!(
            summary.contains("Skipped 1 of 4 digest computations (25%)"),
            "{summary}"
        );
        // 前导空格是输出契约的一部分：和上下文那几行对齐。
        assert!(
            summary.starts_with("   Timestamp optimization: "),
            "{summary}"
        );

        // 全部跳过（分子等于分母）是合法的：100% 不该成为除零的牺牲品。
        stats.skipped = 4;
        let summary = stats.summary().expect("跳过了就该有汇总行");
        assert!(summary.contains("(100%)"), "{summary}");
    }

    /// 期望摘要必须从 depot 记录里取，两种取不到都要响亮失败：期望值不存在时，
    /// 「相等」与「不等」两个结论都是假的，将就其中任何一个都会静默给出错误动作。
    #[test]
    fn the_expected_digest_comes_from_the_depot_record() {
        let mut depot = DepotState::default();
        depot.file_records = vec![DepotFileRecord {
            digest: Some([0x5A; 16]),
            ..depot_record("a.txt")
        }];
        depot.build_mapping();

        let file = dated_file("a.txt", 100);
        assert_eq!(expected_digest(&depot, &file).unwrap(), [0x5A; 16]);

        let unknown = dated_file("missing.txt", 100);
        let error = expected_digest(&depot, &unknown).expect_err("没有记录必须报错");
        assert!(
            error.to_string().contains("Failed to find depot record"),
            "{error}"
        );

        // 记录在、摘要缺：fstat 的补查没做全，同样不能将就。
        let mut without_digest = DepotState::default();
        without_digest.file_records = vec![depot_record("a.txt")];
        without_digest.build_mapping();

        let error = expected_digest(&without_digest, &file).expect_err("缺摘要必须报错");
        assert!(error.to_string().contains("Missing digest"), "{error}");
    }

    /// 读盘计价只算真的读过的那份：缓存命中没有产生读盘，不能算进去。
    #[test]
    fn hash_stats_only_count_what_was_read() {
        let mut hashed = HashStats::new();
        hashed.record(true, 4096);
        hashed.record(false, 100);
        hashed.record(false, 23);

        assert_eq!(hashed.total_size, 123);
    }

    /// 转交出去的文件在 `--to` 下必须钉住目标 changelist。
    ///
    /// 漏掉 `@{CL}` 的话这批文件会被同步到 **head**——比目标更新的版本，正是 `--to` 要
    /// 避免的事，而且用户从输出上看不出来：文件确实被转交了，只是转交错了地方。
    #[test]
    fn handoff_specs_pin_the_target_changelist() {
        let file = |depot_file: &str, client_file: &str| HandoffFile {
            depot_file: depot_file.to_owned(),
            client_file: client_file.to_owned(),
        };

        let files = [
            file("//depot/a.txt", r"C:\ws\a.txt"),
            file("//depot/b/c.txt", r"C:\ws\b\c.txt"),
        ];

        assert_eq!(
            handoff_specs(&files, Some(1234)),
            ["//depot/a.txt@1234", "//depot/b/c.txt@1234"]
        );
        // head 目标下不加后缀：加了反而变成「拉到 1234 那一版」，与目标不符。
        assert_eq!(
            handoff_specs(&files, None),
            ["//depot/a.txt", "//depot/b/c.txt"]
        );
    }

    #[test]
    fn deleting_a_workspace_file_removes_it() {
        let tree = TempTree::new("delete-entry");
        let file = tree.file("a.txt", "x");

        remove_workspace_entry(&file).unwrap();

        assert!(!file.exists());
    }

    /// 幂等：大小写冲突下同一个文件可能被推入两次，第二次不能算失败。
    #[test]
    fn deleting_a_missing_file_is_not_an_error() {
        let tree = TempTree::new("delete-entry-missing");

        remove_workspace_entry(&tree.root.join("never-existed.txt")).unwrap();
    }

    #[test]
    fn a_read_only_file_is_deleted() {
        let tree = TempTree::new("delete-entry-read-only");
        let file = tree.file("a.txt", "x");

        let mut permissions = std::fs::metadata(&file).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&file, permissions).unwrap();

        remove_workspace_entry(&file).unwrap();

        assert!(!file.exists());
    }

    #[test]
    fn deleting_a_symlink_removes_the_link_not_the_target() {
        let tree = TempTree::new("delete-entry-symlink");
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
        let tree = TempTree::new("delete-files-partial");
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
