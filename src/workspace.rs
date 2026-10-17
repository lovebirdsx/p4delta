//! 工作区文件的收集与过滤。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::Instant;

use anyhow::{Result, bail};
use async_global_executor as task;
use humansize::{BINARY, format_size};

use crate::cli::Options;
use crate::json::{P4Command, ignore_query_count, say_phase, sayln};
use crate::model::{DepotState, WorkspaceFile, WorkspaceState};
use crate::p4::process::{FailureMode, run_p4_command_batched, split_command_line_paths};
use crate::path::{
    escape_file_spec, local_path_key, normalize_local_path_owned, path_is_under_key,
};
use crate::prune::{IGNORES_ARGS, PrunePlan, parse_ignores_output, plan_directory_pruning};
use crate::reconcile::changes::NewFile;
use crate::scan;
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
                let (mut collected, dirs, size) = scan::collect(&entry.path, &skip_keys, excludes)?;
                // 空表直接接管，避免 append 再分配一张完整文件表。
                if files.is_empty() {
                    files = collected;
                } else {
                    files.append(&mut collected);
                }
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
    let (mut files, num_dirs, total_size) = scan::collect(work_dir, &skip_keys, excludes)?;

    files.sort_by(|a, b| a.path_lower.cmp(&b.path_lower));

    Ok((files, num_dirs, total_size))
}

/// 串行的目录收集器（`WalkDir`）。
///
/// 两个身份：**非 Windows 平台的产品路径**（`scan` 那条并行实现只在 Windows 上启用了，见
/// `scan::collect`），以及 Windows 上并行实现的语义对拍基准——结果次序、目录计数、字节数、
/// 错误选择都要能跟它逐项比（见 `scan.rs` 的用例）。所以 Windows 上也只在测试构建里编它。
#[cfg(any(not(windows), test))]
pub(crate) fn collect_directory(
    dir: &str,
    skip_keys: &HashSet<String>,
    excludes: &ExcludeSet,
) -> Result<(Vec<WorkspaceFile>, usize, u64)> {
    use crate::prune::has_pruned_ancestor;
    use walkdir::WalkDir;

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

/// 忽略查询的口径。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IgnoreQueryMode {
    /// 全盘过滤（open / clean）用的老口径：p4 报错只告警，已有结论照用——一次查询失败
    /// 不该让整轮停下来。
    Lenient,

    /// 删除保护用：p4 报错、p4 起不来、输出认不出来，一律失败；交不到命令行上的非 ASCII
    /// 候选还要**每个都拿到一条明确裁决**，拿不到就整轮停下（见 [`query_ignores_verbose`]）。
    ///
    /// 这个方向的结论是「这份本地文件删不删」，而失败会退化成空结果，也就是「一个都没被
    /// 忽略」——那正好是把用户的文件删掉的那一边。所以这里宁可整轮不做。
    Strict,
}

/// 用 `p4 ignores -i` 标记被忽略的文件，返回被忽略的数量。
pub(crate) async fn apply_file_ignores(
    options: &Options,
    work_dir: &str,
    files: &mut [WorkspaceFile],
) -> Result<usize> {
    let ignored = query_ignored_paths(
        options,
        work_dir,
        files.iter().map(|file| &file.path),
        IgnoreQueryMode::Lenient,
    )
    .await?;

    let mut ignored_count = 0;
    for file in files.iter_mut() {
        if ignored.contains(&file.path) {
            ignored_count += 1;
            file.filtered = true;
        }
    }

    Ok(ignored_count)
}

/// 对**指定的**一批路径查一次忽略规则，返回其中被正面判定的那些（原样返回请求里的字符串）。
///
/// 这就是原先对**整个工作区**做的那次查询，区别只在于「问哪一批」由调用方决定，它的成本
/// 因此从「工作区有多大」变成「这个调用方关心多少文件」（两笔账的实测数字见
/// `docs/why-faster.md`）。
///
/// 判据两条：`p4 ignores -i` 打头（路径只能挂在命令行上），非 ASCII 的名字（Windows 上
/// 交不出去，见 [`command_line_safe`]）另想办法——怎么问由 `mode` 决定。宽松口径补一次
/// `p4 add -n`，只认「被拒绝添加」这个肯定结论（认不出的输出不算命中，宁可少过滤也不误
/// 过滤）；严格口径改成逐字校验的 `p4 ignores -i -v`，两个方向都要明说。
///
/// [`command_line_safe`]: crate::p4::process::command_line_safe
pub(crate) async fn query_ignored_paths<'a>(
    options: &Options,
    work_dir: &str,
    paths: impl IntoIterator<Item = &'a String>,
    mode: IgnoreQueryMode,
) -> Result<HashSet<String>> {
    // 直接借出每个路径给 split，不必先克隆出一个中间 `Vec`：每个路径只被克隆一次，
    // 就在 split 交出的两半里。
    let (ignores_paths, unreadable_paths) = split_command_line_paths(paths);

    // 交不出去的那些不是被放弃，下面有补充判据接手。只在 `-v` 下说一声：它们现在有解，
    // 不该在正常输出里冒充告警——那会让人以为出了事。
    //
    // 措辞是**静态**判定，不是「送不进 p4 命令行」：严格口径下一步问的正是命令行上的
    // `p4 ignores -i -v`，能不能送进去要等运行时核对回显才知道（见 [`command_line_safe`]）。
    if options.verbose && !unreadable_paths.is_empty() {
        sayln!(
            "         {} path(s) are not statically safe on the p4 command line; their ignored \
             state comes from a {} query instead.",
            unreadable_paths.len(),
            match mode {
                IgnoreQueryMode::Lenient => "\"p4 add -n\"",
                IgnoreQueryMode::Strict => "verbatim-checked \"p4 ignores -i -v\"",
            }
        );
    }

    let mut ignored = query_ignores(options, work_dir, &ignores_paths, mode).await?;

    // 补充判据两条路子给出的都是**请求字符串**，与主查询同一口径。
    ignored.extend(match mode {
        IgnoreQueryMode::Lenient => {
            let refused = query_ignored_refusals(options, work_dir, &unreadable_paths).await?;
            // 按 [`local_path_key`] 归并（p4 回显的盘符大小写未必与请求一致），这里翻回
            // 请求里那一个字符串：调用方都在自己的路径上比较，不该关心键的口径。
            unreadable_paths
                .iter()
                .filter(|path| refused.contains(&local_path_key(path)))
                .cloned()
                .collect::<HashSet<String>>()
        }
        // 严格口径要的是两个方向都明说的裁决，`add -n` 给不出（见 [`query_ignores_verbose`]）。
        IgnoreQueryMode::Strict => {
            query_ignores_verbose(options, work_dir, &unreadable_paths).await?
        }
    });

    Ok(ignored)
}

