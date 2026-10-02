//! 工作区文件的收集与过滤。

use std::collections::HashSet;
use std::time::Instant;

use anyhow::Result;
use humansize::{BINARY, format_size};
use walkdir::WalkDir;

use crate::cli::Options;
use crate::model::{DepotState, WorkspaceFile, WorkspaceState};
use crate::p4::process::{FailureMode, command_line_ready_paths, run_p4_command_batched};
use crate::path::{local_path_key, normalize_local_path_owned, path_is_under_key};
use crate::prune::{
    IGNORES_ARGS, PrunePlan, has_pruned_ancestor, parse_ignores_output, plan_directory_pruning,
};

/// 扫描工作区文件。`skip_dirs` 里的目录及其子树会被跳过（目录级 ignore 剪枝的结果）。
pub(crate) fn collect_workspace_files(
    work_dir: &str,
    skip_dirs: &[String],
) -> Result<(Vec<WorkspaceFile>, usize, u64)> {
    let mut files = Vec::new();
    let mut num_dirs = 0;
    let mut total_size = 0;
    let skip_keys: HashSet<String> = skip_dirs.iter().map(|dir| local_path_key(dir)).collect();

    let mut walker = WalkDir::new(work_dir).into_iter();
    while let Some(entry) = walker.next() {
        let entry = entry?;
        let file_type = entry.file_type();

        if file_type.is_dir() {
            let dir_key = local_path_key(&entry.path().display().to_string());
            if skip_keys.contains(&dir_key) || has_pruned_ancestor(&dir_key, &skip_keys) {
                walker.skip_current_dir();
                continue;
            }

            num_dirs += 1;
        // Symlinks count as files: Perforce tracks them as `symlink` revisions, and a directory
        // symlink is one of those revisions too. Leaving them out made every tracked symlink
        // look deleted from the workspace, because phase one only sees what was collected here.
        // `entry.metadata()` is lstat-like, so size and date describe the link itself.
        } else if file_type.is_file() || file_type.is_symlink() {
            let path_string = normalize_local_path_owned(entry.path().display().to_string());
            let meta = entry.metadata()?;
            total_size += meta.len();

            files.push(WorkspaceFile {
                path_lower: local_path_key(&path_string),
                path: path_string,
                size: meta.len(),
                date: meta.modified()?,
                filtered: false,
            });
        }
    }

    files.sort_by(|a, b| a.path_lower.cmp(&b.path_lower));

    Ok((files, num_dirs, total_size))
}

/// 用 `p4 ignores -i` 标记被忽略的文件，返回被忽略的数量。
/// 返回的路径必须与请求完全一致，异常输出只提示、不参与过滤。
pub(crate) async fn apply_file_ignores(
    options: &Options,
    work_dir: &str,
    files: &mut [WorkspaceFile],
) -> Result<usize> {
    if files.is_empty() {
        return Ok(0);
    }

    // p4 在命令行上认不出的名字整个不进查询：一条这样的路径就足以让**整批**失败，
    // 同批里 ASCII 文件的忽略判断会被一起带走（见 [`command_line_safe`]）。
    let ignores_paths = command_line_ready_paths(
        &files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>(),
    );
    if ignores_paths.is_empty() {
        return Ok(0);
    }

    let requested: HashSet<String> = ignores_paths.iter().cloned().collect();

    // 文件级过滤沿用旧行为：p4 报错只告警，不改变已有结果。
    let ignored_files = run_p4_command_batched(
        options,
        work_dir,
        &IGNORES_ARGS,
        &ignores_paths,
        false,
        FailureMode::Warn,
    )
    .await?;

    if options.verbose {
        for ignored_file in &ignored_files {
            println!("         Ignored file \"{}\" by ignores", ignored_file);
        }
    }

    let (ignored_files_hash, unrecognized) = parse_ignores_output(&ignored_files, &requested);
    if unrecognized > 0 {
        eprintln!(
            "Warning: {} line(s) of p4 ignores output did not match a requested file.",
            unrecognized
        );
    }

    let mut ignored_count = 0;
    for file in files.iter_mut() {
        if ignored_files_hash.contains(&file.path) {
            ignored_count += 1;
            file.filtered = true;
        }
    }

    Ok(ignored_count)
}

/// `p4 where` 的查询参数。它只读 client spec 的 view，不需要连服务器。
const WHERE_ARGS: [&str; 3] = ["-Mj", "-Ztag", "where"];

