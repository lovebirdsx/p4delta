//! 操作范围：把命令行位置参数与 `.p4delta-scope` 求值成本轮实际处理的入口集合。
//!
//! 范围是**硬上限**：配置提供 include/exclude，命令行参数与之取交集，exclude 永远优先。
//! 目录入口递归整个子树；文件入口只处理单个文件——后者允许本地不存在，因为
//! 「本地删除、depot 还有」正是 open for delete 要看的状态，`p4 fstat` 对这样的
//! 路径照样返回记录（实测：路径解析走 view 映射，不读本地文件系统）。

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::cli::Options;
use crate::json::sayln;
use crate::p4::process::{FailureMode, run_p4_command_slice};
use crate::path::{
    absolute_local_path, local_path_key, normalize_local_path, path_is_under_key,
    strip_depot_wildcard_suffix,
};

/// 持久范围配置的文件名。从入口目录向上查找，与 p4 找 `.p4config` 的行为一致。
const SCOPE_FILE_NAME: &str = ".p4delta-scope";

/// 入口是目录还是文件。决定 file spec 的形状（目录补 `/...`）与排除时的匹配方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    Directory,
    File,
}

/// 一个已定位的范围入口。
#[derive(Debug, Clone)]
pub(crate) struct ScopeEntry {
    /// 绝对路径，保留原始大小写（盘符已统一成大写）。
    pub(crate) path: String,

    /// 小写匹配键，与 p4 返回的 clientFile 同一口径。
    pub(crate) path_lower: String,

    pub(crate) kind: EntryKind,
}

impl ScopeEntry {
    /// 给 p4 的 file spec：目录要补 `...`（裸目录不是合法 file spec，会报
    /// `no such file(s)`），文件就是路径本身。
    pub(crate) fn file_spec(&self) -> String {
        match self.kind {
            EntryKind::Directory => format!("{}{}...", self.path, std::path::MAIN_SEPARATOR),
            EntryKind::File => self.path.clone(),
        }
    }
}

/// 排除集合。目录按子树匹配，文件按精确路径匹配。
///
/// 它同时喂给两个消费方（depot 记录过滤与扫盘跳过），两侧口径必须一致——
/// 只做一侧会让排除目录里的已跟踪文件被误判成「本地删除」。
#[derive(Debug, Default)]
pub(crate) struct ExcludeSet {
    dir_keys: Vec<String>,
    file_keys: HashSet<String>,

    /// 用户写下的排除项原样（带 `-` 前缀、原始大小写），只给报错信息用：匹配用的是上面
    /// 那些键，它们折过小写、去过相对形式，直接打印出来像是把路径写错了。
    labels: Vec<String>,

    /// 隐式排除：生效的那份 `.p4delta-scope` 自身。它必须走这条两侧共用的通路——
    /// 扫盘要跳过它（否则会被报成待新增），而一旦它被提交进 depot（团队共享范围配置的
    /// 常见做法），depot 记录也必须一起剔：只剔一侧，它就会因为「本地扫不到」被误判成
    /// 待删除，`-a` 下真的下发 `p4 delete`，把共享的配置删掉。
    implicit_file_key: Option<String>,
}

impl ExcludeSet {
    /// 构造一个只含目录排除项的集合。仅供测试：产品路径上只有 [`build_exclude_set`] 会造它。
    #[cfg(test)]
    pub(crate) fn from_dir_keys(keys: &[&str]) -> Self {
        ExcludeSet {
            dir_keys: keys.iter().map(|key| (*key).to_owned()).collect(),
            file_keys: HashSet::new(),
            labels: keys.iter().map(|key| format!("-{key}")).collect(),
            implicit_file_key: None,
        }
    }

    /// 把生效的配置文件登记为隐式排除。
    pub(crate) fn exclude_implicit_file(&mut self, key: String) {
        self.implicit_file_key = Some(key);
    }

    /// 路径键是否落在排除范围内（含被排除目录自身）。
    pub(crate) fn excludes_key(&self, key: &str) -> bool {
        self.file_keys.contains(key)
            || self.implicit_file_key.as_deref() == Some(key)
            || self
                .dir_keys
                .iter()
                .any(|dir| key == dir || path_is_under_key(key, dir))
    }