/// 主查询：`p4 ignores -i`，路径挂在命令行上。返回命中的**请求字符串**（`parse_ignores_output`
/// 只认路径逐字出现在请求里的那些行，所以这就是请求里的一份）。
async fn query_ignores(
    options: &Options,
    work_dir: &str,
    ignores_paths: &[String],
    mode: IgnoreQueryMode,
) -> Result<HashSet<String>> {
    if ignores_paths.is_empty() {
        return Ok(HashSet::new());
    }

    // 只借用 `ignores_paths` 里的字符串，不克隆：这个集合只用来判断输出行是不是本批
    // 请求过的路径。
    let requested: HashSet<&str> = ignores_paths.iter().map(String::as_str).collect();

    // 全盘过滤沿用旧行为：p4 报错只告警，不改变已有结果——那条路的结论是「这些文件该不该
    // 被 p4 看见」，少过滤只会多干活。删除保护拿的是同一个空结果，含义却是「一个都没被
    // 忽略」：既要退出码非零失败，也要「退出码 0 却往 stderr 写了东西」失败——后者是 p4
    // 把规则文件读不出来之类的问题说出口的形态，而它的 stdout 会是空的（见 [`IgnoreQueryMode`]）。
    let failure = match mode {
        IgnoreQueryMode::Lenient => FailureMode::Warn,
        IgnoreQueryMode::Strict => FailureMode::ExitCodeOrStderr,
    };
    let ignored_lines = run_p4_command_batched(
        options,
        work_dir,
        P4Command::Ignores,
        &IGNORES_ARGS,
        ignores_paths,
        false,
        failure,
    )
    .await?;

    if options.verbose {
        for ignored_file in &ignored_lines {
            sayln!("         Ignored file \"{}\" by ignores", ignored_file);
        }
    }

    let (ignored, unrecognized) = parse_ignores_output(&ignored_lines, &requested);
    if unrecognized > 0 {
        if mode == IgnoreQueryMode::Strict {
            bail!(
                "Refusing to continue: {unrecognized} line(s) of the \"p4 ignores\" output could \
                 not be recognized, so which files the ignore rules cover is unknown."
            );
        }
        eprintln!(
            "Warning: {} line(s) of p4 ignores output did not match a requested file.",
            unrecognized
        );
    }

    Ok(ignored)
}

/// `p4 ignores -i -v` 的命令参数。比文件级过滤用的 [`IGNORES_ARGS`] 多一个 `-v`：只有它会
/// 为**每条**路径回一行明确裁决，而不是只列出被忽略的那些。
pub(crate) const IGNORES_VERBOSE_ARGS: [&str; 3] = ["ignores", "-i", "-v"];

/// 严格口径的补充判据：`p4 ignores -i -v`，路径挂在命令行上，**逐条校验裁决**。
///
/// 为什么不接着用 [`query_ignores`] 的 `-i`：那条口径只认「被忽略」的肯定结论，没出现在
/// 输出里就当没被忽略——对删除保护来说，这半个结论正好落在最危险的方向。`-v` 会为每条路径
/// 回一行明确的 `not ignored` 或 `ignored by <规则来源>`，把「沉默」变成「明说」。
///
/// 为什么路径挂命令行：`p4 ignores` 不认 `-x -`（见 `takes_paths_from_stdin`），而命令行在
/// Windows 上要过一遍系统 ANSI 代码页，非 ASCII 的名字可能被换掉。所以这里不假设它一定送得
/// 进去，而是**逐字核对回显**：p4 把请求的路径原样回显，一个字不同就当这条通道不可用，整轮
/// 停下（见 [`parse_ignore_verdicts`]）。能不能用是运行时的取舍——本机 p4 送得进去就正常
/// 跑完，送不进去就 fail-closed，不由平台一刀切。
async fn query_ignores_verbose(
    options: &Options,
    work_dir: &str,
    paths: &[String],
) -> Result<HashSet<String>> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }

    // 退出码非零，**或**退出码为 0 但 stderr 有话说，都算失败：后者是 p4 把规则文件读不出来
    // 之类的问题说出口的形态，而它的 stdout 会是空的——空 stdout 被读成「一个都没被忽略」
    // 正是删除保护最不能接受的（见 [`IgnoreQueryMode`]）。
    let lines = run_p4_command_batched(
        options,
        work_dir,
        P4Command::Ignores,
        &IGNORES_VERBOSE_ARGS,
        paths,
        false,
        FailureMode::ExitCodeOrStderr,
    )
    .await?;

    if options.verbose {
        for line in &lines {
            sayln!("         Verdict from \"p4 ignores -i -v\": {line}");
        }
    }

    parse_ignore_verdicts(&lines, paths).map_err(|error| error.context("Refusing to continue"))
}

