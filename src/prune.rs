//! 被忽略目录的剪枝。
//!
//! 提前剪掉 p4 会忽略的目录，可以减少文件元数据查询和 `p4 ignores` 子进程数。
//! 忽略语义始终由 p4 判断，这里只做保守的探测与回退。

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use walkdir::WalkDir;

use crate::charset::query_p4_variable;
use crate::cli::Options;
use crate::p4::process::{
    FailureMode, compute_batches, run_p4_command_batched, run_p4_command_slice,
    split_command_line_paths,
};
use crate::path::{local_path_key, normalize_local_path_owned, path_is_under_key};

/// P4IGNORE 必须恰好解析成这个名字，才允许用目录级判断剪枝。
pub(crate) const P4IGNORE_FILE_NAME: &str = ".p4ignore";

/// 探测目录内容是否也被忽略用的合成文件名。只参与 `p4 ignores` 的规则匹配，不落盘。
pub(crate) const IGNORE_PROBE_NAME: &str = "p4delta-probe";

/// `p4 ignores -i` 的命令参数，文件级过滤与目录级剪枝共用。
pub(crate) const IGNORES_ARGS: [&str; 2] = ["ignores", "-i"];

/// 目录的任意祖先是否在集合里，用于跳过已剪枝目录的子树。
pub(crate) fn has_pruned_ancestor(dir_key: &str, pruned_keys: &HashSet<String>) -> bool {
    let mut end = dir_key.len();

    while let Some(index) = dir_key[..end].rfind(std::path::MAIN_SEPARATOR) {
        end = index;
        if pruned_keys.contains(&dir_key[..end]) {
            return true;
        }
    }

    false
}

/// 目录下用于确认目录内容也被忽略的合成子路径。
pub(crate) fn ignore_probe_path(dir: &str) -> String {
    Path::new(dir).join(IGNORE_PROBE_NAME).display().to_string()
}

/// 只保留最上层的目录：祖先已被剪掉的目录不需要单独记录。
pub(crate) fn top_most_dirs(dirs: &[String]) -> Vec<String> {
    let keys: Vec<String> = dirs.iter().map(|dir| local_path_key(dir)).collect();
    let mut result = Vec::new();

    for (index, dir) in dirs.iter().enumerate() {
        let covered = keys
            .iter()
            .enumerate()
            .any(|(other, key)| other != index && path_is_under_key(&keys[index], key));

        if !covered {
            result.push(dir.clone());
        }
    }

    result
}

/// 解析 `p4 ignores -i` 输出：只接受精确的 ` ignored` 后缀，且路径必须出现在请求集合里。
/// 返回 (命中的路径集合, 无法识别的行数)，无法识别的行绝不会被当成过滤结果。
pub(crate) fn parse_ignores_output(
    lines: &[String],
    requested: &HashSet<String>,
) -> (HashSet<String>, usize) {
    let mut ignored = HashSet::new();
    let mut unrecognized = 0;

    for line in lines {
        match line.strip_suffix(" ignored") {
            Some(path) if requested.contains(path) => {
                ignored.insert(path.to_owned());
            }
            // 后缀不符或不在请求里都算异常输出：宁可少过滤，也不能错过滤。
            _ => unrecognized += 1,
        }
    }

    (ignored, unrecognized)
}

/// `p4 ignores -v` 输出的保守解读结果：只判断输出能否完整识别、有没有 `!` 重新包含规则，
/// 不自己实现匹配，忽略语义始终由 p4 决定。
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct IgnoreRules {
    /// 是否出现过 `#FILE` 段头（p4 一定会输出内置默认规则段）。
    pub(crate) saw_file_header: bool,
    /// 是否存在 `!` 重新包含规则，存在时目录剪枝不成立。
    pub(crate) has_reinclude: bool,
    /// 无法识别的行数。
    pub(crate) unrecognized_lines: usize,
}

/// 保守解析 `p4 ignores -v`：只认识 `#FILE`/`#LINE` 段头与映射行，其余一律算未知输出。
pub(crate) fn summarize_ignore_rules(lines: &[String]) -> IgnoreRules {
    let mut rules = IgnoreRules::default();

    for line in lines {
        if let Some(header) = line.strip_prefix("#FILE ") {
            if header.trim().is_empty() {
                rules.unrecognized_lines += 1;
            } else {
                rules.saw_file_header = true;
            }
        } else if let Some(rest) = line.strip_prefix("#LINE ") {
            // 形如 "3:*.log"，冒号后是原始规则文本。
            match rest.split_once(':') {
                Some((number, text))
                    if !text.is_empty()
                        && !number.is_empty()
                        && number.bytes().all(|byte| byte.is_ascii_digit()) =>
                {
                    if text.starts_with('!') {
                        rules.has_reinclude = true;
                    }
                }
                _ => rules.unrecognized_lines += 1,
            }
        } else if line.starts_with('!') {
            // 映射行以 `!` 开头同样是重新包含。
            rules.has_reinclude = true;
        } else if line.starts_with('#') || !is_mapping_line(line) {
            // 未知指令或形状不对的映射行：不能假设自己理解了这个版本的输出。
            rules.unrecognized_lines += 1;
        }
    }

    rules
}