    /// 用户声明的排除项数量（不含隐式的那一项，它不该出现在面向用户的计数里）。
    pub(crate) fn declared_len(&self) -> usize {
        self.labels.len()
    }

    /// 面向报错信息的排除项清单，`-a, -b` 形状。
    pub(crate) fn describe(&self) -> String {
        self.labels.join(", ")
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.declared_len() == 0 && self.implicit_file_key.is_none()
    }
}

/// 本轮实际处理的全部范围。
#[derive(Debug)]
pub(crate) struct Scope {
    /// include 入口：已去重、无覆盖、无重复，按路径排序。
    pub(crate) includes: Vec<ScopeEntry>,

    /// 排除集合。
    pub(crate) excludes: ExcludeSet,

    /// 第一个入口的所在目录（入口是文件时取父目录）。作为 p4 子进程的 cwd，
    /// `.p4config` / P4IGNORE 的发现跟着它走，与单目录时代的工作方式一致。
    pub(crate) first_dir: String,
}

impl Scope {
    /// 全部入口的 file spec，交给 p4 做范围查询。
    pub(crate) fn file_specs(&self) -> Vec<String> {
        self.includes.iter().map(ScopeEntry::file_spec).collect()
    }

    /// 目录入口的路径列表：目录剪枝预扫描的起点。
    pub(crate) fn directory_roots(&self) -> Vec<String> {
        self.includes
            .iter()
            .filter(|entry| entry.kind == EntryKind::Directory)
            .map(|entry| entry.path.clone())
            .collect()
    }
}

/// 解析出的原始条目：方向与路径文本已拆好，还没做路径归一与 depot 翻译。
#[derive(Debug, Clone)]
struct RawEntry {
    /// `-` 前缀表示排除。
    exclude: bool,

    /// 路径文本（`...` 后缀已剥掉）。
    path: String,

    /// 原来带 `...` 后缀：本地不存在时也按目录处理。
    explicit_dir: bool,
}

/// 解析单个条目文本（配置文件的一行、命令行的一段）。
///
/// `-` 前缀是排除标记，所以以 `-` 开头的真实文件名要写成带相对前缀的形式
/// （如 `./-x`），与命令行工具的常规做法一致。
fn parse_entry(text: &str) -> RawEntry {
    let text = text.trim();
    let (exclude, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, text),
    };

    // depot 路径单独处理：它永远用正斜杠，不能过 `normalize_local_path`——Windows 上
    // 那会把 `//depot/main/src/...` 折成 UNC 形状的 `\\depot\main\src`，后面的 depot
    // 翻译就认不出它了，只剩一条通向 `\\depot\...` 的扫盘路径。
    if rest.starts_with("//") {
        let stripped = rest.trim_end_matches("/...");
        return RawEntry {
            exclude,
            explicit_dir: stripped.len() != rest.len(),
            path: stripped.to_owned(),
        };
    }

    // 先归一分隔符再剥后缀：用户可能写正斜杠，也可能写平台分隔符。
    let normalized = normalize_local_path(rest);
    let stripped = strip_depot_wildcard_suffix(&normalized);

    RawEntry {
        exclude,
        explicit_dir: stripped.len() != normalized.len(),
        path: stripped.to_owned(),
    }
}

/// 解析 `.p4delta-scope` 内容：每行一个条目，整行 `#` 注释，空行忽略。
fn parse_scope_content(content: &str) -> Result<Vec<RawEntry>> {
    let mut entries = Vec::new();

    for (index, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let entry = parse_entry(line);
        if entry.path.is_empty() {
            bail!(
                "Invalid entry on line {} of {SCOPE_FILE_NAME}: \"{line}\"",
                index + 1
            );
        }
        entries.push(entry);
    }

    Ok(entries)
}

