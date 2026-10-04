//! 工作区文件的收集与过滤。

use std::collections::HashSet;
use std::fs;
use std::time::Instant;

use anyhow::Result;
use humansize::{BINARY, format_size};
use walkdir::WalkDir;

use crate::cli::Options;
use crate::model::{DepotState, WorkspaceFile, WorkspaceState};
use crate::p4::process::{FailureMode, run_p4_command_batched, split_command_line_paths};
use crate::path::{local_path_key, normalize_local_path_owned, path_is_under_key};
use crate::prune::{
    IGNORES_ARGS, PrunePlan, has_pruned_ancestor, parse_ignores_output, plan_directory_pruning,
};
use crate::scope::{EntryKind, ExcludeSet, Scope, ScopeEntry};

/// 扫描范围入口：目录入口递归收集；文件入口直接取单个文件（本地不存在就跳过，
/// 让 depot 记录去判定——「本地删除、depot 还有」正是 open for delete 要看的状态）。
///
/// `skip_dirs` 是 ignore 剪枝结果（语义：p4 看不见这些目录，但其中已跟踪文件要回扫）；
/// `excludes` 是范围硬排除（命中即整棵子树跳过，且不像剪枝那样回扫）。
pub(crate) fn collect_scope_files(
    includes: &[ScopeEntry],
    skip_dirs: &[String],
    excludes: &ExcludeSet,
) -> Result<(Vec<WorkspaceFile>, usize, u64)> {
    let skip_keys: HashSet<String> = skip_dirs.iter().map(|dir| local_path_key(dir)).collect();
    let mut files = Vec::new();
    let mut num_dirs = 0;
    let mut total_size = 0;

    for entry in includes {
        match entry.kind {
            EntryKind::Directory => {
                let (mut collected, dirs, size) =
                    collect_directory(&entry.path, &skip_keys, excludes)?;
                files.append(&mut collected);
                num_dirs += dirs;
                total_size += size;
            }
            EntryKind::File => {
                if let Some(file) = collect_single_file(&entry.path, excludes)? {
                    total_size += file.size;
                    files.push(file);
                }
            }
        }
    }

    files.sort_by(|a, b| a.path_lower.cmp(&b.path_lower));

    Ok((files, num_dirs, total_size))
}

/// 扫描单个目录。`skip_dirs` 里的目录及其子树会被跳过（目录级 ignore 剪枝的结果）；
/// `excludes` 是范围排除项，命中的子树同样不进入。
pub(crate) fn collect_workspace_files(
    work_dir: &str,
    skip_dirs: &[String],
    excludes: &ExcludeSet,
) -> Result<(Vec<WorkspaceFile>, usize, u64)> {
    let skip_keys: HashSet<String> = skip_dirs.iter().map(|dir| local_path_key(dir)).collect();
    let (mut files, num_dirs, total_size) = collect_directory(work_dir, &skip_keys, excludes)?;

    files.sort_by(|a, b| a.path_lower.cmp(&b.path_lower));

    Ok((files, num_dirs, total_size))
}