/// `p4 ignores -i -v` 的两种行尾。p4 把请求的路径**原样**回显在行首：
///
/// ```text
/// E:\ws\src\使用说明.txt ignored by E:\ws\.p4ignore:1:*.tmp
/// E:\ws\src\lib.txt not ignored
/// ```
const IGNORED_BY_SEPARATOR: &str = " ignored by ";
const NOT_IGNORED_SUFFIX: &str = " not ignored";

/// 一条裁决行里读得出的候选裁决：路径前缀 + 跟在它后面的那个裁决。
///
/// 前缀只有两种来路——行尾的 ` not ignored`，以及每一处 ` ignored by ` 之前的那一段——所以是
/// **从这一行**枚举出几段前缀，再拿它们各查一次请求集合（见 [`parse_ignore_verdicts`]），
/// 而不是拿每条请求来这行上试前缀：后者在几万条候选上是平方级的。
///
fn verdict_candidates(line: &str) -> impl Iterator<Item = (&str, bool)> {
    let not_ignored = line
        .strip_suffix(NOT_IGNORED_SUFFIX)
        .map(|path| (path, false));
    // 按空格起点检查，允许路径尾部与裁决分隔符共用空格，避免 match_indices 跳过重叠匹配。
    let ignored = line.match_indices(' ').filter_map(|(at, _)| {
        let source = line[at..].strip_prefix(IGNORED_BY_SEPARATOR)?;
        (!source.trim().is_empty()).then_some((&line[..at], true))
    });

    not_ignored.into_iter().chain(ignored)
}

/// 解析 `p4 ignores -i -v` 的逐路径裁决，返回被忽略的那些**请求路径**。
///
/// 与 [`parse_ignores_output`] 的宽松口径相反：这里**要求每条请求路径都拿到恰好一条明确
/// 裁决**。漏行、多行、同一路径两条、回显与请求对不上（命令行表示不了那个名字时就是这样）
/// 一律失败——这条结论决定删不删用户的文件，任何一种「看不懂」都不能降级成「没被忽略」
/// （见 [`IgnoreQueryMode`]）。
///
/// 路径本身可能含 ` ignored by ` / ` not ignored` 这样的文本，所以**不能按分隔符截断**：
/// 裁决是按请求路径**逐字前缀**认的（[`verdict_candidates`]），而且一条行匹配上的请求路径
/// 必须恰好一条——两条（一条是另一条的前缀时就会这样）说明这份输出分不清说的是谁，宁可整批
/// 拒绝。
fn parse_ignore_verdicts(lines: &[String], requested: &[String]) -> Result<HashSet<String>> {
    // 请求里重复的路径只算一条：p4 会为每条请求各回一行，重复请求得到两条一模一样的裁决，
    // 而下面的校验把「同一路径两条裁决」当成压缩或截断的迹象。这个集合同时是每行那几段前缀
    // 的查询表——校验的成本因此是「输出量」而不是「请求数 × 输出量」。
    let mut requested_index = HashSet::new();
    let requested: Vec<&str> = requested
        .iter()
        .map(String::as_str)
        .filter(|path| requested_index.insert(*path))
        .collect();

    let mut ignored = HashSet::new();
    let mut decided: HashSet<&str> = HashSet::new();
    let mut unrecognized = Vec::new();
    let mut duplicated = Vec::new();

    for line in lines {
        let mut hits = verdict_candidates(line).filter(|(path, _)| requested_index.contains(*path));

        // 0 条：这行不代表任何请求路径的裁决。≥2 条：这份输出分不清说的是谁。两种都不能当
        // 裁决用。
        let (Some((path, is_ignored)), None) = (hits.next(), hits.next()) else {
            unrecognized.push(line.as_str());
            continue;
        };

        if !decided.insert(path) {
            duplicated.push(path);
        } else if is_ignored {
            ignored.insert(path.to_owned());
        }
    }

    if !unrecognized.is_empty() {
        bail!(
            "\"p4 ignores -i -v\" returned {} line(s) that do not name exactly one requested \
             path, so which files the ignore rules cover is unknown:\n{}",
            unrecognized.len(),
            sample(&unrecognized)
        );
    }

    if !duplicated.is_empty() {
        bail!(
            "\"p4 ignores -i -v\" returned more than one verdict for {} path(s):\n{}",
            duplicated.len(),
            sample(&duplicated)
        );
    }

    let missing: Vec<&str> = requested
        .into_iter()
        .filter(|path| !decided.contains(path))
        .collect();
    if !missing.is_empty() {
        bail!(
            "\"p4 ignores -i -v\" did not return a verdict for {} path(s), so whether the \
             ignore rules cover them is unknown:\n{}",
            missing.len(),
            sample(&missing)
        );
    }

    Ok(ignored)
}

/// 失败消息里点名的路径或输出行：够定位就行，几万条候选不该把一行错误刷满。
fn sample(items: &[&str]) -> String {
    const MAX: usize = 10;

    let mut text = items
        .iter()
        .take(MAX)
        .map(|item| format!("  {item}"))
        .collect::<Vec<String>>()
        .join("\n");

    if items.len() > MAX {
        text.push_str(&format!("\n  ... and {} more", items.len() - MAX));
    }

    text
}

/// 补充判据的命令参数。`p4 add` 认 stdin，`p4 ignores` 不认——这正是这条判据成立的原因。
const ADD_PREVIEW_ARGS: [&str; 2] = ["add", "-n"];

/// p4 拒绝添加被忽略文件时给的行尾，前面是它回显的本地路径。
const IGNORED_REFUSAL_SUFFIX: &str = " - ignored file can't be added.";