/// 位置参数拆成原始条目：每个参数按 `;` 拆分，每段一个条目。
///
/// 分号让 P4V 的 prompt（一个输入框只能填一个参数）也能一次表达多个位置；
/// 多个位置参数与一个参数内多条完全等价。
fn parse_cli_entries(paths: &[String]) -> Vec<RawEntry> {
    let mut entries = Vec::new();

    for argument in paths {
        for segment in argument.split(';') {
            let segment = segment.trim();
            if segment.is_empty() {
                continue;
            }
            entries.push(parse_entry(segment));
        }
    }

    entries
}

/// 从 `start_dir` 起向上找 `.p4delta-scope`（含 start_dir 自身）。
fn find_scope_file(start_dir: &Path) -> Option<PathBuf> {
    let mut dir = Some(start_dir);

    while let Some(current) = dir {
        let candidate = current.join(SCOPE_FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = current.parent();
    }

    None
}

/// 从 `start` 起向上找最近的现存目录（含 `start` 自身）。
fn nearest_existing_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);

    while let Some(current) = dir {
        if current.is_dir() {
            return Some(current.to_path_buf());
        }
        dir = current.parent();
    }

    None
}

/// 配置查找的起点：第一个位置参数所在目录（文件取父目录）。
/// 没有参数、或第一个参数是 depot 路径（无法从它推本地位置）时用 cwd——
/// 「只用配置范围」的入口正是靠 cwd 这一路找到配置的。
fn config_start_dir(cli: &[RawEntry]) -> PathBuf {
    if let Some(entry) = cli.first()
        && !entry.path.starts_with("//")
    {
        let path = PathBuf::from(absolute_local_path(&entry.path));
        if path.is_dir() {
            return path;
        }
        if let Some(parent) = path.parent() {
            return parent.to_path_buf();
        }
    }

    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// depot 路径翻译成工作区路径；不在 client view 里时返回 `None`。
///
/// 翻译失败的路径不能原样留着：`//depot/...` 在 Windows 上会被当成 UNC 路径，
/// 扫盘时换来一个「目录名称无效」，而不是一条能读懂的提示。
async fn translate_depot_path(
    options: &Options,
    depot_path: &str,
    base_dir: Option<&Path>,
) -> Result<Option<String>> {
    let args = ["-Mj", "-Ztag", "where"];
    let paths = [depot_path.to_owned()];

    // 在哪个目录问 `p4 where` 决定了它认哪个 client：配置文件里的 depot 路径按配置文件
    // 所在目录问，命令行参数按调用者自己的 cwd 问——后者正是用户手工敲 p4 时的行为。
    let current_dir = match base_dir {
        Some(dir) => dir.to_path_buf(),
        None => {
            env::current_dir().map_err(|e| anyhow!("Failed to get current directory: {}", e))?
        }
    };

    let result = run_p4_command_slice(
        options,
        &current_dir.to_string_lossy(),
        &args,
        &paths,
        false,
        FailureMode::Warn,
    )
    .await?;

    for line in result {
        let record: serde_json::Value = serde_json::from_str(&line)?;
        if record["depotFile"].as_str() == Some(depot_path)
            && let Some(path) = record["path"].as_str()
        {
            return Ok(Some(path.to_owned()));
        }
    }

    Ok(None)
}

/// 判定入口是目录还是文件：本地存在就按本地类型（符号链接算文件，与扫盘口径一致）；
/// 本地不存在时看有没有显式 `...` 后缀，没有就按文件——「本地已删除的文件」是
/// 合法且常用的入口形态，而「本地完全不存在、也没写 `...` 的目录」无从识别，
/// 只能要求显式写出来。
fn classify_entry(path: &str, explicit_dir: bool) -> EntryKind {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => EntryKind::Directory,
        Ok(_) => EntryKind::File,
        Err(_) => {
            if explicit_dir {
                EntryKind::Directory
            } else {
                EntryKind::File
            }
        }
    }
}