/// 从 `p4 where -Mj -Ztag` 的输出里挑出被 client view 排除的路径键。
///
/// view 里的排除行（例如 `-//aki/....tmp`）让 p4 完全看不见那些路径，`p4 where` 为它们
/// 返回的映射记录里会多一个 `unmap` 字段——这是区分「已映射」与「被排除」的信号。
fn unmapped_path_keys(lines: &[String]) -> HashSet<String> {
    let mut keys = HashSet::new();

    for line in lines {
        // p4 偶尔会混进非 JSON 的提示行，不值得让它中止整轮。
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        if record.get("unmap").is_none() {
            continue;
        }

        if let Some(path) = record["path"].as_str() {
            keys.insert(local_path_key(path));
        }
    }

    keys
}

/// 剔除键落在 `unmapped` 里的文件。大小写与分隔符都按本地路径键归一，与扫描结果一致。
fn drop_unmapped(files: &[String], unmapped: &HashSet<String>) -> Vec<String> {
    files
        .iter()
        .filter(|file| !unmapped.contains(&local_path_key(file)))
        .cloned()
        .collect()
}

/// 剔除被 client view 排除的路径。
///
/// 这些路径 p4 完全看不见：`p4 reconcile`、`p4 clean`、`p4 add` 对它们一律报
/// "not in client view"。工具是自己扫盘的，不查 view 就会把它们当成新增文件——
/// open 模式下会去 `p4 add` 一个 p4 会拒绝的文件，clean 模式下更糟：会**删掉**
/// p4 根本不管的文件。
///
/// 只喂已经判定为新增的文件：这类路径通常很少，一次批量查询就够；对全部工作区文件
/// 做这件事，在几十万文件的目录上会变成灾难。
pub(crate) async fn filter_unmapped_paths(
    options: &Options,
    work_dir: &str,
    files: &[String],
) -> Result<Vec<String>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }

    let lines = run_p4_command_batched(
        options,
        work_dir,
        &WHERE_ARGS,
        files,
        false,
        FailureMode::Warn,
    )
    .await?;

    Ok(drop_unmapped(files, &unmapped_path_keys(&lines)))
}

/// Scans workspace for files that are not ignored.
/// 返回工作区状态和本次实际应用的目录剪枝结果。
pub(crate) async fn gather_workspace(
    options: &Options,
    work_dir: &str,
) -> Result<(WorkspaceState, PrunePlan)> {
    println!("   Scanning workspace for files.");
    let start_time = Instant::now();

    let plan = plan_directory_pruning(options, work_dir).await?;

    let (mut files, num_dirs, total_size) = collect_workspace_files(work_dir, &plan.dirs)?;

    if options.verbose {
        for file in &files {
            println!("         File \"{}\", size {}", file.path, file.size);
        }
    }

    println!(
        "      Collected {} files in {} directories ({}) in {} seconds.",
        files.len(),
        num_dirs,
        format_size(total_size, BINARY),
        start_time.elapsed().as_secs_f32()
    );

    println!("   Filtering workspace files.");
    let start_time = Instant::now();

    let ignored_count = apply_file_ignores(options, work_dir, &mut files).await?;

    // 建立路径索引
    let mut workspace_state = WorkspaceState {
        num_files: files.len() - ignored_count,
        files,
        ..Default::default()
    };
    workspace_state.build_mapping();

    println!(
        "      Filtered out {} files, {} remain, in {} seconds.",
        ignored_count,
        workspace_state.num_files,
        start_time.elapsed().as_secs_f32()
    );

    Ok((workspace_state, plan))
}