/// 映射行的形状检查：绝对路径或 `...` 开头。只判断形状，不做匹配。
pub(crate) fn is_mapping_line(line: &str) -> bool {
    if line.trim().is_empty() {
        return false;
    }

    if line.starts_with('/') || line.starts_with('\\') || line.starts_with("...") {
        return true;
    }

    // Windows 盘符，例如 C:\Workspace\...
    let bytes = line.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// 一次目录级忽略查询的结果。
#[derive(Debug, Default)]
pub(crate) struct DirQueryResult {
    /// 目录本身与其合成子路径都被忽略的目录；输出无法识别时为 None。
    prunable: Option<Vec<String>>,
    /// 批次数，仅用于日志。
    pub(crate) batches: usize,
}

/// 查询这些目录是否被忽略。同时查询一个合成的子路径，用来确认目录内容也被忽略，
/// 只有两者都命中才剪枝；输出无法识别时返回 None，调用方必须放弃剪枝。
pub(crate) async fn query_ignored_dirs(
    options: &Options,
    work_dir: &str,
    dirs: &[String],
) -> Result<DirQueryResult> {
    // 目录名里也可能有 p4 在命令行上认不出的字符（见 [`command_line_safe`]）。它们
    // 不进查询，于是也剪不掉——这是一条保守的回退：那些目录照常完整扫描，
    // 由文件级过滤接手。留在查询里的话，一个这样的名字会让**整批**失败。
    // 文件级过滤有 `p4 add -n` 的补充判据，目录级没有：这里只损失性能。
    let total = dirs.len();
    let (dirs, unreadable) = split_command_line_paths(dirs);

    // 只在 `-v` 下说一声。本函数每轮剪枝规划会被调两次（顶层与其余各一次），
    // 两批的分母不同，所以这里不去重：各自说各自的，反而看得清是哪一批。
    if options.verbose && !unreadable.is_empty() {
        println!(
            "         {} of {} directory name(s) cannot go on the p4 command line; \
             those directories are scanned in full instead of being pruned.",
            unreadable.len(),
            total
        );
    }

    if dirs.is_empty() {
        return Ok(DirQueryResult {
            prunable: Some(Vec::new()),
            batches: 0,
        });
    }

    // 目录里的内容是否被忽略无法直接查询，用一个不会落盘的合成子路径代替：p4 只做规则匹配。
    let mut arguments = Vec::with_capacity(dirs.len() * 2);
    for dir in &dirs {
        arguments.push(dir.clone());
        arguments.push(ignore_probe_path(dir));
    }

    let batches = compute_batches(&arguments).len();
    let requested: HashSet<String> = arguments.iter().cloned().collect();

    let lines = run_p4_command_batched(
        options,
        work_dir,
        &IGNORES_ARGS,
        &arguments,
        false,
        FailureMode::ExitCode,
    )
    .await?;
    let (ignored, unrecognized) = parse_ignores_output(&lines, &requested);

    if unrecognized > 0 {
        eprintln!(
            "Warning: {} line(s) of the p4 ignores output for directories were not recognized.",
            unrecognized
        );
        return Ok(DirQueryResult {
            prunable: None,
            batches,
        });
    }

    let prunable = dirs
        .iter()
        .filter(|dir| ignored.contains(*dir) && ignored.contains(&ignore_probe_path(dir)))
        .cloned()
        .collect();

    Ok(DirQueryResult {
        prunable: Some(prunable),
        batches,
    })
}

/// 目录剪枝计划。剪枝只减少扫描与文件级过滤的工作量，不改变 reconcile 语义。
#[derive(Debug, Default)]
pub(crate) struct PrunePlan {
    /// 最上层、互不重叠的已剪目录。
    pub(crate) dirs: Vec<String>,
    /// 预扫描到的候选目录数，仅用于日志。
    pub(crate) candidates: usize,
    /// 目录级查询用的批次数，仅用于日志。
    pub(crate) batches: usize,
    /// 未启用剪枝的原因；None 表示剪枝生效（可能一个目录都没剪掉）。
    pub(crate) fallback: Option<String>,
}

impl PrunePlan {
    /// 回退到完整扫描。
    fn fallback(reason: impl Into<String>) -> Self {
        PrunePlan {
            dirs: Vec::new(),
            candidates: 0,
            batches: 0,
            fallback: Some(reason.into()),
        }
    }
}

/// 轻量预扫描结果。
#[derive(Debug, Default)]
pub(crate) struct Prescan {
    /// 根目录以下的所有目录（相对根的深度, 归一化路径），深度优先顺序。
    pub(crate) dirs: Vec<(usize, String)>,
    /// 是否存在嵌套忽略文件（同名条目，Windows 上不区分大小写）；
    /// 存在时 p4 会按子目录应用规则，目录级判断不再可靠。
    pub(crate) nested_ignore: bool,
}

/// 是否是 P4IGNORE 指向的忽略文件名。
/// Windows 文件名大小写不敏感，`.P4IGNORE` 也是同一个文件。
pub(crate) fn is_ignore_file_name(name: &OsStr) -> bool {
    let name = name.as_encoded_bytes();

    if cfg!(windows) {
        name.eq_ignore_ascii_case(P4IGNORE_FILE_NAME.as_bytes())
    } else {
        name == P4IGNORE_FILE_NAME.as_bytes()
    }
}

/// 只收集目录并查找嵌套忽略文件，不获取文件 metadata、不保留文件路径。
pub(crate) fn prescan_directories(work_dir: &str) -> Result<Prescan> {
    let mut prescan = Prescan::default();

    for entry in WalkDir::new(work_dir) {
        let entry = entry?;
        let file_type = entry.file_type();

        if file_type.is_dir() {
            if entry.depth() > 0 {
                let path = normalize_local_path_owned(entry.path().display().to_string());
                prescan.dirs.push((entry.depth(), path));
            }
            continue;
        }

        // 同名条目只要不是目录就按嵌套忽略文件处理：符号链接等情况也保守算上。
        // 根目录自己的 .p4ignore 是标准配置（深度 1），只有子目录里的才算嵌套。
        if entry.depth() >= 2 && is_ignore_file_name(entry.file_name()) {
            prescan.nested_ignore = true;
        }
    }

    Ok(prescan)
}

/// 目录剪枝的对外入口：算好计划后统一打印一行结果，便于对比基准。
pub(crate) async fn plan_directory_pruning(options: &Options, work_dir: &str) -> Result<PrunePlan> {
    let start_time = Instant::now();
    // p4 查询出错只说明用不了目录级剪枝，回退到完整扫描继续，不要中止整个协调。
    let plan = match decide_directory_pruning(options, work_dir).await {
        Ok(plan) => plan,
        Err(error) => PrunePlan::fallback(format!("p4 query failed: {error:#}")),
    };

    match &plan.fallback {
        Some(reason) => println!("    Not pruning ignored directories: {}.", reason),
        None => println!(
            "      Pruned {} of {} candidate directories in {} batches ({} seconds).",
            plan.dirs.len(),
            plan.candidates,
            plan.batches,
            start_time.elapsed().as_secs_f32()
        ),
    }

    Ok(plan)
}

/// 判断剪枝路径能不能用。任何一条不满足都回退：目录级判断比完整扫描快，但必须保守使用。
pub(crate) async fn decide_directory_pruning(
    options: &Options,
    work_dir: &str,
) -> Result<PrunePlan> {
    if options.no_prune_ignored_dirs {
        return Ok(PrunePlan::fallback("--no-prune-ignored-dirs"));
    }

    // 只有恰好配置成标准 .p4ignore 时才敢用目录级判断，其他配置（多文件、绝对路径）回退。
    let p4ignore = query_p4_variable(Path::new(work_dir), "P4IGNORE");
    if p4ignore.as_deref() != Some(P4IGNORE_FILE_NAME) {
        return Ok(PrunePlan::fallback(format!(
            "P4IGNORE is {}",
            p4ignore.as_deref().unwrap_or("<unset>")
        )));
    }

    // 生效规则（含父目录规则）必须能完整理解，并且不能有 `!` 重新包含规则。
    let rules = summarize_ignore_rules(&run_p4_ignores_verbose(options, work_dir).await?);
    if !rules.saw_file_header || rules.unrecognized_lines > 0 {
        return Ok(PrunePlan::fallback("unrecognized p4 ignores output"));
    }
    if rules.has_reinclude {
        return Ok(PrunePlan::fallback(".p4ignore contains ! re-include rules"));
    }

    // 嵌套的 .p4ignore 只靠 `p4 ignores -v` 是看不出来的，必须自己预扫描。
    let prescan = prescan_directories(work_dir)?;
    if prescan.nested_ignore {
        return Ok(PrunePlan::fallback("nested .p4ignore found"));
    }

    let candidates = prescan.dirs.len();
    if candidates == 0 {
        return Ok(PrunePlan::default());
    }

    // 先问根的直接子目录：体积最大的忽略目录通常就在这一层。
    let mut top: Vec<String> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    for (depth, path) in prescan.dirs {
        if depth == 1 {
            top.push(path);
        } else {
            rest.push(path);
        }
    }

    let mut batches = 0;
    let mut pruned: Vec<String> = Vec::new();

    let top_result = query_ignored_dirs(options, work_dir, &top).await?;
    batches += top_result.batches;
    let Some(top_pruned) = top_result.prunable else {
        return Ok(PrunePlan::fallback("unrecognized directory ignore output"));
    };
    pruned.extend(top_pruned);

    // 其余目录一次问完：不要每层都启动 p4，但已剪子树不再查询。
    let pruned_keys: HashSet<String> = pruned.iter().map(|dir| local_path_key(dir)).collect();
    let remaining: Vec<String> = rest
        .into_iter()
        .filter(|dir| !has_pruned_ancestor(&local_path_key(dir), &pruned_keys))
        .collect();

    let rest_result = query_ignored_dirs(options, work_dir, &remaining).await?;
    batches += rest_result.batches;
    let Some(rest_pruned) = rest_result.prunable else {
        return Ok(PrunePlan::fallback("unrecognized directory ignore output"));
    };
    pruned.extend(rest_pruned);

    Ok(PrunePlan {
        dirs: top_most_dirs(&pruned),
        candidates,
        batches,
        fallback: None,
    })
}

/// 运行 `p4 ignores -v`，用于剪枝门控。
pub(crate) async fn run_p4_ignores_verbose(
    options: &Options,
    work_dir: &str,
) -> Result<Vec<String>> {
    // 目录级剪枝依赖 -v 的完整输出，p4 报错时必须失败，交给调用方回退到完整扫描。
    run_p4_command_slice(
        options,
        work_dir,
        &["ignores", "-v"],
        &[],
        false,
        FailureMode::ExitCode,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;

    use std::collections::HashSet;
    use std::env;
    use std::path::{Path, PathBuf};

    use async_global_executor as task;
    use clap::Parser;

    use crate::charset::parse_p4_set_value;
    use crate::p4::process::command_line_safe;
    #[test]
    fn pruned_ancestors_are_found_by_component() {
        let sep = std::path::MAIN_SEPARATOR;
        let pruned: HashSet<String> = [format!("c:{sep}ws{sep}build")].into_iter().collect();

        assert!(has_pruned_ancestor(
            &format!("c:{sep}ws{sep}build{sep}sub"),
            &pruned
        ));
        assert!(!has_pruned_ancestor(
            &format!("c:{sep}ws{sep}build2{sep}sub"),
            &pruned
        ));
        assert!(!has_pruned_ancestor(&format!("c:{sep}ws{sep}src"), &pruned));
    }

    #[test]
    fn top_most_dirs_drop_covered_children() {
        let sep = std::path::MAIN_SEPARATOR;
        let dirs = vec![
            format!("c:{sep}ws{sep}a"),
            format!("c:{sep}ws{sep}a{sep}b"),
            format!("c:{sep}ws{sep}c"),
        ];

        assert_eq!(top_most_dirs(&dirs), vec![dirs[0].clone(), dirs[2].clone()]);
    }

    #[test]
    fn probe_paths_are_synthetic_children() {
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            ignore_probe_path(&format!("c:{sep}ws{sep}build")),
            format!("c:{sep}ws{sep}build{sep}{IGNORE_PROBE_NAME}")
        );
    }

    // ---- p4 ignores 输出解析 ----

    #[test]
    fn parses_only_matching_ignore_output() {
        let requested: HashSet<String> = ["c:\\ws\\a.txt".to_string(), "c:\\ws\\b.txt".to_string()]
            .into_iter()
            .collect();

        let lines: Vec<String> = vec![
            "c:\\ws\\a.txt ignored".to_string(),
            // 缺少后缀
            "c:\\ws\\b.txt".to_string(),
            // 不在请求集合里
            "c:\\ws\\other.txt ignored".to_string(),
            // 旧实现按字节截断，这样的行会在切片时 panic
            "中abcdef".to_string(),
        ];

        let (ignored, unrecognized) = parse_ignores_output(&lines, &requested);

        assert_eq!(ignored.len(), 1);
        assert!(ignored.contains("c:\\ws\\a.txt"));
        assert_eq!(unrecognized, 3);
    }

    #[test]
    fn accepts_real_p4_ignores_verbose_output() {
        // 从真实 `p4 ignores -v` 输出抓取，含内置默认规则段。
        let lines: Vec<String> = [
            "#FILE - defaults",
            "#LINE 2:**/.p4root",
            ".../.p4root/...",
            ".../.p4root",
            "#LINE 1:**/.p4config",
            ".../.p4config",
            "#FILE C:/Temp/p4probe\\.p4ignore",
            "#LINE 2:node_modules/",
            "C:/Temp/p4probe/.../node_modules/...",
            "C:/Temp/p4probe/node_modules/...",
            "#LINE 1:*.log",
            "C:/Temp/p4probe/....log/...",
            "C:/Temp/p4probe/....log",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();

        let rules = summarize_ignore_rules(&lines);

        assert!(rules.saw_file_header);
        assert!(!rules.has_reinclude);
        assert_eq!(rules.unrecognized_lines, 0);
    }

    #[test]
    fn rejects_reinclude_unknown_and_empty_rule_output() {
        let reinclude: Vec<String> = [
            "#FILE C:/ws\\.p4ignore",
            "#LINE 2:!keep/x",
            "!C:/ws/.../keep/x/...",
            "#LINE 1:keep/",
            "C:/ws/.../keep/...",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert!(summarize_ignore_rules(&reinclude).has_reinclude);

        // 未知指令和形状不对的行都不能假装理解
        let unknown = vec![
            "#FILE C:/ws\\.p4ignore".to_owned(),
            "#UNKNOWN something".to_owned(),
            "not-a-mapping".to_owned(),
        ];
        let rules = summarize_ignore_rules(&unknown);
        assert_eq!(rules.unrecognized_lines, 2);

        // 查询失败时不会有默认规则段，必须回退
        let empty = summarize_ignore_rules(&[]);
        assert!(!empty.saw_file_header);
    }

    #[test]
    fn prescan_collects_directories_and_finds_nested_ignore_files() {
        let tree = TempTree::new("prescan");
        tree.file(".p4ignore", "ignored/\n");
        tree.file("ignored/a.txt", "a");
        tree.file("src/deep/b.txt", "b");

        let prescan = prescan_directories(&tree.root.to_string_lossy()).unwrap();

        assert!(!prescan.nested_ignore);
        // 根的直接子目录深度为 1，更深的目录交给第二轮目录查询
        let mut depths: Vec<usize> = prescan.dirs.iter().map(|(depth, _)| *depth).collect();
        depths.sort_unstable();
        assert_eq!(depths, vec![1, 1, 2]);
        assert!(prescan.dirs.iter().any(|(_, path)| local_path_key(path)
            == local_path_key(&tree.root.join("src").display().to_string())));

        // 根目录以下的 .p4ignore 会让 p4 按子目录应用规则，必须放弃剪枝
        tree.file("src/deep/.p4ignore", "x\n");
        assert!(
            prescan_directories(&tree.root.to_string_lossy())
                .unwrap()
                .nested_ignore
        );
    }

    #[test]
    fn pruning_plan_queries_only_candidate_directories() {
        let tree = TempTree::new("prune-plan");
        tree.file(".p4config", "P4IGNORE=.p4ignore\n");
        tree.file(".p4ignore", "node_modules/\n*.log\n");
        tree.dir("node_modules/deep/nested");
        tree.dir("src");
        tree.file("node_modules/deep/mod.js", "x");
        tree.file("src/app.txt", "x");

        if !p4_available() {
            eprintln!("skipping: p4 is not available");
            return;
        }

        // 断言放在子进程里做，环境由父测试给定，不依赖这台机器的全局 P4 配置。
        run_child_case("gate-plan", &tree.root);
    }

    /// 子用例入口：只有父测试重新执行的子进程才带 CHILD_CASE_VAR，普通运行时直接返回。
    #[test]
    fn p4_environment_child_case() {
        let Ok(case) = env::var(CHILD_CASE_VAR) else {
            return;
        };
        let work_dir = env::var(CHILD_DIR_VAR).expect("child case directory");
        let root = PathBuf::from(&work_dir);

        // 子进程里 P4CONFIG 指向 fixture 自己的配置，配置没生效说明测试环境不成立。
        assert!(
            fixture_config_is_effective(&root),
            "the child must resolve P4IGNORE from the fixture {TEST_P4CONFIG_NAME}"
        );

        match case.as_str() {
            // 命中忽略目录时只剪掉最上层目录，其余目录不再查询
            "gate-plan" => {
                let options = Options::parse_from(["p4delta", "-w", "p4delta-test"]);
                let plan = task::block_on(decide_directory_pruning(&options, &work_dir))
                    .expect("p4 queries must succeed when p4 is available");
                assert!(
                    plan.fallback.is_none(),
                    "directory pruning must not fall back: {:?}",
                    plan.fallback
                );
                assert_eq!(
                    plan.dirs,
                    vec![root.join("node_modules").display().to_string()]
                );
                assert_eq!(plan.candidates, 4);
            }
            // 嵌套忽略文件必须让门控回退
            "gate-nested" => {
                assert!(prescan_directories(&work_dir).unwrap().nested_ignore);
                let options = Options::parse_from(["p4delta", "-w", "p4delta-test"]);
                let plan = task::block_on(decide_directory_pruning(&options, &work_dir))
                    .expect("p4 queries must succeed when p4 is available");
                assert!(
                    plan.dirs.is_empty(),
                    "a nested ignore file must disable pruning"
                );
                let reason = plan.fallback.expect("the fallback reason must be reported");
                assert!(
                    reason.contains("nested"),
                    "unexpected fallback reason: {reason}"
                );
            }
            // p4 查询失败时只回退，绝不中止整个协调
            "gate-failure" => {
                // 用客户端名 `*` 制造一次真实的查询失败：Windows 上 p4 把命令行解析两遍
                // （宽字符一遍、ANSI 一遍）再比对参数个数，`*` 在两遍里展开成不同个数，
                // p4 报 `Argument parsing ambiguity.` 并以 -1 退出（见 `command_line_safe`）。
                //
                // Unix 上没有这道转换，`p4 ignores` 也压根不看客户端名——空串、引号、换行、
                // 超长名字实测都照样退出 0，而忽略文件读不出来时 p4 只当没有规则。这条失败
                // 在 Unix 上造不出来，所以这一段只在 Windows 上跑；Err → 回退那几行映射
                // 本身与平台无关（`plan_directory_pruning` 里那个 match）。
                if !cfg!(windows) {
                    eprintln!(
                        "skipping: the `p4 -c '*'` failure injection only reproduces on Windows"
                    );
                    return;
                }

                // 前提先确认真实成立：换成不再失败的 p4 时这里会明确报错，而不是让断言失效。
                let options = Options::parse_from(["p4delta", "-w", "*"]);
                assert!(
                    task::block_on(run_p4_ignores_verbose(&options, &work_dir)).is_err(),
                    "client name \"*\" must make p4 exit non-zero on this platform"
                );

                let plan = task::block_on(plan_directory_pruning(&options, &work_dir))
                    .expect("a failing p4 query must not abort the reconcile");
                assert!(
                    plan.dirs.is_empty(),
                    "no directory may be pruned from a failed query"
                );
                let reason = plan.fallback.expect("the fallback reason must be reported");
                assert!(
                    reason.contains("p4 query failed"),
                    "unexpected fallback reason: {reason}"
                );
            }
            other => panic!("unknown child case: {other}"),
        }
    }

    /// 用真实 p4 查询哪些路径被忽略；p4 不存在时返回 None 让调用方跳过。
    ///
    /// 路径先过 [`split_command_line_paths`]：`p4 ignores` 不认 `-x`，路径只能挂在命令行上，
    /// 生产会把送不进命令行的那些挑掉（见 `src/p4/process.rs` 里那段说明）。这里照做，
    /// 否则用例会去断言一个生产根本不会发出的请求。
    fn p4_ignored_paths(
        root: &Path,
        paths: &[PathBuf],
        env: &[(&str, &str)],
    ) -> Option<HashSet<String>> {
        let path_strings: Vec<String> = paths
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        let (ready, _unreadable) = split_command_line_paths(&path_strings);
        let mut args: Vec<&str> = vec!["ignores", "-i"];
        args.extend(ready.iter().map(String::as_str));

        let output = p4_output(root, &args, env)?;

        assert!(
            output.status.success(),
            "p4 ignores failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let text = String::from_utf8_lossy(&output.stdout);
        Some(
            text.lines()
                .filter_map(|line| line.strip_suffix(" ignored").map(local_path_key))
                .collect(),
        )
    }

    /// 用真实 p4 读取 `p4 ignores -v` 的输出行。
    fn p4_ignores_verbose(root: &Path, env: &[(&str, &str)]) -> Option<Vec<String>> {
        let output = p4_output(root, &["ignores", "-v"], env)?;

        assert!(
            output.status.success(),
            "p4 ignores -v failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let text = String::from_utf8_lossy(&output.stdout);
        Some(text.lines().map(str::to_owned).collect())
    }

    #[test]
    fn real_p4_agrees_on_directory_and_file_level_judgment() {
        let tree = TempTree::new("dir-ignores");
        tree.file(
            ".p4ignore",
            "node_modules/\n*.log\nsp ace dir/\n空格 目录/\n",
        );
        tree.dir("node_modules/sub");
        tree.dir("node_modules2");
        tree.dir("src");
        tree.dir("sp ace dir");
        tree.dir("空格 目录");

        let node_modules = tree.dir("node_modules");
        let node_modules2 = tree.dir("node_modules2");
        let src = tree.dir("src");
        let spaced = tree.dir("sp ace dir");
        let unreadable = tree.dir("空格 目录");

        let Some(ignored) = p4_ignored_paths(
            &tree.root,
            &[
                node_modules.clone(),
                ignore_probe_path(&node_modules.display().to_string()).into(),
                node_modules2.clone(),
                src.clone(),
                spaced.clone(),
                ignore_probe_path(&spaced.display().to_string()).into(),
                unreadable.clone(),
                ignore_probe_path(&unreadable.display().to_string()).into(),
            ],
            &TEST_P4_ENV,
        ) else {
            eprintln!("skipping: p4 is not available");
            return;
        };

        // 目录本身与合成的子路径必须同时命中，剪枝才成立
        assert!(ignored.contains(&local_path_key(&node_modules.display().to_string())));
        assert!(ignored.contains(&local_path_key(&ignore_probe_path(
            &node_modules.display().to_string()
        ))));

        // 组件边界：node_modules2 不能被 node_modules/ 覆盖
        assert!(!ignored.contains(&local_path_key(&node_modules2.display().to_string())));

        // 没被规则覆盖的目录不参与剪枝
        assert!(!ignored.contains(&local_path_key(&src.display().to_string())));

        // 名字里带空格的目录同样按目录级判断——规则里的空格是名字的一部分，不是分隔符
        assert!(ignored.contains(&local_path_key(&spaced.display().to_string())));
        assert!(ignored.contains(&local_path_key(&ignore_probe_path(
            &spaced.display().to_string()
        ))));

        // 非 ASCII 的目录名在 Windows 上送不进 p4 的命令行（生产的
        // `split_command_line_paths` 会把它挑掉，见 `src/p4/process.rs`），目录级剪枝对此
        // 没有回退，只能保守地按「未被忽略」处理；Unix 上没有这道转换，必须命中。
        let key = local_path_key(&unreadable.display().to_string());
        let probe = local_path_key(&ignore_probe_path(&unreadable.display().to_string()));
        let sendable = command_line_safe(&unreadable.display().to_string());
        assert_eq!(
            ignored.contains(&key),
            sendable,
            "{unreadable:?} 的判断与可送性不符"
        );
        assert_eq!(
            ignored.contains(&probe),
            sendable,
            "{unreadable:?} 的判断与可送性不符"
        );
    }

    #[test]
    fn real_p4_reports_reinclude_rules_and_nested_files() {
        let tree = TempTree::new("reinclude");
        tree.file(".p4ignore", "build/\n!build/keep/\n");
        tree.dir("build/keep");

        let Some(lines) = p4_ignores_verbose(&tree.root, &TEST_P4_ENV) else {
            eprintln!("skipping: p4 is not available");
            return;
        };

        // `!` 重新包含规则会被门控识别出来并回退
        let rules = summarize_ignore_rules(&lines);
        assert!(rules.has_reinclude);

        // 嵌套 .p4ignore 在 -v 里看不到，只能靠预扫描
        tree.file("build/keep/.p4ignore", "x\n");
        let prescan = prescan_directories(&tree.root.to_string_lossy()).unwrap();
        assert!(prescan.nested_ignore);
    }

    #[test]
    fn real_p4_ignores_files_under_a_directory_rule() {
        let tree = TempTree::new("file-ignores");
        tree.file(".p4ignore", "node_modules/\n*.log\n");
        tree.dir("node_modules");
        tree.file("node_modules/mod.js", "x");
        tree.file("src/app.log", "x");
        tree.file("src/app.txt", "x");

        let paths: Vec<PathBuf> = ["node_modules/mod.js", "src/app.log", "src/app.txt"]
            .iter()
            .map(|relative| tree.root.join(relative))
            .collect();

        let Some(ignored) = p4_ignored_paths(&tree.root, &paths, &TEST_P4_ENV) else {
            eprintln!("skipping: p4 is not available");
            return;
        };

        // 剪掉的目录里的文件本来就该被文件级过滤处理，两者的判断必须一致
        assert!(ignored.contains(&local_path_key(&paths[0].display().to_string())));
        assert!(ignored.contains(&local_path_key(&paths[1].display().to_string())));
        assert!(!ignored.contains(&local_path_key(&paths[2].display().to_string())));
    }

    #[test]
    fn p4_config_files_take_precedence_over_the_environment() {
        let tree = TempTree::new("config-precedence");
        let config = tree.file(".p4config", "P4IGNORE=.cfgignore\n");
        tree.file(".cfgignore", "node_modules/\n");
        tree.file(".envignore", "src/\n");
        tree.dir("node_modules");
        tree.dir("src");

        // 只配置子进程：P4CONFIG 指向 fixture 的配置文件，同时用环境变量放一个诱饵值。
        let config_value = config.display().to_string();
        let env = [
            ("P4CONFIG", config_value.as_str()),
            ("P4IGNORE", ".envignore"),
        ];

        let Some(output) = p4_output(&tree.root, &["set"], &env) else {
            eprintln!("skipping: p4 is not available");
            return;
        };
        assert!(
            output.status.success(),
            "p4 set failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // query_p4_variable 解析的就是这条输出：生效值必须来自配置文件，而不是环境变量
        assert_eq!(
            parse_p4_set_value(&String::from_utf8_lossy(&output.stdout), "P4IGNORE").as_deref(),
            Some(".cfgignore"),
            "p4 must resolve P4IGNORE from .p4config even when the environment sets it"
        );

        // 规则也必须来自 .cfgignore：node_modules 被忽略，src 不受 .envignore 影响
        let paths = [tree.root.join("node_modules"), tree.root.join("src")];
        let Some(ignored) = p4_ignored_paths(&tree.root, &paths, &env) else {
            eprintln!("skipping: p4 is not available");
            return;
        };
        assert!(ignored.contains(&local_path_key(&paths[0].display().to_string())));
        assert!(!ignored.contains(&local_path_key(&paths[1].display().to_string())));
    }

    #[test]
    fn prescan_counts_case_insensitive_and_symlinked_nested_ignore_files() {
        let tree = TempTree::new("prescan-nested");
        tree.file(".p4ignore", "ignored/\n");
        tree.file("src/app.txt", "x");
        let work_dir = tree.root.to_string_lossy().to_string();

        // 根目录自己的忽略文件是标准配置，只有子目录里的才算嵌套
        assert!(!prescan_directories(&work_dir).unwrap().nested_ignore);

        // Windows 的文件名不区分大小写，.P4IGNORE 就是同一个文件
        tree.file("src/.P4IGNORE", "!keep/\n");
        assert_eq!(
            prescan_directories(&work_dir).unwrap().nested_ignore,
            cfg!(windows),
            ".P4IGNORE is a nested ignore file only on case-insensitive filesystems"
        );

        // 符号链接的忽略文件同样要算嵌套：只要不是目录就保守当成忽略文件。
        // 用独立的树，避免被上面的 .P4IGNORE 掩盖（那样断言就永远成立）。
        let symlink_tree = TempTree::new("prescan-symlink");
        symlink_tree.file(".p4ignore", "ignored/\n");
        let symlink_dir = symlink_tree.root.to_string_lossy().to_string();
        assert!(!prescan_directories(&symlink_dir).unwrap().nested_ignore);

        let target = symlink_tree.file("shared-rules.txt", "x\n");
        let link = symlink_tree.root.join("linked").join(P4IGNORE_FILE_NAME);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        match symlink_file(&target, &link) {
            Ok(()) => assert!(
                prescan_directories(&symlink_dir).unwrap().nested_ignore,
                "a symlinked nested ignore file must disable pruning"
            ),
            // 没有创建符号链接的权限时只能明确跳过，其他错误一律失败
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping symlinked ignore file check: {error}")
            }
            Err(error) => panic!("creating a symlinked ignore file failed: {error}"),
        }
    }

    #[test]
    fn pruning_gate_falls_back_for_a_nested_ignore_file() {
        let tree = TempTree::new("prune-nested");
        tree.file(".p4config", "P4IGNORE=.p4ignore\n");
        tree.file(".p4ignore", "node_modules/\n");
        tree.file("node_modules/mod.js", "x");
        // 子目录里多出来的忽略文件会让 p4 换一套规则，门控必须回退
        let nested_name = if cfg!(windows) {
            ".P4IGNORE"
        } else {
            ".p4ignore"
        };
        tree.file(&format!("node_modules/{nested_name}"), "!keep/\n");

        if !p4_available() {
            eprintln!("skipping: p4 is not available");
            return;
        }

        run_child_case("gate-nested", &tree.root);
    }

    #[test]
    fn pruning_plan_falls_back_when_a_p4_query_fails() {
        let tree = TempTree::new("prune-p4-failure");
        tree.file(".p4config", "P4IGNORE=.p4ignore\n");
        tree.file(".p4ignore", "node_modules/\n");
        tree.file("node_modules/mod.js", "x");

        if !p4_available() {
            eprintln!("skipping: p4 is not available");
            return;
        }

        run_child_case("gate-failure", &tree.root);
    }
}
