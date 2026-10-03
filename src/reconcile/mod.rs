//! 单个目录的 reconcile 编排。

mod analyze;
mod changes;
mod clean;
mod sync;

use std::collections::HashMap;
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
use crate::model::{DepotState, HaveRecord, TargetMap, WorkspaceCache, WorkspaceState};
use crate::p4::fstat::{run_p4_fstat_all, run_p4_fstat_at_revision};
use crate::p4::process::{FailureMode, run_p4_command_batched, run_p4_have};
use crate::prune::PrunePlan;
use crate::workspace::{filter_unmapped_paths, gather_workspace, rescan_tracked_pruned_dirs};

/// Performs reconcile for a single directory. Ties everything else together.
pub(crate) async fn reconcile_dir(
    options: &Options,
    work_dir: &str,
    cache: &mut WorkspaceCache,
    cache_writer: &mut Option<CacheWriter>,
) -> Result<()> {
    println!("Processing path \"{}\".", work_dir);

    let (maybe_depot, maybe_workspace, maybe_have) = futures::join!(
        run_p4_fstat_all(options, work_dir),
        gather_workspace(options, work_dir),
        run_p4_have(options, work_dir)
    );

    // 第二项是 head 目标快照，只有 sync 模式才建（`head_action` 会被 fstat 的补查覆盖，
    // 只能在补查之前取）。
    let (depot, head_target): (DepotState, Option<TargetMap>) = maybe_depot?;
    let (mut workspace, prune_plan): (WorkspaceState, PrunePlan) = maybe_workspace?;
    let have_records: HashMap<String, HaveRecord> = maybe_have?;

    // 差异分析之前补回已剪目录里 depot 已跟踪的文件，否则它们会被当成被删除。
    rescan_tracked_pruned_dirs(options, work_dir, &depot, &prune_plan, &mut workspace).await?;

    if depot.file_records.is_empty() && workspace.num_files == 0 {
        println!("The folder contains no files that need checking.");
        return Ok(());
    }

    // sync 是另一套分类学与另一套动作（`analyze_at_target` / `SyncChanges`），在这里就
    // 分叉：下面 open / clean 那条路径一个字节都不动。
    if options.sync {
        let target = match options.to {
            Some(changelist) => {
                let target = run_p4_fstat_at_revision(options, work_dir, changelist).await?;
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
            println!(
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
        }

        if plan.changes.total() == 0 {
            // 转交出去的那批不算在 `changes` 里，但 `-a` 下它刚刚真的被执行过——这时说
            // 「一切与目标版本一致」就是自相矛盾，只能说「除此之外没有别的」。
            if plan.unsupported.is_empty() {
                println!("No files to sync, everything up to date.");
            } else {
                println!("No other files to sync.");
                // 这句话只在有东西可落地时才说：刚跑的那次转交是预演（带 `-n`）。
                if !options.apply {
                    println!("Re-run with -a to sync the workspace.");
                }
            }
            return Ok(());
        }

        return apply_sync(options, work_dir, &plan.changes).await;
    }

    println!("   Analyzing files for inconsistencies.");
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

    println!(
        "      Analysis complete in {} seconds.",
        start_time.elapsed().as_secs_f32()
    );

    // client view 的排除行会让 p4 完全看不见某些路径（例如 `-//aki/....tmp`），
    // 扫盘时它们却长得像新增文件。不剔掉的话，open 模式会去 add 一个 p4 拒绝的文件，
    // clean 模式会删掉一个 p4 根本不管的文件。
    changes.add = filter_unmapped_paths(options, work_dir, &changes.add).await?;

    // Track timestamp optimization stats
    let mut total_skipped = 0;
    let mut total_candidates = 0;

    // Filter files using timestamp optimization
    let check_edit_filtered = if !check_edit.is_empty() {
        let original_count = check_edit.len();
        total_candidates += original_count;

        let (unchanged, needs_digest): (Vec<_>, Vec<_>) = check_edit
            .into_iter()
            .partition(|(file, _)| is_unchanged_since_sync(file, &have_records));

        total_skipped += unchanged.len();

        if !unchanged.is_empty() && options.verbose {
            println!(
                "   Skipped {} file(s) unchanged since sync (check_edit).",
                unchanged.len()
            );
        }

        needs_digest
    } else {
        Vec::new()
    };

    let (check_revert_edit_filtered, check_revert_edit_reverted) = if !check_revert_edit.is_empty()
    {
        let original_count = check_revert_edit.len();
        total_candidates += original_count;

        let (unchanged, needs_digest): (Vec<_>, Vec<_>) = check_revert_edit
            .into_iter()
            .partition(|(file, _)| is_unchanged_since_sync(file, &have_records));

        total_skipped += unchanged.len();

        if !unchanged.is_empty() && options.verbose {
            println!(
                "   Skipped {} file(s) unchanged since sync (check_revert_edit).",
                unchanged.len()
            );
        }

        (needs_digest, unchanged)
    } else {
        (Vec::new(), Vec::new())
    };

    let (check_revert_delete_filtered, check_revert_delete_reverted) =
        if !check_revert_delete_or_reopen_edit.is_empty() {
            let original_count = check_revert_delete_or_reopen_edit.len();
            total_candidates += original_count;

            let (unchanged, needs_digest): (Vec<_>, Vec<_>) = check_revert_delete_or_reopen_edit
                .into_iter()
                .partition(|(file, _)| is_unchanged_since_sync(file, &have_records));

            total_skipped += unchanged.len();

            if !unchanged.is_empty() && options.verbose {
                println!(
                    "   Skipped {} file(s) unchanged since sync (check_revert_delete).",
                    unchanged.len()
                );
            }

            (needs_digest, unchanged)
        } else {
            (Vec::new(), Vec::new())
        };

    if total_skipped > 0 {
        // Skipped files are a subset of the candidates, so the divisor is non-zero here;
        // `checked_div` keeps that from being load-bearing.
        let percentage = (total_skipped * 100)
            .checked_div(total_candidates)
            .unwrap_or(0);
        println!(
            "   Timestamp optimization: Skipped {} of {} digest computations ({}%).",
            total_skipped, total_candidates, percentage
        );
    }

    // Phase 2: Compute digests for files that need checking

    // Compute digests to see if we need to open files for edit.
    if !check_edit_filtered.is_empty() {
        println!(
            "   Checking digests for {} files.",
            check_edit_filtered.len()
        );

        let start_time = Instant::now();
        let mut total_size = 0;

        let results = parallel_compute_digests(check_edit_filtered, cache, CachePolicy::Use)?;
        // Persist as we go: this phase dominates the runtime, and a later failure must not
        // discard everything it produced.
        save_cache(cache_writer, cache, false)?;
        for result in results {
            if !result.2 {
                total_size += result.0.size;
            }

            let record = depot
                .get_client_record(&result.0.path_lower)
                .ok_or_else(|| anyhow!("Failed to find depot record for {}", result.0.path))?;
            let expected_digest = record
                .digest
                .ok_or_else(|| anyhow!("Missing digest for {}", record.depot_file))?;
            if result.1 != expected_digest {
                changes.edit.push(result.0.path.clone());

                if options.verbose {
                    println!("         File \"{}\" digest is wrong.", result.0.path);
                }
            }
        }

        if total_size > 0 {
            println!(
                "      Hashed {} in {} seconds.",
                format_size(total_size, BINARY),
                start_time.elapsed().as_secs_f32()
            );
        }
    }

    // Compute digests to see if we need to revert files open for edit.
    // First add the files we already know are unchanged
    for (file, _) in check_revert_edit_reverted {
        changes.revert_edit.push(file.path.clone());
        if options.verbose {
            println!(
                "         File \"{}\" unchanged since sync (revert).",
                file.path
            );
        }
    }

    if !check_revert_edit_filtered.is_empty() {
        println!(
            "   Checking digests for {} files.",
            check_revert_edit_filtered.len()
        );

        let start_time = Instant::now();
        let mut total_size = 0;

        let results =
            parallel_compute_digests(check_revert_edit_filtered, cache, CachePolicy::Use)?;
        save_cache(cache_writer, cache, false)?;
        for result in results {
            if !result.2 {
                total_size += result.0.size;
            }

            let record = depot
                .get_client_record(&result.0.path_lower)
                .ok_or_else(|| anyhow!("Failed to find depot record for {}", result.0.path))?;
            let expected_digest = record
                .digest
                .ok_or_else(|| anyhow!("Missing digest for {}", record.depot_file))?;
            if result.1 == expected_digest {
                changes.revert_edit.push(result.0.path.clone());

                if options.verbose {
                    println!("         File \"{}\" digest is correct.", result.0.path);
                }
            }
        }

        if total_size > 0 {
            println!(
                "      Hashed {} in {} seconds.",
                format_size(total_size, BINARY),
                start_time.elapsed().as_secs_f32()
            );
        }
    }

    // Compute digests to see if we need to revert deletes or reopen files for edit.
    // First add the files we already know are unchanged (revert delete)
    for (file, _) in check_revert_delete_reverted {
        changes.revert_delete.push(file.path.clone());
        if options.verbose {
            println!(
                "         File \"{}\" unchanged since sync (revert delete).",
                file.path
            );
        }
    }

    if !check_revert_delete_filtered.is_empty() {
        println!(
            "   Checking digests for {} files.",
            check_revert_delete_filtered.len()
        );

        let start_time = Instant::now();
        let mut total_size = 0;

        let results =
            parallel_compute_digests(check_revert_delete_filtered, cache, CachePolicy::Use)?;
        save_cache(cache_writer, cache, false)?;
        for result in results {
            if !result.2 {
                total_size += result.0.size;
            }

            let record = depot
                .get_client_record(&result.0.path_lower)
                .ok_or_else(|| anyhow!("Failed to find depot record for {}", result.0.path))?;
            let expected_digest = record
                .digest
                .ok_or_else(|| anyhow!("Missing digest for {}", record.depot_file))?;
            if result.1 == expected_digest {
                changes.revert_delete.push(result.0.path.clone());

                if options.verbose {
                    println!("         File \"{}\" digest is correct.", result.0.path);
                }
            } else {
                changes.reopen_edit.push(result.0.path.clone());

                if options.verbose {
                    println!("         File \"{}\" digest is wrong.", result.0.path);
                }
            }
        }

        if total_size > 0 {
            println!(
                "      Hashed {} in {} seconds.",
                format_size(total_size, BINARY),
                start_time.elapsed().as_secs_f32()
            );
        }
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
        println!(
            "Found {} file(s) in the depot this client has never synced; sync or resolve them, then re-run.",
            unsynced_files.len()
        );
    }

    if !archived_files.is_empty() {
        println!(
            "Skipped {} archived file(s): their contents live in an archive depot, so there is nothing to compare.",
            archived_files.len()
        );
    }

    if !unsupported_files.is_empty() {
        println!(
            "Found {num_files} file(s) that are not supported by p4delta, running a manual {command}",
            command = if options.clean { "clean" } else { "reconcile" },
            num_files = unsupported_files.len()
        );
        let unsupported_paths: Vec<_> = unsupported_files
            .into_iter()
            .map(|rec| rec.depot_file.to_owned())
            .collect();

        // 预演的方向必须和实际动作一致：`p4 clean -n` 与 `p4 reconcile -n` 对同一批文件
        // 给出的预告正好相反，用错方向的预演比没有预演更糟。
        let args: &[&str] = match (options.clean, options.apply) {
            (true, true) => &["clean"],
            (true, false) => &["clean", "-n"],
            (false, true) => &["reconcile"],
            (false, false) => &["reconcile", "-n"],
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
        run_p4_command_batched(
            options,
            work_dir,
            args,
            &unsupported_paths,
            !options.clean,
            mode,
        )
        .await?;
    }

    if sum_changes == 0 {
        println!(
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
        None => apply_changes(options, work_dir, &changes).await?,
    }

    Ok(())
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
fn handoff_specs(depot_files: &[String], to: Option<u32>) -> Vec<String> {
    depot_files
        .iter()
        .map(|depot_file| match to {
            Some(changelist) => format!("{depot_file}@{changelist}"),
            None => depot_file.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_util::{TempTree, symlink_file};

    /// 转交出去的文件在 `--to` 下必须钉住目标 changelist。
    ///
    /// 漏掉 `@{CL}` 的话这批文件会被同步到 **head**——比目标更新的版本，正是 `--to` 要
    /// 避免的事，而且用户从输出上看不出来：文件确实被转交了，只是转交错了地方。
    #[test]
    fn handoff_specs_pin_the_target_changelist() {
        let files = ["//depot/a.txt".to_owned(), "//depot/b/c.txt".to_owned()];

        assert_eq!(
            handoff_specs(&files, Some(1234)),
            ["//depot/a.txt@1234", "//depot/b/c.txt@1234"]
        );
        // head 目标下不加后缀：加了反而变成「拉到 1234 那一版」，与目标不符。
        assert_eq!(handoff_specs(&files, None), files);
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