/// 原始条目 → 已定位的入口：depot 翻译、绝对化、盘符大写、类型判定。
///
/// `base_dir` 是相对路径的基准：配置文件里的条目以配置文件所在目录为基准（配置挪到
/// 子目录时，语义自然变成"以此目录为根"），命令行参数传 `None`，走
/// `std::path::absolute` 的默认基准（cwd）。
async fn resolve_entries(
    options: &Options,
    raw: &[RawEntry],
    base_dir: Option<&Path>,
) -> Result<Vec<ScopeEntry>> {
    let mut entries = Vec::with_capacity(raw.len());

    for entry in raw {
        let mut path = entry.path.clone();

        if path.starts_with("//") {
            match translate_depot_path(options, &path, base_dir).await? {
                Some(local) => path = local,
                None => {
                    eprintln!("Warning: skipping \"{path}\", it is not in this client's view.");
                    continue;
                }
            }
        }

        // `//depot/...` 翻译失败时保留原样，它在两个平台都被当成绝对路径，不会走这里。
        if let Some(base) = base_dir
            && !Path::new(&path).is_absolute()
        {
            path = base.join(&path).display().to_string();
        }

        path = absolute_local_path(&path);
        if let Some(first_letter) = path.get_mut(0..1) {
            first_letter.make_ascii_uppercase();
        }

        entries.push(ScopeEntry {
            path_lower: local_path_key(&path),
            kind: classify_entry(&path, entry.explicit_dir),
            path,
        });
    }

    Ok(entries)
}

/// 单对入口的交集，不相交时返回 None。
fn intersect_entry(a: &ScopeEntry, b: &ScopeEntry) -> Option<ScopeEntry> {
    match (a.kind, b.kind) {
        // 目录 ∩ 目录：取更深的那个（浅的整个包含深的）。
        (EntryKind::Directory, EntryKind::Directory) => {
            if a.path_lower == b.path_lower || path_is_under_key(&a.path_lower, &b.path_lower) {
                Some(a.clone())
            } else if path_is_under_key(&b.path_lower, &a.path_lower) {
                Some(b.clone())
            } else {
                None
            }
        }
        // 目录 ∩ 文件：文件落在目录之下才相交。
        (EntryKind::Directory, EntryKind::File) => (a.path_lower == b.path_lower
            || path_is_under_key(&b.path_lower, &a.path_lower))
        .then(|| b.clone()),
        (EntryKind::File, EntryKind::Directory) => (a.path_lower == b.path_lower
            || path_is_under_key(&a.path_lower, &b.path_lower))
        .then(|| a.clone()),
        // 文件 ∩ 文件：同一路径才相交。
        (EntryKind::File, EntryKind::File) => (a.path_lower == b.path_lower).then(|| a.clone()),
    }
}

/// 两组入口的交集。
fn intersect_entries(left: &[ScopeEntry], right: &[ScopeEntry]) -> Vec<ScopeEntry> {
    let mut result = Vec::new();

    for a in left {
        for b in right {
            if let Some(entry) = intersect_entry(a, b) {
                result.push(entry);
            }
        }
    }

    result
}

/// 去重并剔除被排除的入口：路径相同的只留一条；目录入口覆盖的子目录/文件入口丢掉。
fn dedupe_entries(entries: Vec<ScopeEntry>, excludes: &ExcludeSet) -> Vec<ScopeEntry> {
    let mut result: Vec<ScopeEntry> = Vec::new();

    'outer: for entry in entries {
        // 落在排除范围内的入口直接丢：exclude 无条件优先。
        if excludes.excludes_key(&entry.path_lower) {
            continue;
        }

        for existing in &result {
            // 已有同路径条目，或被已有的目录入口覆盖 → 丢掉新的。
            if existing.path_lower == entry.path_lower
                || (existing.kind == EntryKind::Directory
                    && path_is_under_key(&entry.path_lower, &existing.path_lower))
            {
                continue 'outer;
            }
        }

        // 新的目录入口覆盖已收的条目（子目录或其中的文件）→ 移除它们。
        if entry.kind == EntryKind::Directory {
            result.retain(|existing| {
                existing.path_lower != entry.path_lower
                    && !path_is_under_key(&existing.path_lower, &entry.path_lower)
            });
        }

        result.push(entry);
    }

    result.sort_by(|a, b| a.path_lower.cmp(&b.path_lower));
    result
}