/// 宽松口径的补充判据：给交不到命令行上的路径补一次 `p4 add -n`，返回被拒绝添加的那些的
/// [`local_path_key`] 集合。
///
/// 这些路径只能走 stdin（`p4 ignores` 不认 `-x`，`p4 add` 认），而 stdin 是字节无损的
/// 通道，恰好绕开让它们上不了命令行的代码页转换。问的是「p4 会不会让我 add 它」——
/// 这正是这些路径最终会撞上的那个操作：被忽略又没入库的文件会被报成 Add，
/// `-a` 时 `p4 add` 拒绝它并让整轮失败。
///
/// 只采纳肯定结论。已入库的路径 p4 回的是 `//depot/...#N - opened for add` 或
/// `can't add existing file`，那里面**没有**忽略信息，所以「没被拒绝」不能反推成
/// 「未被忽略」——宽松口径下这批维持原状（少过滤只会多干活）。两个方向都要明说裁决的那条
/// 路子是 [`query_ignores_verbose`]。
async fn query_ignored_refusals(
    options: &Options,
    work_dir: &str,
    unreadable_paths: &[String],
) -> Result<HashSet<String>> {
    if unreadable_paths.is_empty() {
        return Ok(HashSet::new());
    }

    let requested: HashSet<String> = unreadable_paths
        .iter()
        .map(|path| local_path_key(path))
        .collect();

    // 被忽略的文件必然让 p4 退出 1，那正是这里的信号，不能当失败上报。
    let lines = run_p4_command_batched(
        options,
        work_dir,
        P4Command::Add,
        &ADD_PREVIEW_ARGS,
        unreadable_paths,
        false,
        FailureMode::Silent,
    )
    .await?;

    let (refused, unrecognized) = parse_ignored_refusals(&lines, &requested);
    if unrecognized > 0 {
        eprintln!(
            "Warning: {unrecognized} line(s) of p4 add -n output did not match a requested file."
        );
    }

    if options.verbose {
        for path in unreadable_paths {
            if refused.contains(&local_path_key(path)) {
                sayln!("         Ignored file \"{path}\" by add -n");
            }
        }
    }

    Ok(refused)
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

/// `p4 where -Mj -Ztag` 的一条记录：一个本地路径在 view 里的映射。
#[derive(Debug, PartialEq)]
struct WhereRecord {
    /// 归一化后的本地路径键，与扫描结果、fstat 的 clientFile 同一口径。
    key: String,

    /// depot 路径。新增文件的 `depotFile` 就来自这里——它 depot 里还没有记录可查。
    depot_file: String,

    /// 被 view 的排除行（例如 `-//depot/....tmp`）挡下：p4 完全看不见这个路径。
    /// 这是区分「已映射」与「被排除」的唯一信号。
    unmapped: bool,
}

/// 解析 `p4 where` 的输出行。p4 偶尔会混进非 JSON 的提示行，跳过即可，不值得中止整轮。
fn parse_where_lines(lines: &[String]) -> Vec<WhereRecord> {
    let mut records = Vec::new();

    for line in lines {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        // `path` 与 `depotFile` 缺一不可：没有它们这条记录对我们的两个用途都没意义。
        let (Some(path), Some(depot_file)) =
            (record["path"].as_str(), record["depotFile"].as_str())
        else {
            continue;
        };

        records.push(WhereRecord {
            key: local_path_key(path),
            depot_file: depot_file.to_owned(),
            unmapped: record.get("unmap").is_some(),
        });
    }

    records
}

/// 用 `p4 where` 的结果给新增文件补齐：剔掉被排除的路径，其余配上 depot 路径。
///
/// 比较按本地路径键（大小写与分隔符已归一），与扫描结果一致——p4 回的路径形式与本地
/// 扫描的未必逐字相同。
fn apply_where(files: &[NewFile], records: &[WhereRecord]) -> Vec<NewFile> {
    let mut unmapped = HashSet::new();
    let mut mapped = HashMap::new();

    for record in records {
        if record.unmapped {
            unmapped.insert(record.key.clone());
        } else {
            mapped.insert(record.key.clone(), record.depot_file.clone());
        }
    }

    files
        .iter()
        .filter(|file| !unmapped.contains(&local_path_key(&file.client_file)))
        .map(|file| NewFile {
            depot_file: mapped.get(&local_path_key(&file.client_file)).cloned(),
            client_file: file.client_file.clone(),
        })
        .collect()
}

/// 给新增文件补上 depot 路径，并剔掉被 client view 排除的路径。
///
/// 两件事共用同一次 `p4 where`：排除名单与 depot 映射是同一份输出的两个投影。
///
/// 排除项必须剔掉——被 view 排除的路径 p4 完全看不见：`p4 reconcile`、`p4 clean`、
/// `p4 add` 对它们一律报 "not in client view"。工具是自己扫盘的，不查 view 就会把它们
/// 当成新增文件——open 模式下会去 `p4 add` 一个 p4 会拒绝的文件，clean 模式下更糟：
/// 会**删掉** p4 根本不管的文件。
///
/// depot 路径则必须补上：契约里记录的 `depotFile` 是必填项，而新增文件是全流程里唯一
/// 没有 depot 记录可查的一类。补不上（p4 没回这条路径）时留 `None`，由报告层响亮失败——
/// 编一个路径出来只会把错的东西写进记录里。
///
/// 只喂已经判定为新增的文件：这类路径通常很少，一次批量查询就够；对全部工作区文件
/// 做这件事，在几十万文件的目录上会变成灾难。
pub(crate) async fn map_new_paths(
    options: &Options,
    work_dir: &str,
    files: &[NewFile],
) -> Result<Vec<NewFile>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }

    let paths: Vec<String> = files
        .iter()
        .map(|file| escape_file_spec(&file.client_file))
        .collect();

    let lines = run_p4_command_batched(
        options,
        work_dir,
        P4Command::Where,
        &WHERE_ARGS,
        &paths,
        false,
        FailureMode::Warn,
    )
    .await?;

    Ok(apply_where(files, &parse_where_lines(&lines)))
}