/// 递归收集一个目录里的文件。
fn collect_directory(
    dir: &str,
    skip_keys: &HashSet<String>,
    excludes: &ExcludeSet,
) -> Result<(Vec<WorkspaceFile>, usize, u64)> {
    let mut files = Vec::new();
    let mut num_dirs = 0;
    let mut total_size = 0;

    let mut walker = WalkDir::new(dir).into_iter();
    while let Some(entry) = walker.next() {
        let entry = entry?;
        let file_type = entry.file_type();

        if file_type.is_dir() {
            let dir_key = local_path_key(&entry.path().display().to_string());

            // 范围排除是硬排除：整棵子树不碰，也不像 ignore 剪枝那样回扫
            // （回扫是给「p4 看不见但已跟踪」的目录用的，排除目录不在其列）。
            if excludes.excludes_key(&dir_key) {
                walker.skip_current_dir();
                continue;
            }

            if skip_keys.contains(&dir_key) || has_pruned_ancestor(&dir_key, skip_keys) {
                walker.skip_current_dir();
                continue;
            }

            num_dirs += 1;
        // 符号链接算文件：Perforce 把它们作为 `symlink` 修订跟踪，指向目录的符号链接
        // 也是其中一种。漏掉它们会让每个被跟踪的符号链接看起来像从工作区删掉了——
        // 第一阶段只看得见这里收集到的东西。`entry.metadata()` 是 lstat 语义，
        // 大小与时间描述的是链接本身。
        } else if file_type.is_file() || file_type.is_symlink() {
            let path_string = normalize_local_path_owned(entry.path().display().to_string());

            if excludes.excludes_key(&local_path_key(&path_string)) {
                continue;
            }

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

    Ok((files, num_dirs, total_size))
}

/// 收集单个文件入口。本地不存在时返回 None——文件可能已被删除，depot 记录会把它判成
/// 待删除；这里静默跳过正是让那条链路成立的前提。
fn collect_single_file(path: &str, excludes: &ExcludeSet) -> Result<Option<WorkspaceFile>> {
    if excludes.excludes_key(&local_path_key(path)) {
        return Ok(None);
    }

    // lstat 语义，与目录扫描一致：符号链接算文件，大小与时间描述链接本身。
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Ok(None);
    };

    // 入口位置被本地目录顶替（文件被删、原地建了同名目录）时不按文件收。
    if meta.is_dir() {
        return Ok(None);
    }

    Ok(Some(WorkspaceFile {
        path_lower: local_path_key(path),
        path: normalize_local_path_owned(path.to_owned()),
        size: meta.len(),
        date: meta.modified()?,
        filtered: false,
    }))
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

    // p4 在命令行上认不出的名字整个不进主查询：一条这样的路径就足以让**整批**失败，
    // 同批里 ASCII 文件的忽略判断会被一起带走（见 [`command_line_safe`]）。
    // 它们改走下面的补充判据，不是被放弃。
    //
    // 直接借出每个 `WorkspaceFile` 的路径给 split，不必先克隆出一个中间 `Vec`：
    // 每个路径只被克隆一次，就在 split 交出的两半里。
    let (ignores_paths, unreadable_paths) =
        split_command_line_paths(files.iter().map(|file| &file.path));

    // 交不出去的那些不是被放弃，下面有补充判据接手。只在 `-v` 下说一声：它们现在有解，
    // 不该在正常输出里冒充告警——那会让人以为出了事。
    if options.verbose && !unreadable_paths.is_empty() {
        println!(
            "         {} of {} path(s) cannot go on the p4 command line; \
             their ignored state comes from a \"p4 add -n\" query instead.",
            unreadable_paths.len(),
            files.len()
        );
    }

    let queried = query_ignores(options, work_dir, files, &ignores_paths).await?;
    let refused = query_ignored_refusals(options, work_dir, files, &unreadable_paths).await?;

    Ok(queried + refused)
}

/// 主查询：`p4 ignores -i`，路径挂在命令行上。
async fn query_ignores(
    options: &Options,
    work_dir: &str,
    files: &mut [WorkspaceFile],
    ignores_paths: &[String],
) -> Result<usize> {
    if ignores_paths.is_empty() {
        return Ok(0);
    }

    // 只借用 `ignores_paths` 里的字符串，不克隆：这个集合只用来判断输出行是不是本批
    // 请求过的路径。
    let requested: HashSet<&str> = ignores_paths.iter().map(String::as_str).collect();

    // 文件级过滤沿用旧行为：p4 报错只告警，不改变已有结果。
    let ignored_files = run_p4_command_batched(
        options,
        work_dir,
        &IGNORES_ARGS,
        ignores_paths,
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

/// 补充判据的命令参数。`p4 add` 认 stdin，`p4 ignores` 不认——这正是这条判据成立的原因。
const ADD_PREVIEW_ARGS: [&str; 2] = ["add", "-n"];

/// p4 拒绝添加被忽略文件时给的行尾，前面是它回显的本地路径。
const IGNORED_REFUSAL_SUFFIX: &str = " - ignored file can't be added.";

/// 给交不到命令行上的路径补一次 `p4 add -n`，返回新标记的数量。
///
/// 这些路径只能走 stdin（`p4 ignores` 不认 `-x`，`p4 add` 认），而 stdin 是字节无损的
/// 通道，恰好绕开让它们上不了命令行的代码页转换。问的是「p4 会不会让我 add 它」——
/// 这正是这些路径最终会撞上的那个操作：被忽略又没入库的文件会被报成 Add，
/// `-a` 时 `p4 add` 拒绝它并让整轮失败。
///
/// 只采纳肯定结论。已入库的路径 p4 只回 `can't add existing file`，那里面没有忽略信息，
/// 所以「没被拒绝」不能反推成「未被忽略」——那些文件维持原状，与没有这条判据时一样。
async fn query_ignored_refusals(
    options: &Options,
    work_dir: &str,
    files: &mut [WorkspaceFile],
    unreadable_paths: &[String],
) -> Result<usize> {
    if unreadable_paths.is_empty() {
        return Ok(0);
    }

    let requested: HashSet<String> = unreadable_paths
        .iter()
        .map(|path| local_path_key(path))
        .collect();

    // 被忽略的文件必然让 p4 退出 1，那正是这里的信号，不能当失败上报。
    let lines = run_p4_command_batched(
        options,
        work_dir,
        &ADD_PREVIEW_ARGS,
        unreadable_paths,
        false,
        FailureMode::Silent,
    )
    .await?;

    let (refused, unrecognized) = parse_ignored_refusals(&lines, &requested);
    if unrecognized > 0 {
        eprintln!(
            "Warning: {} line(s) of p4 add -n output did not match a requested file.",
            unrecognized
        );
    }

    let mut ignored_count = 0;
    for file in files.iter_mut() {
        if !file.filtered && refused.contains(&local_path_key(&file.path)) {
            if options.verbose {
                println!("         Ignored file \"{}\" by add -n", file.path);
            }
            ignored_count += 1;
            file.filtered = true;
        }
    }

    Ok(ignored_count)
}

/// 解析 `p4 add -n` 输出里「被忽略」的裁决，返回命中路径的 [`local_path_key`] 集合。
///
/// 只认 `<本地路径> - ignored file can't be added.` 这一种行。同一批里还有 p4 给每条路径
/// 的 depot 侧附注（`//depot/path#1 - opened for add` 之类），那些不含裁决，不算异常输出。
///
/// 回显的盘符大小写与请求未必一致（实测输入 `E:\...` 回来是 `e:\...`），所以路径按键比对。
/// 判据是「这一行是不是本地路径」：裁决行必然不是 depot 路径，因此不以 `//` 开头；
/// 反过来，不认得的本地路径行说明输出格式变了，计进 `unrecognized` 提醒一声——
/// 漏认只会少过滤，方向是保守的。
fn parse_ignored_refusals(
    lines: &[String],
    requested: &HashSet<String>,
) -> (HashSet<String>, usize) {
    let mut refused = HashSet::new();
    let mut unrecognized = 0;

    for line in lines {
        if line.starts_with("//") {
            continue;
        }

        let Some(path) = line.strip_suffix(IGNORED_REFUSAL_SUFFIX) else {
            unrecognized += 1;
            continue;
        };

        let key = local_path_key(path);
        if requested.contains(&key) {
            refused.insert(key);
        } else {
            unrecognized += 1;
        }
    }

    (refused, unrecognized)
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

/// 扫描工作区，返回收集到的文件状态与本次实际应用的剪枝计划。
pub(crate) async fn gather_workspace(
    options: &Options,
    scope: &Scope,
) -> Result<(WorkspaceState, PrunePlan)> {
    println!("   Scanning workspace for files.");
    let start_time = Instant::now();

    let roots = scope.directory_roots();
    let plan = plan_directory_pruning(options, &scope.first_dir, &roots, &scope.excludes).await?;

    let (mut files, num_dirs, total_size) =
        collect_scope_files(&scope.includes, &plan.dirs, &scope.excludes)?;

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

    let ignored_count = apply_file_ignores(options, &scope.first_dir, &mut files).await?;

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
///
/// `excludes` 要一路带下去：排除子树在 depot 侧已经被过滤掉，本地若还把它们收回来，
/// 那些文件在分析眼里就成了「本地有、depot 没有」的新增。
pub(crate) async fn rescan_tracked_pruned_dirs(
    options: &Options,
    work_dir: &str,
    depot: &DepotState,
    plan: &PrunePlan,
    excludes: &ExcludeSet,
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
        let (mut files, _num_dirs, _total_size) = collect_workspace_files(dir, &[], excludes)?;
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

    /// 裁决行里的路径是 p4 回显的本地路径，盘符大小写未必与请求一致（实测 `E:\` 回来是 `e:\`）。
    #[test]
    fn ignored_refusals_match_across_case() {
        let lines = vec![r"e:\ws\使用说明.txt - ignored file can't be added.".to_owned()];
        let requested: HashSet<String> = [local_path_key(r"E:\ws\使用说明.txt")]
            .into_iter()
            .collect();

        let (refused, unrecognized) = parse_ignored_refusals(&lines, &requested);

        assert_eq!(refused, requested);
        assert_eq!(unrecognized, 0);
    }

    /// 每条请求路径都会换回一行 depot 侧附注，那些不含裁决，不能当成异常输出刷警告。
    #[test]
    fn add_preview_depot_notes_are_not_verdicts() {
        let lines = vec![
            r"//depot/ws/readme.txt#1 - opened for add".to_owned(),
            r"//depot/ws/lib.txt - can't add existing file".to_owned(),
        ];
        let requested: HashSet<String> =
            [local_path_key(r"C:\ws\readme.txt")].into_iter().collect();

        let (refused, unrecognized) = parse_ignored_refusals(&lines, &requested);

        assert!(refused.is_empty());
        assert_eq!(unrecognized, 0, "depot 侧附注是预期输出，不是格式漂移");
    }

    /// 不认得的本地路径行说明输出格式变了：只记数告警，绝不产生过滤结果。
    #[test]
    fn unrecognized_lines_never_filter_anything() {
        let lines = vec![
            // 后缀不全。
            r"C:\ws\readme.txt - ignored".to_owned(),
            // 后缀对，但路径不在请求集合里。
            r"C:\ws\other.txt - ignored file can't be added.".to_owned(),
        ];
        let requested: HashSet<String> =
            [local_path_key(r"C:\ws\readme.txt")].into_iter().collect();

        let (refused, unrecognized) = parse_ignored_refusals(&lines, &requested);

        assert!(refused.is_empty());
        assert_eq!(unrecognized, 2);
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
        tree.file("node_modules/generated/gen.js", "excluded by the scope");
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
            collect_workspace_files(&work_dir, &plan.dirs, &ExcludeSet::default()).unwrap();
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
        let (_gen_path, gen_key) = path_of("node_modules/generated/gen.js");

        // 范围排除的子树：depot 侧已经过滤掉它，本地侧同样不能再收回来。
        let excludes = ExcludeSet::from_dir_keys(&[&local_path_key(
            &tree
                .root
                .join("node_modules/generated")
                .display()
                .to_string(),
        )]);

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
            &excludes,
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
        // 排除子树不收：它的记录在 depot 侧已被过滤，收回来就成了凭空的「新增」
        assert!(
            !workspace.has_file(&gen_key),
            "an excluded subtree must not be recollected"
        );

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
            collect_workspace_files(&tree.root.to_string_lossy(), &[], &ExcludeSet::default())
                .unwrap();

        let link_key = local_path_key(&link.display().to_string());
        assert!(
            files.iter().any(|file| file.path_lower == link_key),
            "a symlink must be part of the workspace, or phase one reports it as deleted"
        );
    }
}