/// 入口列表 → 排除集合。重复的条目只算一次（`-a;-a` 是同一件事说了两遍）。
fn build_exclude_set(entries: Vec<ScopeEntry>) -> ExcludeSet {
    let mut set = ExcludeSet::default();
    let mut seen: HashSet<String> = HashSet::new();

    for entry in entries {
        if !seen.insert(entry.path_lower.clone()) {
            continue;
        }

        set.labels.push(format!("-{}", entry.path));
        match entry.kind {
            EntryKind::Directory => set.dir_keys.push(entry.path_lower),
            EntryKind::File => {
                set.file_keys.insert(entry.path_lower);
            }
        }
    }

    set
}

/// 求值本轮范围：命令行条目与配置文件条目取交集、去重，得到实际处理的入口集合。
///
/// 组合规则：
/// - include = 配置 include ∩ 传入 include（任一侧没有 include 条目时以另一侧为准）
/// - exclude = 配置 exclude ∪ 传入 exclude，无条件优先
/// - 配置里只有 exclude 时，include 默认取**配置文件所在目录**（"根 + 排除"形状；
///   载体就在工作区根，于是这就是"默认整个工作区"）
pub(crate) async fn evaluate_scope(options: &Options) -> Result<Scope> {
    let cli = parse_cli_entries(&options.paths);

    // `--no-scope-file` 让配置整个缺席：范围只认命令行。给编辑器用——它的范围是「聚焦目录
    // 减排除项」，自己算好了，工作区里那份 `.p4delta-scope` 是给手工跑 CLI 的人写的，
    // 两者叠在一起会让编辑器看到的范围与用户以为的不一样。
    let mut scope_file = if options.no_scope_file {
        None
    } else {
        find_scope_file(&config_start_dir(&cli))
    };
    let mut config = Vec::new();
    let mut config_dir = None;

    if let Some(file) = &scope_file {
        let content = fs::read_to_string(file)
            .with_context(|| format!("Failed to read {}", file.display()))?;
        sayln!("Using scope file {}.", file.display());
        config = parse_scope_content(&content)?;

        // 只有注释与空行的配置等同于没有配置。"以配置目录为根"是留给"只写排除项"
        // 那种形态的；套到空配置上会静默把整个目录变成范围，与用户的预期相反，
        // clean/sync 下还可能是破坏性的。
        if config.is_empty() {
            eprintln!("Warning: {} has no entries, ignoring it.", file.display());
            scope_file = None;
        } else {
            config_dir = file.parent().map(Path::to_path_buf);
        }
    }

    let (config_includes, config_excludes): (Vec<_>, Vec<_>) =
        config.iter().cloned().partition(|entry| !entry.exclude);
    let (cli_includes, cli_excludes): (Vec<_>, Vec<_>) =
        cli.iter().cloned().partition(|entry| !entry.exclude);

    // 配置没有 include 时默认以配置文件所在目录为根——这正是"整个工作区减去排除项"。
    let config_includes = if config_includes.is_empty() {
        match &config_dir {
            Some(dir) => vec![RawEntry {
                exclude: false,
                path: dir.display().to_string(),
                explicit_dir: true,
            }],
            None => Vec::new(),
        }
    } else {
        config_includes
    };

    let config_includes = resolve_entries(options, &config_includes, config_dir.as_deref()).await?;
    let config_excludes = resolve_entries(options, &config_excludes, config_dir.as_deref()).await?;
    let cli_includes = resolve_entries(options, &cli_includes, None).await?;
    let cli_excludes = resolve_entries(options, &cli_excludes, None).await?;

    let cli_declared_includes = cli.iter().any(|entry| !entry.exclude);
    let includes = match (config_includes.is_empty(), cli_includes.is_empty()) {
        (true, _) => cli_includes,
        // 用户点名了路径，却没有一条能落到工作区（不在 client view 里的 depot 路径，
        // resolve_entries 已逐条警告过）。此时退回配置范围，等于把操作悄悄放大到用户
        // 没点过的东西上，clean/sync 下可能是破坏性的——宁可报错。
        (false, true) if cli_declared_includes => bail!(
            "Nothing to work on: none of the given paths could be located in this client's view."
        ),
        (false, true) => config_includes,
        (false, false) => intersect_entries(&config_includes, &cli_includes),
    };

    let mut excludes = build_exclude_set(config_excludes.into_iter().chain(cli_excludes).collect());
    // 生效的配置文件自身也登记为隐式排除：它落在工作区里，扫盘必须跳过（否则报成待新增）；
    // 而它一旦被提交进 depot（团队共享范围配置的常见做法），depot 记录也要一起剔——只剔
    // 一侧就会变成凭空的删除。
    if let Some(file) = &scope_file {
        excludes.exclude_implicit_file(local_path_key(&absolute_local_path(
            &file.display().to_string(),
        )));
    }
    let includes = dedupe_entries(includes, &excludes);

    if includes.is_empty() {
        // 三种空范围各有各的成因，报错要能直接指向它。
        if cli.is_empty() && scope_file.is_none() {
            bail!("No path given; pass the folder to work on.");
        }

        let cli_has_includes = cli.iter().any(|entry| !entry.exclude);
        let cli_has_excludes = cli.iter().any(|entry| entry.exclude);
        if scope_file.is_none() && cli_has_excludes && !cli_has_includes {
            bail!(
                "Nothing to work on: only exclusions were given, with nothing to exclude from. \
                 Add at least one folder or file."
            );
        }

        let describe = |entries: &[RawEntry]| {
            entries
                .iter()
                .map(|entry| {
                    if entry.exclude {
                        format!("-{}", entry.path)
                    } else {
                        entry.path.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        // 排除项必须列出来：`scope: src` 配 `given: src/generated` 看上去明明相交，
        // 真正的成因（`-src/generated` 把入口排掉了）全在那一行里。
        let excluded = if excludes.declared_len() > 0 {
            format!("\n  excluded: {}", excludes.describe())
        } else {
            String::new()
        };
        bail!(
            "Nothing to work on: the given paths do not overlap the configured scope.{excluded}\n  \
             scope ({}): {}\n  given: {}",
            scope_file
                .as_ref()
                .map(|file| file.display().to_string())
                .unwrap_or_else(|| "no scope file".to_owned()),
            describe(&config),
            describe(&cli)
        );
    }

    // `first_dir` 要拿去当 p4 子进程的 cwd（相对路径解析、client 定位都靠它），必须真实
    // 存在：入口本身可能是本地已删除的文件，连它的父目录都可能跟着一起没了，所以往上找
    // 最近的现存祖先，而不是照着路径拼一个。取 `includes[0]`——入口已按路径键排序，
    // 所以「哪条入口定的 cwd」在多次运行之间是确定的。
    let anchor = Path::new(&includes[0].path);
    let anchor = match includes[0].kind {
        EntryKind::Directory => anchor,
        EntryKind::File => anchor.parent().unwrap_or(anchor),
    };
    let first_dir = nearest_existing_dir(anchor)
        .unwrap_or_else(|| anchor.to_path_buf())
        .display()
        .to_string();

    if excludes.declared_len() > 0 {
        sayln!(
            "Scope has {} include entr{} and {} exclusion(s).",
            includes.len(),
            if includes.len() == 1 { "y" } else { "ies" },
            excludes.declared_len()
        );
    }

    Ok(Scope {
        includes,
        excludes,
        first_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, kind: EntryKind) -> ScopeEntry {
        ScopeEntry {
            path: path.to_owned(),
            path_lower: local_path_key(path),
            kind,
        }
    }

    /// 平台形状的测试路径。`local_path_key` 与 `path_is_under_key` 都按 `MAIN_SEPARATOR`
    /// 切组件，硬编码的 `C:\ws\...` 在 Unix 上只是一串普通字符，父子关系判不出来。
    fn ws_path(parts: &[&str]) -> String {
        let sep = std::path::MAIN_SEPARATOR;
        format!("C:{sep}ws{sep}{}", parts.join(&sep.to_string()))
    }

    // ---- 条目解析 ----

    #[test]
    fn entries_split_on_semicolons_and_mark_exclusions() {
        let paths = vec!["a;b;c".to_owned(), "d.txt".to_owned(), "-e".to_owned()];
        let entries = parse_cli_entries(&paths);

        let described: Vec<(bool, &str)> = entries
            .iter()
            .map(|entry| (entry.exclude, entry.path.as_str()))
            .collect();

        assert_eq!(
            described,
            [
                (false, "a"),
                (false, "b"),
                (false, "c"),
                (false, "d.txt"),
                (true, "e"),
            ]
        );
    }

    #[test]
    fn blank_segments_are_ignored() {
        assert!(parse_cli_entries(&["".to_owned()]).is_empty());
        assert!(parse_cli_entries(&["  ;  ".to_owned()]).is_empty());
    }

    #[test]
    fn wildcard_suffix_marks_an_explicit_directory() {
        let entry = parse_entry("some-folder/...");

        assert!(entry.explicit_dir);
        assert_eq!(entry.path, "some-folder");
    }

    #[test]
    fn depot_entries_keep_their_forward_slashes() {
        // Windows 上 `normalize_local_path` 会把 `/` 全折成 `\`，`//depot/...` 于是变成
        // UNC 形状的 `\\depot\...`，后面的 depot 翻译再也认不出它。
        let directory = parse_entry("//depot/main/src/...");
        assert!(directory.explicit_dir);
        assert_eq!(directory.path, "//depot/main/src");

        let excluded = parse_entry("-//depot/main/tmp/...");
        assert!(excluded.exclude);
        assert_eq!(excluded.path, "//depot/main/tmp");

        let file = parse_entry("//depot/main/src/lib.txt");
        assert!(!file.explicit_dir);
        assert_eq!(file.path, "//depot/main/src/lib.txt");
    }

    #[test]
    fn a_lone_dash_prefix_is_an_exclusion_without_a_wildcard() {
        let entry = parse_entry("-old.txt");

        assert!(entry.exclude);
        assert!(!entry.explicit_dir);
        assert_eq!(entry.path, "old.txt");
    }

    // ---- 入口类型判定 ----

    /// 本地存在的按本地类型判（符号链接算文件，与扫盘口径一致）；本地不存在的看有没有
    /// 显式 `...`——没有就当文件，因为「本地已删除的文件」是合法入口形态。
    #[test]
    fn classify_entry_uses_the_local_type_and_falls_back_to_the_wildcard() {
        let tree = crate::test_util::TempTree::new("classify-entry");
        let directory = tree.dir("src");
        let file = tree.file("src/lib.txt", "x\n");

        assert_eq!(
            classify_entry(&directory.display().to_string(), false),
            EntryKind::Directory
        );
        assert_eq!(
            classify_entry(&file.display().to_string(), false),
            EntryKind::File
        );

        let missing = tree.root.join("gone.txt").display().to_string();
        assert_eq!(classify_entry(&missing, false), EntryKind::File);
        assert_eq!(classify_entry(&missing, true), EntryKind::Directory);
    }

    // ---- 配置文件解析 ----

    #[test]
    fn scope_content_skips_comments_and_blank_lines() {
        let entries = parse_scope_content("# comment\n\na\n\n-b\n").unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "a");
        assert!(entries[1].exclude);
    }

    #[test]
    fn an_empty_path_in_the_scope_file_is_an_error() {
        // 只有一个 `-` 的条目剥掉前缀后什么都不剩，是明确的写法错误。
        let error = parse_scope_content("a\n-\n").unwrap_err().to_string();
        assert!(error.contains("line 2"), "{error}");
    }

    // ---- 交集 ----

    #[test]
    fn directory_intersections_take_the_deeper_side() {
        let outer = entry(&ws_path(&["a"]), EntryKind::Directory);
        let inner = entry(&ws_path(&["a", "b"]), EntryKind::Directory);
        let unrelated = entry(&ws_path(&["c"]), EntryKind::Directory);

        assert_eq!(
            intersect_entry(&outer, &inner).unwrap().path_lower,
            inner.path_lower
        );
        assert_eq!(
            intersect_entry(&inner, &outer).unwrap().path_lower,
            inner.path_lower
        );
        assert!(intersect_entry(&outer, &unrelated).is_none());
    }

    #[test]
    fn a_file_intersects_only_the_directory_that_contains_it() {
        let dir = entry(&ws_path(&["a"]), EntryKind::Directory);
        let file = entry(&ws_path(&["a", "b", "d.txt"]), EntryKind::File);
        let other_file = entry(&ws_path(&["z", "d.txt"]), EntryKind::File);

        assert_eq!(
            intersect_entry(&dir, &file).unwrap().path_lower,
            file.path_lower
        );
        assert!(intersect_entry(&dir, &other_file).is_none());
    }

    #[test]
    fn files_intersect_only_on_the_exact_path() {
        let a = entry(&ws_path(&["d.txt"]), EntryKind::File);
        let same = entry(&ws_path(&["d.txt"]).to_ascii_uppercase(), EntryKind::File);
        let other = entry(&ws_path(&["e.txt"]), EntryKind::File);

        assert!(intersect_entry(&a, &same).is_some());
        assert!(intersect_entry(&a, &other).is_none());
    }

    #[test]
    fn a_file_path_equal_to_a_directory_path_still_intersects() {
        // 本地不存在时类型判定可能与实际不符，同一路径的两种判定要能相交。
        let dir = entry(&ws_path(&["thing"]), EntryKind::Directory);
        let file = entry(&ws_path(&["thing"]), EntryKind::File);

        assert!(intersect_entry(&dir, &file).is_some());
        assert!(intersect_entry(&file, &dir).is_some());
    }

    // ---- 去重与排除 ----

    #[test]
    fn a_directory_entry_swallows_entries_beneath_it() {
        let includes = vec![
            entry(&ws_path(&["a", "b", "c.txt"]), EntryKind::File),
            entry(&ws_path(&["a"]), EntryKind::Directory),
            entry(&ws_path(&["a", "b"]), EntryKind::Directory),
        ];

        let deduped = dedupe_entries(includes, &ExcludeSet::default());

        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].path_lower, local_path_key(&ws_path(&["a"])));
    }

    #[test]
    fn duplicate_file_entries_are_kept_once() {
        let includes = vec![
            entry(&ws_path(&["d.txt"]), EntryKind::File),
            entry(&ws_path(&["d.txt"]).to_ascii_uppercase(), EntryKind::File),
        ];

        assert_eq!(dedupe_entries(includes, &ExcludeSet::default()).len(), 1);
    }

    #[test]
    fn exclusion_beats_inclusion_unconditionally() {
        let includes = vec![entry(&ws_path(&["d.txt"]), EntryKind::File)];
        let excludes = build_exclude_set(vec![entry(&ws_path(&["d.txt"]), EntryKind::File)]);

        assert!(dedupe_entries(includes, &excludes).is_empty());
    }

    #[test]
    fn an_exclusion_directory_covers_files_and_subdirectories() {
        let excludes = build_exclude_set(vec![entry(&ws_path(&["gen"]), EntryKind::Directory)]);

        assert!(excludes.excludes_key(&local_path_key(&ws_path(&["gen"]))));
        assert!(excludes.excludes_key(&local_path_key(&ws_path(&["gen", "out", "a.txt"]))));
        assert!(!excludes.excludes_key(&local_path_key(&ws_path(&["gen2", "a.txt"]))));
    }

    // ---- file spec ----

    #[test]
    fn directory_file_specs_get_the_wildcard() {
        let sep = std::path::MAIN_SEPARATOR;
        let dir = entry(&format!("c:{sep}ws{sep}a"), EntryKind::Directory);
        let file = entry(&format!("c:{sep}ws{sep}d.txt"), EntryKind::File);

        assert_eq!(dir.file_spec(), format!("c:{sep}ws{sep}a{sep}..."));
        assert_eq!(file.file_spec(), format!("c:{sep}ws{sep}d.txt"));
    }
}