/// 一条本地路径在 client view 里的映射结果。
#[derive(Debug, PartialEq)]
pub(crate) struct MappedPath {
    /// 请求里的本地路径，与扫描结果同一拼写。
    pub(crate) client_file: String,

    /// view 映射出的 depot 路径；被排除或 p4 没回答时为 `None`。
    pub(crate) depot_file: Option<String>,

    /// view 的排除行（例如 `-//depot/....tmp`）挡下了这个路径。
    pub(crate) unmapped: bool,
}

/// 严格版 `p4 where`：命令失败即整轮失败，绝不把失败读成「没有映射」。
///
/// 删除未跟踪文件时，这个查询是「p4 到底管不管这个路径」的唯一证据。读成空映射的后果
/// 不是少做一点，而是把 client view 之外的文件也删掉，所以判据用 [`FailureMode::ExitCode`]：
/// 退出码非零必须中止，而 stderr 上的提示（`no such file(s)` 之类）不算失败。
pub(crate) async fn map_paths_strict(
    options: &Options,
    work_dir: &str,
    paths: &[String],
) -> Result<Vec<MappedPath>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }

    let escaped: Vec<String> = paths.iter().map(|path| escape_file_spec(path)).collect();
    let lines = run_p4_command_batched(
        options,
        work_dir,
        P4Command::Where,
        &WHERE_ARGS,
        &escaped,
        false,
        FailureMode::ExitCode,
    )
    .await?;

    let mut records: HashMap<String, WhereRecord> = HashMap::new();
    for record in parse_where_lines(&lines) {
        records.insert(record.key.clone(), record);
    }

    Ok(paths
        .iter()
        .map(|path| {
            let record = records.get(&local_path_key(path));
            MappedPath {
                client_file: path.clone(),
                depot_file: record
                    .filter(|record| !record.unmapped)
                    .map(|record| record.depot_file.clone()),
                unmapped: record.is_some_and(|record| record.unmapped),
            }
        })
        .collect())
}

/// 把同步扫盘交给阻塞线程池。
///
/// 遍历整段没有 await 点（`WalkDir` 与 `scan` 的收集器都是同步的），留在执行器线程上会让同一轮
/// `futures::join!` 里的 depot 查询一起饿死：那几路只在被 poll 时才读管道，p4 子进程
/// 填满管道后原地阻塞。冷缓存那一轮实测到的「三路并发退化成串行」就是这样——遍历独占
/// 执行器的 301s 里，`p4 fstat` 子进程的 CPU 时间纹丝不动、`p4 -G have` 连进程都没起，
/// 两条查询真正的成本（37s / 13s）全被推到遍历之后才暴露在墙钟上。
/// `prune::decide_directory_pruning` 的预扫描是同一类遍历，也走 `task::spawn_blocking`。
/// 取舍与实测见 `docs/why-faster.md`。
///
/// 闭包要 `'static`，所以调用方把名单按所有权复制过去（范围入口与排除项都很小）。
async fn off_executor<T, F>(walk: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    task::spawn_blocking(walk).await
}

