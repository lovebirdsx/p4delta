//! 单个目录的 reconcile 编排。

mod analyze;
mod changes;
mod clean;

use std::collections::HashMap;
use std::time::Instant;

use anyhow::{Result, anyhow};

use humansize::{BINARY, format_size};

use analyze::{Analysis, analyze};
use changes::apply_changes;
use clean::{CleanChanges, apply_clean};

use crate::cache::{CacheWriter, save_cache};
use crate::cli::Options;
use crate::digest::{is_unchanged_since_sync, parallel_compute_digests};
use crate::model::{DepotState, HaveRecord, WorkspaceCache, WorkspaceState};
use crate::p4::fstat::run_p4_fstat_all;
use crate::p4::process::{run_p4_command_batched, run_p4_have};
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

    let depot: DepotState = maybe_depot?;
    let (mut workspace, prune_plan): (WorkspaceState, PrunePlan) = maybe_workspace?;
    let have_records: HashMap<String, HaveRecord> = maybe_have?;

    // 差异分析之前补回已剪目录里 depot 已跟踪的文件，否则它们会被当成被删除。
    rescan_tracked_pruned_dirs(options, work_dir, &depot, &prune_plan, &mut workspace).await?;

    if depot.file_records.is_empty() && workspace.num_files == 0 {
        println!("The folder contains no files that need checking.");
        return Ok(());
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

        let results = parallel_compute_digests(check_edit_filtered, cache)?;
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

        let results = parallel_compute_digests(check_revert_edit_filtered, cache)?;
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

        let results = parallel_compute_digests(check_revert_delete_filtered, cache)?;
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
        run_p4_command_batched(
            options,
            work_dir,
            args,
            &unsupported_paths,
            !options.clean,
            false,
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