/// 已剪目录里可能有 depot 已跟踪的文件，例如新加的忽略规则盖住了已同步文件。
/// 这些目录必须用原来的扫描逻辑重新收集并按原生规则过滤，否则会被误判为删除。
pub(crate) async fn rescan_tracked_pruned_dirs(
    options: &Options,
    work_dir: &str,
    depot: &DepotState,
    plan: &PrunePlan,
    workspace: &mut WorkspaceState,
) -> Result<()> {
    if plan.dirs.is_empty() {
        return Ok(());
    }

    let mut needed: Vec<&String> = Vec::new();

    for dir in &plan.dirs {
        let dir_key = local_path_key(dir);
        let tracked = depot
            .file_records
            .iter()
            .any(|record| path_is_under_key(&record.client_file_lower, &dir_key));

        if tracked {
            needed.push(dir);
        }
    }

    if needed.is_empty() {
        return Ok(());
    }

    println!(
        "   Re-scanning {} pruned directories that contain depot-tracked files.",
        needed.len()
    );

    let mut recollected = 0;
    let mut ignored_total = 0;

    for dir in needed {
        // 重新收集时不再剪枝，保证链接、文件类型与 I/O 错误语义和完整扫描一致。
        let (mut files, _num_dirs, _total_size) = collect_workspace_files(dir, &[])?;
        let ignored = apply_file_ignores(options, work_dir, &mut files).await?;

        recollected += files.len();
        ignored_total += ignored;
        workspace.num_files += files.len() - ignored;
        workspace.files.extend(files);
    }

    workspace
        .files
        .sort_by(|a, b| a.path_lower.cmp(&b.path_lower));
    workspace.build_mapping();

    println!(
        "      Re-collected {} files ({} ignored, {} remain).",
        recollected,
        ignored_total,
        recollected - ignored_total
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;

    use async_global_executor as task;
    use clap::Parser;

    use crate::model::{DepotFileRecord, FileAction};
    use crate::path::local_path_key;

    /// 被 view 排除的路径在 `p4 where` 的输出里多一个 `unmap` 字段，这是唯一的信号。
    #[test]
    fn where_output_marks_unmapped_paths() {
        let lines = vec![
            // 正常映射：没有 unmap 字段。
            r#"{"clientFile":"//ws/a.txt","depotFile":"//depot/a.txt","path":"C:\\ws\\a.txt"}"#
                .to_owned(),
            // 被 view 的排除行挡下：多一个 unmap 字段。
            r#"{"clientFile":"//ws/b.tmp","depotFile":"//depot/b.tmp","path":"C:\\ws\\b.tmp","unmap":""}"#
                .to_owned(),
            // p4 偶尔混进来的提示行，不该中止解析。
            "Some warning from p4".to_owned(),
        ];

        let keys = unmapped_path_keys(&lines);

        assert_eq!(keys.len(), 1);
        assert!(keys.contains(&local_path_key(r"C:\ws\b.tmp")));
    }

    #[test]
    fn unmapped_files_are_dropped_even_with_a_different_case() {
        // 扫描结果的路径大小写与 p4 返回的未必一致，过滤要按归一化后的键来。
        let files = vec![
            r"C:\ws\a.txt".to_owned(),
            r"C:\ws\B.TMP".to_owned(),
            r"C:\ws\c.txt".to_owned(),
        ];
        let unmapped: HashSet<String> = [local_path_key(r"C:\ws\b.tmp")].into_iter().collect();

        assert_eq!(
            drop_unmapped(&files, &unmapped),
            vec![r"C:\ws\a.txt".to_owned(), r"C:\ws\c.txt".to_owned()]
        );
    }

    #[test]
    fn rescan_restores_tracked_files_in_pruned_directories() {
        let tree = TempTree::new("rescan");
        // 被剪掉的目录里既有 depot 已跟踪的文件，也有没被跟踪的文件
        tree.file(".p4config", "P4IGNORE=.p4ignore\n");
        tree.file(".p4ignore", "node_modules/\nbuild/\n");
        tree.file("node_modules/mod.js", "tracked");
        tree.file("node_modules/open.js", "tracked, open for edit");
        tree.file("node_modules/extra.tmp", "not tracked");
        tree.file("build/out.bin", "no tracked file in this pruned directory");
        tree.file("node_modules2/keep.js", "not ignored");
        tree.file("src/app.txt", "not ignored");

        let work_dir = tree.root.to_string_lossy().to_string();
        let options = Options::parse_from(["p4delta", "-w", "p4delta-test"]);

        // 已剪目录：node_modules（有 tracked 文件）与 build（没有）
        let build_dir = tree.root.join("build").display().to_string();
        let plan = PrunePlan {
            dirs: vec![
                tree.root.join("node_modules").display().to_string(),
                build_dir,
            ],
            ..Default::default()
        };

        // 主扫描按剪枝结果跳过了这些目录，本地文件都在，但工作区里看不到
        let (files, _num_dirs, _total_size) =
            collect_workspace_files(&work_dir, &plan.dirs).unwrap();
        let mut workspace = WorkspaceState {
            num_files: files.len(),
            files,
            ..Default::default()
        };
        workspace.build_mapping();

        let path_of = |relative: &str| {
            let path = tree.root.join(relative).display().to_string();
            (path.clone(), local_path_key(&path))
        };
        let (mod_path, mod_key) = path_of("node_modules/mod.js");
        let (open_path, open_key) = path_of("node_modules/open.js");
        let (_extra_path, extra_key) = path_of("node_modules/extra.tmp");
        let (_build_path, build_key) = path_of("build/out.bin");
        let (_keep_path, keep_key) = path_of("node_modules2/keep.js");
        let (_gone_path, gone_key) = path_of("node_modules/gone.js");

        assert!(
            !workspace.has_file(&mod_key),
            "pruned files are not scanned"
        );
        assert!(workspace.has_file(&keep_key));

        // depot 里：mod.js 已同步、open.js 已同步且处于 open for edit、gone.js 已同步但本地没有
        let mut depot = DepotState::default();
        depot.file_records.push(depot_record(&mod_path));
        depot.file_records.push(DepotFileRecord {
            action: Some(FileAction::Edit),
            ..depot_record(&open_path)
        });
        depot.file_records.push(depot_record(&_gone_path));

        // 本机 p4 能否看到 fixture 的忽略规则，决定重扫后是否带 filtered 标记。
        // 进程内的环境改不了，这里只保证不吞掉真实错误；看不到规则时明确说明。
        let mut probe = vec![WorkspaceFile {
            path: mod_path.clone(),
            path_lower: mod_key.clone(),
            ..Default::default()
        }];
        let native_filter_available = if p4_available() {
            let ignored = task::block_on(apply_file_ignores(&options, &work_dir, &mut probe))
                .expect("file-level ignore filtering must not fail when p4 is available");
            if ignored > 0 {
                true
            } else {
                eprintln!(
                    "note: this environment does not resolve the fixture ignore rules, the filtered flag is not asserted"
                );
                false
            }
        } else {
            eprintln!("note: p4 is not available, the filtered flag is not asserted");
            false
        };

        task::block_on(rescan_tracked_pruned_dirs(
            &options,
            &work_dir,
            &depot,
            &plan,
            &mut workspace,
        ))
        .unwrap();

        // 存在的 tracked 文件必须回到工作区，否则差异分析会把它当成被删除
        assert!(workspace.has_file(&mod_key));
        assert!(
            workspace.has_file(&open_key),
            "open for edit 的记录只看路径"
        );
        // 本地确实没有的 tracked 文件仍然是缺失的
        assert!(!workspace.has_file(&gone_key));
        // 没有 tracked 文件的已剪目录不重扫，剪枝收益保持
        assert!(!workspace.has_file(&build_key));
        // 组件边界：node_modules2 不在剪枝范围内，一直是普通扫描结果
        assert!(workspace.has_file(&keep_key));
        // 重扫目录里的普通文件按原逻辑收集
        assert!(workspace.has_file(&extra_key));

        if native_filter_available {
            // 被原生规则忽略时标记 filtered，但必须仍留在工作区里（存在即不删）
            assert!(workspace.get_file(&mod_key).unwrap().filtered);
        } else {
            eprintln!("note: p4 ignore rules are not visible here, only presence is asserted");
        }

        // 重建后计数与 mapping 一致，且没有重复键
        assert_eq!(workspace.file_map.len(), workspace.files.len());
        assert_eq!(
            workspace.num_files,
            workspace.files.iter().filter(|file| !file.filtered).count()
        );
    }

    /// p4 把符号链接作为 `symlink` 类型的版本跟踪，所以扫描必须把它们当成文件。
    /// 漏掉它们时，每个已同步的符号链接都会被差异分析判成"本地已删除"。
    #[test]
    fn symlinks_are_collected_as_workspace_files() {
        let tree = TempTree::new("symlink-scan");
        let target = tree.file("real.so", "content");
        let link = tree.root.join("link.so");
        if let Err(error) = symlink_file(&target, &link) {
            eprintln!("skipping: cannot create symlinks here ({error})");
            return;
        }

        let (files, _num_dirs, _total_size) =
            collect_workspace_files(&tree.root.to_string_lossy(), &[]).unwrap();

        let link_key = local_path_key(&link.display().to_string());
        assert!(
            files.iter().any(|file| file.path_lower == link_key),
            "a symlink must be part of the workspace, or phase one reports it as deleted"
        );
    }
}