/// 扫描工作区，返回收集到的文件状态与本次实际应用的剪枝计划。
pub(crate) async fn gather_workspace(
    options: &Options,
    scope: &Scope,
) -> Result<(WorkspaceState, PrunePlan)> {
    sayln!("   Scanning workspace for files.");
    let start_time = Instant::now();

    let roots = scope.directory_roots();
    let plan = plan_directory_pruning(options, &scope.work_dir, &roots, &scope.excludes).await?;

    let (files, num_dirs, total_size) = off_executor({
        let includes = scope.includes.clone();
        let skip_dirs = plan.dirs.clone();
        let excludes = scope.excludes.clone();
        move || collect_scope_files(&includes, &skip_dirs, &excludes)
    })
    .await?;

    if options.verbose {
        for file in &files {
            sayln!("         File \"{}\", size {}", file.path, file.size);
        }
    }

    sayln!(
        "      Collected {} files in {} directories ({}) in {} seconds.",
        files.len(),
        num_dirs,
        format_size(total_size, BINARY),
        start_time.elapsed().as_secs_f32()
    );
    say_phase(
        "scan",
        &format!(
            "{} file(s), {} directories, {}",
            files.len(),
            num_dirs,
            format_size(total_size, BINARY)
        ),
        Some(start_time.elapsed().as_secs_f32()),
    );

    let mut workspace_state = WorkspaceState {
        files,
        ..Default::default()
    };

    if options.force_sync() {
        // 强制修复不做全盘忽略过滤：这一步的成本跟着**工作区大小**走，而它的结论在这条路上
        // 用不上——`.p4ignore` 拦不住 `p4 sync -f`，被规则盖着的文件照旧按真实本地状态参与
        // 分析。真正还需要它的只剩删除组，那时只对删除候选发问
        // （见 `reconcile::sync::protect_delete_candidates`）。代价与实测数字见
        // `docs/why-faster.md`。
        sayln!("   Skipping ignore filtering in forced sync mode.");
        say_phase(
            "ignore",
            &format!(
                "skipped whole-workspace filtering; {} file(s) left unfiltered",
                workspace_state.files.len()
            ),
            None,
        );
        workspace_state.num_files = workspace_state.files.len();
    } else {
        sayln!("   Filtering workspace files.");
        let start_time = Instant::now();

        let queries_before = ignore_query_count();
        let ignored_count =
            apply_file_ignores(options, &scope.work_dir, &mut workspace_state.files).await?;
        let queries = ignore_query_count() - queries_before;
        workspace_state.num_files = workspace_state.files.len() - ignored_count;

        sayln!(
            "      Filtered out {} files, {} remain, in {} seconds.",
            ignored_count,
            workspace_state.num_files,
            start_time.elapsed().as_secs_f32()
        );
        say_phase(
            "ignore",
            &format!(
                "filtered {} of {} file(s), {queries} p4 query/queries",
                ignored_count,
                workspace_state.files.len()
            ),
            Some(start_time.elapsed().as_secs_f32()),
        );
    }

    workspace_state.build_mapping();

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

    sayln!(
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

    sayln!(
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

    use clap::Parser;

    use crate::model::{DepotFileRecord, FileAction};
    use crate::path::{local_path_key, path_identity_ignores_case};

    /// 被 view 排除的路径在 `p4 where` 的输出里多一个 `unmap` 字段，这是唯一的信号；
    /// 其余记录则要交出 depot 路径。两个投影来自同一份输出，这条用例把两边都钉住。
    #[test]
    fn where_output_carries_the_mapping_and_the_unmap_marker() {
        let lines = vec![
            // 正常映射：没有 unmap 字段。
            r#"{"clientFile":"//ws/a.txt","depotFile":"//depot/a.txt","path":"C:\\ws\\a.txt"}"#
                .to_owned(),
            // 被 view 的排除行挡下：多一个 unmap 字段（它同样带着 depotFile，别把它当映射）。
            r#"{"clientFile":"//ws/b.tmp","depotFile":"//depot/b.tmp","path":"C:\\ws\\b.tmp","unmap":""}"#
                .to_owned(),
            // p4 偶尔混进来的提示行，不该中止解析。
            "Some warning from p4".to_owned(),
        ];

        let records = parse_where_lines(&lines);

        assert_eq!(
            records,
            vec![
                WhereRecord {
                    key: local_path_key(r"C:\ws\a.txt"),
                    depot_file: "//depot/a.txt".to_owned(),
                    unmapped: false,
                },
                WhereRecord {
                    key: local_path_key(r"C:\ws\b.tmp"),
                    depot_file: "//depot/b.tmp".to_owned(),
                    unmapped: true,
                },
            ]
        );

        // 只喂一条记录里的路径：被排除的那条必须整个消失（映射有意义也不作数），
        // 没被 p4 提到的那条留着但没有 depot 路径（报告层会为它响亮失败）。
        let files = vec![
            NewFile::unmapped(r"C:\ws\a.txt".to_owned()),
            NewFile::unmapped(r"C:\ws\b.tmp".to_owned()),
            NewFile::unmapped(r"C:\ws\never-mentioned.txt".to_owned()),
        ];
        let mapped = apply_where(&files, &records);

        assert_eq!(
            mapped,
            vec![
                NewFile {
                    client_file: r"C:\ws\a.txt".to_owned(),
                    depot_file: Some("//depot/a.txt".to_owned()),
                },
                NewFile::unmapped(r"C:\ws\never-mentioned.txt".to_owned()),
            ]
        );
    }

    /// 名字里带 p4 filespec 元字符的文件：不转义的话一个这样的名字就能让整批 `p4 where`
    /// 当场失败，所有新增文件都拿不到映射。转义函数本身在 `path.rs`，与范围入口、动作调用
    /// 共用同一份（见 `escape_file_spec` 的文档）。
    #[test]
    fn filespec_metacharacters_are_escaped_for_the_mapping_query() {
        assert_eq!(
            escape_file_spec(r"C:\ws\report@2024.txt"),
            r"C:\ws\report%402024.txt"
        );
        // `%` 先换：否则后面生成的 `%40` 会被再编码一遍。
        assert_eq!(
            escape_file_spec("50%off#1*star.txt"),
            "50%25off%231%2Astar.txt"
        );
        // 没有元字符的路径一个字节都不该动。
        assert_eq!(escape_file_spec(r"C:\ws\readme.txt"), r"C:\ws\readme.txt");
    }

    #[test]
    fn unmapped_files_are_dropped_even_with_a_different_case() {
        // 扫描结果的路径大小写与 p4 返回的未必一致，过滤要按归一化后的键来。
        // 折大小写是折叠平台上的事，那里故意用另一种拼法；其余平台的键逐字比，
        // 两个来源就得用同一种拼法，否则测的就不是过滤而是平台差异了。
        let scanned_case_variant = if path_identity_ignores_case() {
            r"C:\ws\B.TMP"
        } else {
            r"C:\ws\b.tmp"
        };

        let files = vec![
            NewFile::unmapped(r"C:\ws\a.txt".to_owned()),
            NewFile::unmapped(scanned_case_variant.to_owned()),
            NewFile::unmapped(r"C:\ws\c.txt".to_owned()),
        ];
        let records = vec![
            WhereRecord {
                key: local_path_key(r"C:\ws\a.txt"),
                depot_file: "//depot/a.txt".to_owned(),
                unmapped: false,
            },
            WhereRecord {
                key: local_path_key(r"C:\ws\b.tmp"),
                depot_file: "//depot/b.tmp".to_owned(),
                unmapped: true,
            },
            WhereRecord {
                key: local_path_key(r"C:\ws\c.txt"),
                depot_file: "//depot/c.txt".to_owned(),
                unmapped: false,
            },
        ];

        let mapped: Vec<String> = apply_where(&files, &records)
            .into_iter()
            .map(|file| file.client_file)
            .collect();

        assert_eq!(
            mapped,
            vec![r"C:\ws\a.txt".to_owned(), r"C:\ws\c.txt".to_owned()]
        );
    }

    /// 裁决行里的路径是 p4 回显的本地路径，盘符大小写未必与请求一致（实测 `E:\` 回来是 `e:\`），
    /// 匹配走的是归一化后的路径键。折大小写是折叠平台上的事：那里才用两种拼法，
    /// 其余平台键逐字比，回显与请求就得是同一种拼法。
    #[test]
    fn ignored_refusals_match_across_case() {
        let (echoed, requested_form) = if path_identity_ignores_case() {
            (r"e:\ws\使用说明.txt", r"E:\ws\使用说明.txt")
        } else {
            (r"/ws/使用说明.txt", r"/ws/使用说明.txt")
        };

        let lines = vec![format!("{echoed}{IGNORED_REFUSAL_SUFFIX}")];
        let requested: HashSet<String> = [local_path_key(requested_form)].into_iter().collect();

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

    // ---- `p4 ignores -i -v` 的逐路径裁决 ----

    /// 两个方向都要认：`ignored by <规则来源>` 与 `not ignored`。
    ///
    /// 这两行的形状照本机实测的 `p4 ignores -i -v <path>` 输出写：行首是**请求路径的逐字
    /// 回显**，后面才是裁决。与宽松口径（[`parse_ignores_output`] 只认 ` ignored` 后缀）不同，
    /// 这里的负面结论也是明说的——删除保护要的就是它。
    #[test]
    fn verbose_verdicts_are_read_in_both_directions() {
        let covered = r"C:\ws\src\使用说明.txt".to_owned();
        let kept = r"C:\ws\src\lib.txt".to_owned();
        let lines = vec![
            format!(r"{covered} ignored by C:\ws\.p4ignore:1:*.tmp"),
            format!("{kept} not ignored"),
        ];

        let ignored = parse_ignore_verdicts(&lines, &[covered.clone(), kept.clone()]).unwrap();

        assert_eq!(ignored, HashSet::from([covered]));
    }

    /// 名字里的空格、方括号、emoji，以及 ` ignored by ` / ` not ignored` 这样的**分隔符文本**，
    /// 都不该让裁决错位：判据是「这一行以某条请求路径逐字开头」，不是按第一个分隔符截断。
    #[test]
    fn verdicts_survive_separator_text_inside_the_path() {
        let bracketed = r"C:\ws\空 格 [方括号] 目录\文件📁.tmp".to_owned();
        let contains_ignored_by = r"C:\ws\weird ignored by name.txt".to_owned();
        let contains_not_ignored = r"C:\ws\weird not ignored.txt".to_owned();
        let lines = vec![
            format!(r"{bracketed} ignored by C:\ws\.p4ignore:2:*.tmp"),
            format!("{contains_ignored_by} not ignored"),
            format!("{contains_not_ignored} not ignored"),
        ];

        let ignored = parse_ignore_verdicts(
            &lines,
            &[bracketed.clone(), contains_ignored_by, contains_not_ignored],
        )
        .unwrap();

        assert_eq!(ignored, HashSet::from([bracketed]));
    }

    #[test]
    fn verdicts_survive_overlapping_separator_text() {
        let path = r"C:\ws\中文 ignored by".to_owned();
        let lines = vec![format!(r"{path} ignored by C:\ws\.p4ignore:1:*")];

        let ignored = parse_ignore_verdicts(&lines, std::slice::from_ref(&path)).unwrap();

        assert_eq!(ignored, HashSet::from([path]));
    }

    /// 一条请求路径是另一条的前缀时，同一行会同时读成两条裁决。这份输出分不清说的是谁——
    /// 宁可整批拒绝，也不能挑一个信。
    #[test]
    fn a_line_naming_several_requested_paths_is_refused() {
        let short = r"C:\ws\escaped".to_owned();
        let long = r"C:\ws\escaped ignored by a.txt".to_owned();
        // 这一行既读得成 long 的 `not ignored`，也读得成 short 的 `ignored by a.txt not ignored`。
        let lines = vec![format!("{long} not ignored")];

        let error = parse_ignore_verdicts(&lines, &[short, long]).unwrap_err();

        assert!(
            error.to_string().contains("do not name exactly one"),
            "{error:#}"
        );
    }

    /// 漏行、空输出、额外行、重复与矛盾，一条都不能放过——它们各自的读法都会把「不知道」
    /// 变成「没被忽略」，而那条路上是要删用户的文件的。
    #[test]
    fn missing_extra_and_duplicate_verdicts_are_refused() {
        let first = r"C:\ws\a.txt".to_owned();
        let second = r"C:\ws\b.txt".to_owned();
        let requested = vec![first.clone(), second.clone()];

        // 漏行：b.txt 没有裁决。空输出是它的极端情形（p4 起不来、被别的东西截断）。
        let missing = vec![format!("{first} not ignored")];
        for lines in [missing, Vec::new()] {
            let error = parse_ignore_verdicts(&lines, &requested).unwrap_err();
            assert!(
                error.to_string().contains("did not return a verdict"),
                "{error:#}"
            );
        }

        // 额外行：这条路径不在请求里（回显被代码页换掉、或 p4 说了别的文件），不是裁决。
        let extra = vec![
            format!("{first} not ignored"),
            format!("{second} not ignored"),
            r"C:\ws\????.txt not ignored".to_owned(),
        ];
        let error = parse_ignore_verdicts(&extra, &requested).unwrap_err();
        assert!(
            error.to_string().contains("do not name exactly one"),
            "{error:#}"
        );

        // 重复与矛盾：同一条路径两条裁决，说明输出被压缩或截断了。
        let duplicated = vec![
            format!("{first} not ignored"),
            format!(r"{first} ignored by C:\ws\.p4ignore:1:*.txt"),
            format!("{second} not ignored"),
        ];
        let error = parse_ignore_verdicts(&duplicated, &requested).unwrap_err();
        assert!(
            error.to_string().contains("more than one verdict"),
            "{error:#}"
        );
    }

    /// 行尾形状不对的一律不算裁决：`ignored by` 后面空无一物（规则来源非空是它的下限），
    /// `not ignored` 后面多一个字符，都不是 p4 会给出的形状。
    #[test]
    fn malformed_verdict_lines_are_refused() {
        let path = r"C:\ws\a.txt".to_owned();
        let requested = vec![path.clone()];

        for line in [
            format!("{path} ignored by "),
            format!("{path} ignored by    "),
            format!("{path} not ignored."),
            format!("{path} ignored"),
        ] {
            let error = parse_ignore_verdicts(&[line], &requested).unwrap_err();
            assert!(
                error.to_string().contains("do not name exactly one"),
                "{error:#}"
            );
        }
    }

    /// 规模回归：解析量要跟**输出量**成正比，不能跟「请求数 × 输出行数」成正比。
    ///
    /// 删除组的候选就是这一轮问的全部路径，超大工作区里会有几万条。逐行去扫一遍全部请求
    /// （旧实现就是那样）在 5 万条候选上是 25 亿次前缀比较，而每行能读出的路径本来就只有
    /// 那几段。这条用例钉住线性口径：退回去的话它会慢到超时。
    #[test]
    fn parsing_verdicts_stays_linear_in_the_number_of_paths() {
        use std::time::{Duration, Instant};

        const PATHS: usize = 50_000;

        let requested: Vec<String> = (0..PATHS)
            .map(|index| format!(r"C:\ws\dir{index}\file{index}.txt"))
            .collect();
        let lines: Vec<String> = requested
            .iter()
            .enumerate()
            .map(|(index, path)| {
                if index % 2 == 0 {
                    format!("{path} not ignored")
                } else {
                    format!(r"{path} ignored by C:\ws\.p4ignore:{index}:*.tmp")
                }
            })
            .collect();

        let started = Instant::now();
        let ignored = parse_ignore_verdicts(&lines, &requested).unwrap();

        assert_eq!(ignored.len(), PATHS / 2, "两个方向都要逐条读出来");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "解析退成平方级了：5 万条候选用了 {:?}",
            started.elapsed()
        );
    }

    /// 从真实 `p4 ignores -i -v` 抓的一对行（p4 2024.1，本机实测）：规则来源是
    /// `<P4IGNORE 文件>:<行号>:<规则原文>`，回显的路径与请求逐字相同。
    #[test]
    fn accepts_real_p4_verbose_verdict_output() {
        let root = r"E:\ws";
        let covered = format!(r"{root}\中文目录\使用说明.tmp");
        let kept = format!(r"{root}\中文目录\plain.txt");
        let lines = vec![
            format!(r"{covered} ignored by {root}\.p4ignore:1:*.tmp"),
            format!("{kept} not ignored"),
        ];

        let ignored = parse_ignore_verdicts(&lines, &[covered.clone(), kept]).unwrap();

        assert_eq!(ignored, HashSet::from([covered]));
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

    /// 全量扫盘整段没有 await 点，必须交给阻塞线程：留在 `block_on` 那条线程上，同一轮
    /// `futures::join!` 里的 depot 查询会被它整段饿死（那几路只在被 poll 时才读管道），
    /// 「三路并发」退化成串行。冷缓存那一轮实测：遍历独占执行器 301s，期间 `p4 fstat`
    /// 子进程的 CPU 时间纹丝不动、`p4 -G have` 连进程都没起，两条查询真正的成本
    /// （37s / 13s）全被推到遍历之后暴露在墙钟上（见 `docs/why-faster.md`）。
    ///
    /// 判据是「第一次 poll 必然 Pending」：poll 就发生在 `spawn_blocking` 的同一次调用里，
    /// 两千个条目的遍历不可能在那几微秒内跑完；内联实现则会一路跑完并 Ready。
    #[test]
    fn the_scan_leaves_the_executor_thread_free() {
        let tree = TempTree::new("scan-off-executor");
        for index in 0..2000 {
            tree.file(&format!("bulk/{index}.txt"), "x");
        }

        // force 档：剪枝决策直接回退，这一步不碰 p4，用例不需要服务器。
        let options = Options::parse_from(["p4delta", "--sync", "--force", "-w", "ws"]);
        let root = tree.root.to_string_lossy().into_owned();
        let scope = Scope {
            includes: vec![ScopeEntry::new(root.clone(), EntryKind::Directory)],
            excludes: ExcludeSet::default(),
            work_dir: root,
        };

        let waker = futures::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);

        // 前提也钉住：剪枝决策在扫盘之前、且首轮就 Ready。它哪天变成会挂起的，下面的
        // Pending 就不再说明「扫盘离开了线程」——那一步先在这里红，不会被静默带过。
        let roots = scope.directory_roots();
        let mut pruning = std::pin::pin!(plan_directory_pruning(
            &options,
            &scope.work_dir,
            &roots,
            &scope.excludes
        ));
        assert!(
            pruning.as_mut().poll(&mut context).is_ready(),
            "前提变了：force 档的剪枝决策不该挂起，否则本条判据失去意义"
        );

        let mut scan = std::pin::pin!(gather_workspace(&options, &scope));
        assert!(
            scan.as_mut().poll(&mut context).is_pending(),
            "全量扫盘必须跑在阻塞线程上：内联跑会把同轮的 depot 查询饿死"
        );
    }
}
