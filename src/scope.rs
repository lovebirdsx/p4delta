//! 操作范围：`client root 上的 .p4delta-scope` 与命令行目标求值成本轮实际处理的入口集合。
//!
//! 两件事在这里定下来，而且都是**硬上限**：
//!
//! - **归属固定**：范围配置只从该 client 已确认的 root 读取，相对路径也以它为基准。
//!   打开子目录、换个参数顺序都不会换一份配置——`find_scope_file` 那种「往上找」已经删掉，
//!   因为「哪一份配置生效」不该取决于调用者站在哪儿。
//! - **集合代数不变**：include 取交集、exclude 取并集且无条件优先，去重后按路径键排序。
//!
//! 命令行一侧的输入形态也跟着收紧：**每个 argv 就是一条目标**，不再按 `;` 拆分、不再认
//! `-` 前缀那套排除 DSL，排除项走 `--exclude-dir` / `--exclude-file`。这些都是普通参数，
//! 与「机器生成的路径不必回灌文本语法」是同一件事：文本语法里 `;` 是分隔符、路径里的
//! `#` `@` `%` 会被 p4 当语法读，回来一趟就丢了对应用户输入的原样。
//!
//! **路径的两个身份要分清**：入口装的是**本地原始路径**（用户写下的那串字符），p4 的
//! file spec 是在边界上现转的——[`escape_file_spec`] 只在交给 p4 之前跑一次，所以
//! 名字里带 `#` `@` `%` `*` `?` 的文件既不用降级、也不会被读成别的规格。

use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::cli::Options;
use crate::json::sayln;
use crate::p4::process::{FailureMode, run_p4_command_slice};
use crate::path::{
    canonical_local_path, directory_key, escape_file_spec, local_path_key, normalize_local_path,
    path_is_under_key, strip_depot_wildcard_suffix,
};
use crate::scope_config::{ConfigEntry, ScopeConfig, parse_scope_config};

/// 持久范围配置的文件名。固定放在 client root 下，不再向上查找。
pub(crate) const SCOPE_FILE_NAME: &str = ".p4delta-scope";

/// 配置文件的大小上限。配置是人手写的几十行，超过这个量级多半是拿错了文件；
/// 无界读取会让「读一份配置」变成一次没有上界的分配。
const MAX_SCOPE_FILE_BYTES: u64 = 1024 * 1024;

/// 入口是目录还是文件。决定 file spec 的形状（目录补 `/...`）与排除时的匹配方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum EntryKind {
    Directory,
    File,
}

/// 一个已定位的范围入口。
#[derive(Debug, Clone)]
pub(crate) struct ScopeEntry {
    /// 绝对本地路径，保留原始大小写（盘符已统一成大写）。
    pub(crate) path: String,

    /// 平台路径身份键（见 [`local_path_key`]），与 p4 返回的 clientFile 同一口径。
    pub(crate) path_lower: String,

    pub(crate) kind: EntryKind,
}

impl ScopeEntry {
    pub(crate) fn new(path: String, kind: EntryKind) -> Self {
        let path_lower = local_path_key(&path);
        ScopeEntry {
            path,
            path_lower,
            kind,
        }
    }

    /// 给 p4 的 file spec：目录要补 `...`（裸目录不是合法 file spec，会报
    /// `no such file(s)`），文件就是路径本身。元字符在这一步转义，见 [`escape_file_spec`]。
    pub(crate) fn file_spec(&self) -> String {
        match self.kind {
            EntryKind::Directory => directory_file_spec(&escape_file_spec(&self.path)),
            EntryKind::File => escape_file_spec(&self.path),
        }
    }
}

/// 目录的 file spec：路径 + `...` 后缀，路径自己已经带尾分隔符时不再补一个。
///
/// 盘根（`C:\`、Unix 的 `/`）与 UNC 共享根（`\\server\share\`）都是「整条就是分隔符结尾」的
/// 形状。补出来的是重复分隔符，而 Unix 上的 `//...` 尤其危险：p4 把 `//` 开头读成 depot
/// 路径，范围就从「本地根」悄悄变成「整个 depot」。
fn directory_file_spec(path: &str) -> String {
    if path.ends_with(std::path::MAIN_SEPARATOR) {
        format!("{path}...")
    } else {
        format!("{path}{}...", std::path::MAIN_SEPARATOR)
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

    /// 用户声明的排除项（原始大小写与类型），面向报错信息与计数。
    entries: Vec<ScopeEntry>,

    /// 隐式排除：生效的那份 `.p4delta-scope` 自身。它必须走这条两侧共用的通路——
    /// 扫盘要跳过它（否则会被报成待新增），而一旦它被提交进 depot（团队共享范围配置的
    /// 常见做法），depot 记录也必须一起剔：只剔一侧，它就会因为「本地扫不到」被误判成
    /// 待删除，`-a` 下真的下发 `p4 delete`，把共享的配置删掉。
    implicit_file_key: Option<String>,
}

impl ExcludeSet {
    /// 构造一个只含目录排除项的集合。仅供测试：产品路径上只有 [`build_exclude_set`] 会造它。
    ///
    /// 键照 [`ScopeEntry::new`] 的口径归一——直接拿用户写的原文当键，在大小写不一致的
    /// 平台上会静默匹配不上，而测试恰恰要验的就是匹配本身。
    #[cfg(test)]
    pub(crate) fn from_dir_keys(keys: &[&str]) -> Self {
        let mut set = ExcludeSet::default();
        for key in keys {
            set.push(ScopeEntry::new((*key).to_owned(), EntryKind::Directory));
        }
        set
    }

    fn push(&mut self, entry: ScopeEntry) {
        match entry.kind {
            EntryKind::Directory => self.dir_keys.push(entry.path_lower.clone()),
            EntryKind::File => {
                self.file_keys.insert(entry.path_lower.clone());
            }
        }
        self.entries.push(entry);
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

    pub(crate) fn declared_len(&self) -> usize {
        self.entries.len()
    }

    /// 用户声明的排除项（**不含**隐式的配置文件自身），面向报错信息与共享契约向量。
    #[cfg(test)]
    pub(crate) fn entries(&self) -> &[ScopeEntry] {
        &self.entries
    }

    /// 面向报错信息的排除项清单，`dir "a", file "b"` 形状。
    pub(crate) fn describe(&self) -> String {
        describe_entries(&self.entries)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.declared_len() == 0 && self.implicit_file_key.is_none()
    }
}

/// 本轮实际处理的全部范围。
#[derive(Debug)]
pub(crate) struct Scope {
    /// include 入口：已去重、无覆盖、无重复，按路径键排序。
    pub(crate) includes: Vec<ScopeEntry>,

    /// 排除集合。
    pub(crate) excludes: ExcludeSet,

    /// p4 子进程的 cwd（`.p4config` / P4IGNORE 的发现跟着它走）。
    ///
    /// 它是**已确认的 client root**，与入口列表无关：入口可能落在任意子目录，而
    /// 「这一轮连的是哪个 client、按哪份 `.p4config`」不该跟着某个可选中的目录漂。
    pub(crate) work_dir: String,
}

impl Scope {
    /// 全部入口的 file spec（已转义），交给 p4 做范围查询。`changelist` 非空时把目标 CL
    /// 钉在**每一条**规格上：`@CL` 是 per-spec 的，漏掉哪条哪条就跑到 head 去，而输出上
    /// 看不出来。
    pub(crate) fn query_specs(&self, changelist: Option<u32>) -> Vec<String> {
        self.includes
            .iter()
            .map(|entry| match changelist {
                Some(cl) => format!("{}@{cl}", entry.file_spec()),
                None => entry.file_spec(),
            })
            .collect()
    }

    /// 路径键是否落在某个入口之内（不含排除判断，那一侧走 [`ExcludeSet::excludes_key`]）。
    ///
    /// 普通同步拿它验证原生候选确实在范围内：目录入口按子树算，文件入口要精确相等。
    /// 只看路径文本，**不要求磁盘上存在**——「本地已删除、depot 还有」的候选正是要处理的。
    pub(crate) fn includes_key(&self, key: &str) -> bool {
        entries_cover(&self.includes, key)
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

/// 入口列表是否覆盖某个路径键（不含排除判断）。
pub(crate) fn entries_cover(entries: &[ScopeEntry], key: &str) -> bool {
    entries.iter().any(|entry| match entry.kind {
        EntryKind::Directory => {
            key == entry.path_lower || path_is_under_key(key, &entry.path_lower)
        }
        EntryKind::File => key == entry.path_lower,
    })
}

/// 本轮的入口与排除项是否覆盖 `path`：两侧都要过。共用契约的向量拿它断言成员关系。
#[cfg(test)]
pub(crate) fn scope_covers(includes: &[ScopeEntry], excludes: &ExcludeSet, path: &str) -> bool {
    let key = local_path_key(path);
    entries_cover(includes, &key) && !excludes.excludes_key(&key)
}

fn describe_entries(entries: &[ScopeEntry]) -> String {
    entries
        .iter()
        .map(|entry| match entry.kind {
            EntryKind::Directory => format!("dir \"{}\"", entry.path),
            EntryKind::File => format!("file \"{}\"", entry.path),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// ---- 范围配置：固定 client root 下的一份文件 ----

/// 一份生效的范围配置，连同它的位置（报错信息与自身排除都要用）。
#[derive(Debug)]
pub(crate) struct ScopeFile {
    pub(crate) path: String,
    pub(crate) config: ScopeConfig,
}

/// 读 client root 下的 `.p4delta-scope`。
///
/// 只有 **ENOENT** 才是「没有配置」：权限不足、路径是目录、编码不是 UTF-8、正文不是合法
/// JSON 都不是——把它们读成「没有配置」会让范围**放大**成整个 root，而调用方以为配置还在。
pub(crate) fn read_scope_file(client_root: &str, no_scope_file: bool) -> Result<Option<ScopeFile>> {
    if no_scope_file {
        return Ok(None);
    }

    let path = Path::new(client_root).join(SCOPE_FILE_NAME);
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read {}", path.display()));
        }
    };

    if !metadata.is_file() {
        bail!(
            "{} is not a regular file. A missing config means \"no extra limit inside the client \
             root\"; anything else is an error rather than silently the whole client.",
            path.display()
        );
    }
    if metadata.len() > MAX_SCOPE_FILE_BYTES {
        bail!(
            "{} is {} bytes, above the {MAX_SCOPE_FILE_BYTES} byte limit for a scope config.",
            path.display(),
            metadata.len()
        );
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    fs::File::open(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?
        .take(MAX_SCOPE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    if bytes.len() as u64 > MAX_SCOPE_FILE_BYTES {
        bail!(
            "{} grew past the {MAX_SCOPE_FILE_BYTES} byte limit while it was being read.",
            path.display()
        );
    }

    let text = String::from_utf8(bytes)
        .with_context(|| format!("{} is not valid UTF-8.", path.display()))?;
    let config =
        parse_scope_config(&text).with_context(|| format!("Invalid {}", path.display()))?;

    sayln!("Using scope file {}.", path.display());
    Ok(Some(ScopeFile {
        path: canonical_local_path(&path.display().to_string()),
        config,
    }))
}

// ---- client root：范围归属与 p4 子进程 cwd 的那一个根 ----

/// 固定启动 cwd：命令行一侧解析连接、探测字符集与翻译 depot 路径都以它为基准。
///
/// 刻意不看第一个位置参数——那会让「同一批参数换个顺序」变成另一个连接与另一份配置，
/// 而范围归属必须只由显式的 workspace / client 决定。
pub(crate) fn startup_dir() -> String {
    env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .display()
        .to_string()
}

/// 本轮用哪个 client root。
///
/// `--client-root` **不是**任意的配置目录选择器，`p4 info` 报的根也不是。唯一权威是这个 client
/// spec 里那个**固定的** `Root`——`p4 info` 的 `clientRoot` 随 cwd 变，cwd 落在某个 AltRoot 上
/// 时它报的就是那个 AltRoot，直接采信就会把范围配置的归属定在那棵多根布局的树上。所以这里
/// 一律先查 spec 的固定 Root，`p4 info` 只用来核对「这次调用不是从 AltRoot 里发起的」。
///
/// 对不上、拿不到、或 cwd 停在 AltRoot 上，一律失败关闭：换一个根就等于换一份范围配置。
pub(crate) async fn resolve_client_root(options: &Options, client: &str) -> Result<String> {
    let work_dir = startup_dir();
    let spec = read_client_spec_roots(options, &work_dir).await?;
    let Some(fixed) = spec.root.as_deref() else {
        bail!(
            "Could not determine the root of client \"{client}\": its client spec has no Root (or \
             could not be read). The scope file is read from that root, so there is no safe \
             default here."
        );
    };

    if let Some(given) = options.client_root.as_deref() {
        if directory_key(given) == directory_key(fixed) {
            return Ok(canonical_local_path(fixed));
        }
        if spec
            .alt_roots
            .iter()
            .any(|alt| directory_key(alt) == directory_key(given))
        {
            bail!(
                "--client-root \"{given}\" is an AltRoot of client \"{client}\". This tool keeps the \
                 scope file and every p4 subprocess under one root, so AltRoots layouts are not \
                 supported: pass the client's primary Root ({fixed})."
            );
        }
        bail!(
            "--client-root \"{given}\" is not the root of client \"{client}\" (the client spec says \
             {fixed}). The scope file is read from the client root, so a different path there would \
             mean a different scope."
        );
    }

    // 没有 `--client-root`：固定 Root 就是答案，但先核对 cwd 没有站在某个 AltRoot 上。
    // `p4 info` 按 cwd 认根，报出一个与固定 Root 不同的目录，说明这次调用来自另一个树：
    // 位置参数里的相对路径以启动 cwd 为基准，而范围配置以固定 Root 为基准，两者一旦不是
    // 同一个树，入口与范围就会各指一处——那种「跑通了」是错的，直接拒绝。
    match p4_info_client_root(options, &work_dir).await {
        Some(reported) if directory_key(&reported) != directory_key(fixed) => bail!(
            "The current directory sits under an AltRoot of client \"{client}\": p4 info reports \
             {reported}, while the client spec's fixed Root is {fixed}. This tool keeps the scope \
             file and every p4 subprocess under one root, so AltRoots layouts are not supported: \
             run it from the client's primary Root, or name that Root with --client-root."
        ),
        Some(_) => Ok(canonical_local_path(fixed)),
        None => bail!(
            "Could not determine the root of client \"{client}\": p4 info reported no clientRoot, \
             so there is no way to tell whether this directory sits on an AltRoot. The scope file \
             is read from that root, so there is no safe default here."
        ),
    }
}

async fn p4_info_client_root(options: &Options, work_dir: &str) -> Option<String> {
    let args: [&str; 3] = ["-Mj", "-Ztag", "info"];
    match run_p4_command_slice(options, work_dir, &args, &[], false, FailureMode::Warn).await {
        Ok(lines) => lines.iter().find_map(|line| {
            let record: serde_json::Value = serde_json::from_str(line).ok()?;
            record["clientRoot"]
                .as_str()
                .filter(|root| !root.trim().is_empty())
                .map(str::to_owned)
        }),
        Err(error) => {
            eprintln!("Warning: failed to read the client root from p4 info: {error}");
            None
        }
    }
}

#[derive(Debug, Default)]
struct ClientSpecRoots {
    root: Option<String>,
    alt_roots: Vec<String>,
}

/// 读 client spec 的 `Root` 与 `AltRoots`——本轮范围的唯一权威。
///
/// 不带参数：p4 子进程已经带着本轮的 client 跑（`-c <workspace>`），
/// `p4 client -o` 取的就是它。p4 报错即整轮失败（不是「拿不到就猜」）：固定 Root 是范围的归属，
/// 读不出来就没有第二来源。
async fn read_client_spec_roots(options: &Options, work_dir: &str) -> Result<ClientSpecRoots> {
    let args: [&'static str; 2] = ["client", "-o"];
    let lines =
        run_p4_command_slice(options, work_dir, &args, &[], false, FailureMode::ExitCode).await?;

    let mut roots = ClientSpecRoots::default();
    let mut in_alt_roots = false;

    for line in &lines {
        if let Some(value) = line.strip_prefix("Root:") {
            in_alt_roots = false;
            let value = value.trim();
            if !value.is_empty() {
                roots.root = Some(value.to_owned());
            }
            continue;
        }
        if let Some(inline) = line.strip_prefix("AltRoots:") {
            in_alt_roots = true;
            let inline = inline.trim();
            if !inline.is_empty() {
                roots.alt_roots.push(inline.to_owned());
            }
            continue;
        }
        if in_alt_roots {
            // 表单里续行以空白开头；遇到下一个字段（顶格）就结束。
            if line.starts_with([' ', '\t']) && !line.trim().is_empty() {
                roots.alt_roots.push(line.trim().to_owned());
                continue;
            }
            in_alt_roots = false;
        }
    }

    Ok(roots)
}

/// p4 子进程的 cwd：client root 本身，root 本地不存在时退到最近的现存祖先
/// （拿一个不存在的目录当 cwd，连查询都起不来）。
pub(crate) fn work_dir_for(client_root: &str) -> String {
    let path = Path::new(client_root);
    nearest_existing_dir(path)
        .unwrap_or_else(|| path.to_path_buf())
        .display()
        .to_string()
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

// ---- 命令行一侧：目标与排除项 ----

/// 窄范围的立场差异。
///
/// [`ScopePolicy::Lenient`] 是 open / clean / 强制同步的既有行为：一条 depot 路径不在
/// client view 里就警告一声跳过，其余入口照常处理。
///
/// [`ScopePolicy::Strict`] 是普通同步用的：那条路径会被写进「原生要对它做什么」的查询，
/// 静默丢掉它等于让 p4 在一个**比用户以为的更小**的范围上作答，而工具随后会把这个答案
/// 当成完整结论报出去。范围是硬上限，缩小它同样要报错。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScopePolicy {
    Lenient,
    Strict,
}

/// 一条命令行目标解析出来的样子。
#[derive(Debug, Clone)]
struct RawTarget {
    /// 路径文本（`...` 后缀已剥掉）。
    path: String,

    /// 原来带 `...` 后缀：本地不存在时也按目录处理。
    explicit_dir: bool,
}

/// 一条位置参数 → 一条目标。
///
/// **每个 argv 就是一条目标**：不按 `;` 拆、不认 `-` 前缀。路径文本原样保留，只在去掉
/// 「显式的递归后缀」时碰它——`...` 是这一侧唯一容许的形态标记，用来表达「本地还不存在、
/// 但按目录处理」（`classify_entry` 在没有它时按本地类型判）。
///
/// 不做 `trim`：路径里的空格是文件名的一部分（见 README 的范围一节），悄悄裁掉会让
/// 目标指向另一个文件。空白参数（只有空格）本身是空的，等于没给。
fn parse_target(argument: &str) -> Option<RawTarget> {
    if argument.trim().is_empty() {
        return None;
    }

    // depot 路径单独处理：它永远用正斜杠，不能过 `normalize_local_path`——Windows 上
    // 那会把 `//depot/main/src/...` 折成 UNC 形状的 `\\depot\main\src`，后面的 depot
    // 翻译就认不出它了，只剩一条通向 `\\depot\...` 的扫盘路径。
    if argument.starts_with("//") {
        let stripped = argument.trim_end_matches("/...");
        return Some(RawTarget {
            explicit_dir: stripped.len() != argument.len(),
            path: stripped.to_owned(),
        });
    }

    // 先归一分隔符再剥后缀：用户可能写正斜杠，也可能写平台分隔符。
    let normalized = normalize_local_path(argument);
    let stripped = strip_depot_wildcard_suffix(&normalized);

    Some(RawTarget {
        explicit_dir: stripped.len() != normalized.len(),
        path: stripped.to_owned(),
    })
}

/// 判定目标在本地是目录还是文件：本地存在就按本地类型（符号链接算文件，与扫盘口径一致）；
/// 本地不存在时看有没有显式 `...` 后缀，没有就按文件——「本地已删除的文件」是合法且常用的
/// 目标形态，而「本地完全不存在、也没写 `...` 的目录」无从识别，只能要求显式写出来。
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

/// 位置参数解析出来的目标。
pub(crate) struct ResolvedTargets {
    pub(crate) entries: Vec<ScopeEntry>,

    /// 命令行上确实给了目标（哪怕一条都没能定位）。报错分支要区分「没给」与「给了但落空」。
    pub(crate) declared: bool,
}

/// 把位置参数解析成本地入口：depot 路径翻译、绝对化、盘符大写、类型判定。
pub(crate) async fn resolve_targets(
    options: &Options,
    policy: ScopePolicy,
) -> Result<ResolvedTargets> {
    let work_dir = startup_dir();
    let mut entries = Vec::new();
    let mut declared = false;

    for argument in &options.paths {
        let Some(target) = parse_target(argument) else {
            continue;
        };
        declared = true;

        let mut path = target.path.clone();
        if path.starts_with("//") {
            match translate_depot_path(options, &path, &work_dir).await? {
                Some(local) => path = local,
                // 严格档逐条报错，而不是先把定位不了的都丢掉再看剩没剩：
                // 这样「给的目标一条都没能定位」根本走不到后面。
                None if policy == ScopePolicy::Strict => bail!(
                    "Nothing to work on: \"{path}\" is not in this client's view, so the scope \
                     cannot be evaluated."
                ),
                None => {
                    eprintln!("Warning: skipping \"{path}\", it is not in this client's view.");
                    continue;
                }
            }
        }

        let path = canonical_local_path(&path);
        let kind = classify_entry(&path, target.explicit_dir);
        entries.push(ScopeEntry::new(path, kind));
    }

    Ok(ResolvedTargets { entries, declared })
}

/// `--exclude-dir` / `--exclude-file` → 排除项。
///
/// 相对路径的基准是 **client root**（编辑器传的是绝对路径）；类型由参数自己声明，不去 stat
/// 猜——「本地还不存在的输出目录」正是最常见的一条。不拆 `;`，也不回灌 `-` 前缀那套文本语法。
pub(crate) fn resolve_cli_excludes(options: &Options, client_root: &str) -> Vec<ScopeEntry> {
    let mut entries = Vec::new();

    for (values, kind) in [
        (&options.exclude_dir, EntryKind::Directory),
        (&options.exclude_file, EntryKind::File),
    ] {
        for value in values {
            if value.trim().is_empty() {
                continue;
            }

            let normalized = normalize_local_path(value);
            let stripped: &str = strip_depot_wildcard_suffix(&normalized);
            let path = if Path::new(stripped).is_absolute() {
                canonical_local_path(stripped)
            } else {
                canonical_local_path(&Path::new(client_root).join(stripped).display().to_string())
            };
            entries.push(ScopeEntry::new(path, kind));
        }
    }

    entries
}

/// depot 路径翻译成工作区路径；不在 client view 里时返回 `None`。
///
/// 翻译失败的路径不能原样留着：`//depot/...` 在 Windows 上会被当成 UNC 路径，
/// 扫盘时换来一个「目录名称无效」，而不是一条能读懂的提示。
async fn translate_depot_path(
    options: &Options,
    depot_path: &str,
    work_dir: &str,
) -> Result<Option<String>> {
    let args = ["-Mj", "-Ztag", "where"];
    let paths = [depot_path.to_owned()];

    // 在哪个目录问 `p4 where` 决定了它认哪个 client：与这一轮其余查询同一处（固定启动 cwd），
    // 不再「命令行参数按调用者 cwd 问」——那会让连接的判据跟着参数走。
    let result =
        run_p4_command_slice(options, work_dir, &args, &paths, false, FailureMode::Warn).await?;

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

// ---- 集合代数 ----

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

/// 入口列表 → 排除集合。重复的条目只算一次（同一个目录说两遍是同一件事）。
fn build_exclude_set(entries: Vec<ScopeEntry>) -> ExcludeSet {
    let mut set = ExcludeSet::default();
    let mut seen: HashSet<String> = HashSet::new();

    for entry in entries {
        if !seen.insert(entry.path_lower.clone()) {
            continue;
        }

        set.push(entry);
    }

    set
}

/// 把三样东西合成这一轮的入口与排除项：client root、范围配置、命令行。
///
/// 组合规则：
/// - include = **配置 ∩ 目标**。配置没写 `include` 时它默认是 client root 本身，
///   于是「只写排除项」自然落成「整个 root 减去排除项」；`include: []` 是**明确的空集**，
///   给了目标也不会被放大。
/// - exclude = 配置 ∪ `--exclude-*`，无条件优先。
/// - 配置整个缺席时，范围只由目标决定；两边都没有才是「没给路径」。
///
/// 没有配置也没有目标时**报错**，而不是默认整个 root：不给范围就把整棵 client 过一遍，
/// 是这套契约里最不该有的默认值（`--clean -a` 下它是破坏性的）。
pub(crate) fn combine_scope(
    client_root: &str,
    scope_file: Option<&ScopeFile>,
    targets: &[ScopeEntry],
    targets_declared: bool,
    cli_excludes: &[ScopeEntry],
) -> Result<(Vec<ScopeEntry>, ExcludeSet)> {
    let config = scope_file.map(|file| &file.config);

    if targets_declared && targets.is_empty() {
        bail!(
            "Nothing to work on: none of the given paths could be located in this client's view."
        );
    }
    if scope_file.is_none() && targets.is_empty() {
        bail!("No path given; pass the folder to work on.");
    }

    // 配置贡献的基础范围。`None` = 整份配置缺席，此时范围只由目标决定。
    let config_includes: Option<Vec<ScopeEntry>> =
        config.map(|config| match config.include.as_ref() {
            Some(entries) => entries
                .iter()
                .map(|entry| resolve_config_entry(entry, client_root))
                .collect(),
            // 没写 `include`：整个 client root 就是基础范围，目标只能收窄它。
            None => vec![ScopeEntry::new(
                canonical_local_path(client_root),
                EntryKind::Directory,
            )],
        });
    let config_excludes: Vec<ScopeEntry> = config
        .map(|config| {
            config
                .exclude
                .iter()
                .map(|entry| resolve_config_entry(entry, client_root))
                .collect()
        })
        .unwrap_or_default();

    let includes = match config_includes {
        // 配置缺席：范围就是目标自己。空数组那档不必单列——`include: []` 与目标求交
        // 本来就得空集，给了目标也不会被放大。
        None => targets.to_vec(),
        Some(config_includes) if targets.is_empty() => config_includes,
        Some(config_includes) => intersect_entries(&config_includes, targets),
    };

    let mut excludes = build_exclude_set(
        config_excludes
            .into_iter()
            .chain(cli_excludes.iter().cloned())
            .collect(),
    );
    // 生效的配置文件自身也登记为隐式排除：它落在工作区里，扫盘必须跳过（否则报成待新增）；
    // 而它一旦被提交进 depot（团队共享范围配置的常见做法），depot 记录也要一起剔——只剔
    // 一侧就会变成凭空的删除。
    if let Some(file) = scope_file {
        excludes.exclude_implicit_file(local_path_key(&file.path));
    }

    let includes = dedupe_entries(includes, &excludes);
    if includes.is_empty() {
        let scope_name = scope_file
            .map(|file| file.path.clone())
            .unwrap_or_else(|| "no scope file".to_owned());
        let scope_shape = config
            .map(ScopeConfig::describe)
            .unwrap_or_else(|| "no scope file".to_owned());
        // 排除项必须列出来：`scope: src` 配 `given: src/generated` 看上去明明相交，
        // 真正的成因（`dir "src/generated"` 把入口排掉了）全在那一行里。
        let excluded = if excludes.declared_len() > 0 {
            format!("\n  excluded: {}", excludes.describe())
        } else {
            String::new()
        };
        let cli = if cli_excludes.is_empty() {
            String::new()
        } else {
            format!("\n  cli excludes: {}", describe_entries(cli_excludes))
        };

        bail!(
            "Nothing to work on: the given paths do not overlap the configured scope.{excluded}\n  \
             scope ({scope_name}): {scope_shape}\n  given: {}{cli}",
            describe_entries(targets)
        );
    }

    Ok((includes, excludes))
}

/// 配置条目 → 绝对入口：相对路径以 **client root** 为基准。
fn resolve_config_entry(entry: &ConfigEntry, client_root: &str) -> ScopeEntry {
    let path = if entry.path == "." {
        canonical_local_path(client_root)
    } else {
        let relative = entry.path.replace('/', std::path::MAIN_SEPARATOR_STR);
        canonical_local_path(&Path::new(client_root).join(relative).display().to_string())
    };

    ScopeEntry::new(path, entry.kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, kind: EntryKind) -> ScopeEntry {
        ScopeEntry::new(path.to_owned(), kind)
    }

    /// 平台形状的 client root。测试路径都得挂在它下面：`canonical_local_path` 会把相对路径
    /// 拼到 cwd 上，而 `C:\ws` 在 Unix 上正是一个相对路径。
    #[cfg(windows)]
    const ROOT: &str = r"C:\ws";
    #[cfg(not(windows))]
    const ROOT: &str = "/ws";

    /// 平台形状的测试路径。`local_path_key` 与 `path_is_under_key` 都按 `MAIN_SEPARATOR`
    /// 切组件，硬编码的 `C:\ws\...` 在 Unix 上只是一串普通字符，父子关系判不出来。
    fn ws_path(parts: &[&str]) -> String {
        let sep = std::path::MAIN_SEPARATOR;
        if parts.is_empty() {
            return ROOT.to_owned();
        }
        format!("{ROOT}{sep}{}", parts.join(&sep.to_string()))
    }

    fn config_of(text: &str) -> ScopeFile {
        ScopeFile {
            path: format!("{ROOT}{}.p4delta-scope", std::path::MAIN_SEPARATOR),
            config: parse_scope_config(text).expect("配置必须能解析"),
        }
    }

    fn combine(
        text: Option<&str>,
        targets: &[ScopeEntry],
        cli_excludes: &[ScopeEntry],
    ) -> (Vec<ScopeEntry>, ExcludeSet) {
        let file = text.map(config_of);
        let declared = !targets.is_empty();
        combine_scope(ROOT, file.as_ref(), targets, declared, cli_excludes).expect("求值成功")
    }

    fn paths(entries: &[ScopeEntry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.path.as_str()).collect()
    }

    // ---- 命令行目标的解析 ----

    /// 每个 argv 就是一条目标：不按 `;` 拆，也不认 `-` 前缀那套排除 DSL。
    #[test]
    fn each_argument_is_one_target_and_nothing_is_split() {
        let target = parse_target("a;b;c").expect("不是空参数");
        assert_eq!(target.path, "a;b;c");
        assert!(!target.explicit_dir);

        let target = parse_target("-src/deep").expect("不是空参数");
        assert_eq!(
            target.path,
            normalize_local_path("-src/deep").into_owned(),
            "`-` 前缀不再是排除标记"
        );

        // 空白参数等于没给。
        assert!(parse_target("").is_none());
        assert!(parse_target("   ").is_none());
        // 路径里的空格是文件名的一部分，不能被 trim 掉。
        assert_eq!(parse_target(" a.txt ").expect("有内容").path, " a.txt ");
    }

    #[test]
    fn a_wildcard_suffix_marks_an_explicit_directory() {
        let target = parse_target("some-folder/...").expect("有内容");
        assert!(target.explicit_dir);
        assert_eq!(target.path, "some-folder");

        // 没有后缀时按本地类型判，见 `classify_entry`。
        let plain = parse_target("readme.txt").expect("有内容");
        assert!(!plain.explicit_dir);
        assert_eq!(plain.path, "readme.txt");
    }

    #[test]
    fn depot_targets_keep_their_forward_slashes() {
        // Windows 上 `normalize_local_path` 会把 `/` 全折成 `\`，`//depot/...` 于是变成
        // UNC 形状的 `\\depot\...`，后面的 depot 翻译再也认不出它。
        let directory = parse_target("//depot/main/src/...").expect("有内容");
        assert!(directory.explicit_dir);
        assert_eq!(directory.path, "//depot/main/src");

        let file = parse_target("//depot/main/src/lib.txt").expect("有内容");
        assert!(!file.explicit_dir);
        assert_eq!(file.path, "//depot/main/src/lib.txt");
    }

    /// 本地存在的按本地类型判（符号链接算文件，与扫盘口径一致）；本地不存在的看有没有
    /// 显式 `...`——没有就当文件，因为「本地已删除的文件」是合法目标形态。
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

    // ---- 集合代数 ----

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
        let other = entry(&ws_path(&["e.txt"]), EntryKind::File);

        assert!(intersect_entry(&a, &a.clone()).is_some());
        assert!(intersect_entry(&a, &other).is_none());
    }

    /// 大小写是不是同一个文件跟着平台走：Windows / macOS 折，其余平台不折。
    #[test]
    fn file_identity_follows_the_platform_case_policy() {
        let lower = entry(&ws_path(&["d.txt"]), EntryKind::File);
        let upper = entry(&ws_path(&["D.TXT"]), EntryKind::File);

        if crate::path::path_identity_ignores_case() {
            assert!(intersect_entry(&lower, &upper).is_some());
        } else {
            assert!(intersect_entry(&lower, &upper).is_none());
        }
    }

    #[test]
    fn a_file_path_equal_to_a_directory_path_still_intersects() {
        // 本地不存在时类型判定可能与实际不符，同一路径的两种判定要能相交。
        let dir = entry(&ws_path(&["thing"]), EntryKind::Directory);
        let file = entry(&ws_path(&["thing"]), EntryKind::File);

        assert!(intersect_entry(&dir, &file).is_some());
        assert!(intersect_entry(&file, &dir).is_some());
    }

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
            entry(&ws_path(&["d.txt"]), EntryKind::File),
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

    /// 目录自己就带尾分隔符时不再补一个：盘根 `C:\`、尾分隔符 `C:\ws\`、UNC 共享根
    /// `\\server\share\` 都是这种形状，补出来的 `//...` 会被 p4 读成 depot 路径——
    /// 范围从「本地根」悄悄变成「整个 depot」。
    #[test]
    fn a_directory_spec_never_doubles_its_separator() {
        let sep = std::path::MAIN_SEPARATOR;

        assert_eq!(
            entry(&format!("c:{sep}ws{sep}"), EntryKind::Directory).file_spec(),
            format!("c:{sep}ws{sep}...")
        );
        assert_eq!(
            entry(&format!("c:{sep}"), EntryKind::Directory).file_spec(),
            format!("c:{sep}...")
        );

        if cfg!(windows) {
            assert_eq!(
                entry(r"\\server\share\", EntryKind::Directory).file_spec(),
                r"\\server\share\..."
            );
        } else {
            let root = entry("/", EntryKind::Directory).file_spec();
            assert_eq!(root, "/...");
            assert!(!root.starts_with("//"), "`//` 开头是 depot 语法：{root}");
        }
    }

    /// 元字符在 p4 边界上转义**一次**：入口里存的是用户写下的原文，交给 p4 的那份才是
    /// `%XX` 形态。这条管线覆盖全部模式（open / clean / 两种 sync）——一个名叫
    /// `notes#1.txt` 的本地文件不该被读成「`notes` 的第 1 版」，也不该因此被拒。
    #[test]
    fn file_specs_escape_p4_metacharacters_once() {
        let sep = std::path::MAIN_SEPARATOR;
        // 按入口的路径键排序——`dedupe_entries` 出来的就是排好序的，这里照着产线的形状摆。
        let scope = Scope {
            includes: vec![
                entry(&format!("c:{sep}ws{sep}a@b%c*d?e"), EntryKind::Directory),
                entry(&format!("c:{sep}ws{sep}notes#1.txt"), EntryKind::File),
            ],
            excludes: ExcludeSet::default(),
            work_dir: format!("c:{sep}ws"),
        };

        assert_eq!(
            scope.query_specs(None),
            [
                format!("c:{sep}ws{sep}a%40b%25c%2Ad%3Fe{sep}..."),
                format!("c:{sep}ws{sep}notes%231.txt"),
            ]
        );
        // 目标 CL 钉在每一条规格上，且钉在转义之后。
        assert_eq!(
            scope.query_specs(Some(12)),
            [
                format!("c:{sep}ws{sep}a%40b%25c%2Ad%3Fe{sep}...@12"),
                format!("c:{sep}ws{sep}notes%231.txt@12"),
            ]
        );
    }

    /// 已经带 `%XX` 的名字是**原文**，不是「已转义」：`%23` 会被再转一次成 `%2523`。
    /// 这正是「本地原始路径与 file spec 是两种东西」的判据——反过来的话，一次转义在
    /// 两个地方各做一遍，用户点名的文件就再也对不上了。
    #[test]
    fn an_already_escaped_name_is_a_literal_name() {
        let entry = entry("C:\\ws\\notes%231.txt", EntryKind::File);
        assert_eq!(entry.file_spec(), "C:\\ws\\notes%25231.txt");
    }

    // ---- 配置 → 范围 ----

    /// 只写排除项：include 默认是 client root，于是就是「整个 root 减去排除项」。
    #[test]
    fn a_config_without_include_covers_the_whole_root() {
        let (includes, excludes) = combine(Some(r#"{"exclude": [{"dir": "gen"}]}"#), &[], &[]);

        assert_eq!(paths(&includes), [ws_path(&[])]);
        assert!(excludes.excludes_key(&local_path_key(&ws_path(&["gen", "a.txt"]))));
        assert!(!scope_covers(
            &includes,
            &excludes,
            &ws_path(&["gen", "a.txt"])
        ));
        assert!(scope_covers(
            &includes,
            &excludes,
            &ws_path(&["src", "a.txt"])
        ));
    }

    /// `include: []` 是明确的空集：给了目标也不放大，而是在执行档报错。
    #[test]
    fn an_explicitly_empty_include_stays_empty() {
        let file = config_of(r#"{"include": []}"#);
        let targets = vec![entry(&ws_path(&[]), EntryKind::Directory)];

        let error = combine_scope(ROOT, Some(&file), &targets, true, &[])
            .expect_err("空集必须报错")
            .to_string();
        assert!(error.contains("do not overlap"), "{error}");
        assert!(error.contains("include <empty>"), "{error}");
    }

    /// 配置是硬上限：传入 root 时，根上的东西仍被配置挡在外面。
    #[test]
    fn the_config_caps_the_targets() {
        let (includes, _) = combine(
            Some(r#"{"include": [{"dir": "src"}]}"#),
            &[entry(&ws_path(&[]), EntryKind::Directory)],
            &[],
        );

        assert_eq!(paths(&includes), [ws_path(&["src"])]);
    }

    /// 目标收窄配置：配置说整个 root，目标只点名 src 时结果就是 src。
    #[test]
    fn targets_narrow_a_config_that_covers_everything() {
        let (includes, _) = combine(
            Some("{}"),
            &[entry(&ws_path(&["src"]), EntryKind::Directory)],
            &[],
        );

        assert_eq!(paths(&includes), [ws_path(&["src"])]);
    }

    /// 配置整个缺席时范围只由目标决定；两边都没有才是「没给路径」，绝不默认整个 root。
    #[test]
    fn no_config_and_no_targets_is_an_error_not_the_whole_root() {
        let error = combine_scope(ROOT, None, &[], false, &[])
            .expect_err("必须报错")
            .to_string();
        assert!(error.contains("No path given"), "{error}");

        let (includes, _) = combine(
            None,
            &[entry(&ws_path(&["src"]), EntryKind::Directory)],
            &[],
        );
        assert_eq!(paths(&includes), [ws_path(&["src"])]);
    }

    /// 目标一条都没能定位（比如 depot 路径不在 client view 里）时不能退回配置范围：
    /// 那等于把操作放大到用户没点过的东西上。
    #[test]
    fn declared_but_unlocatable_targets_are_an_error() {
        let error = combine_scope(ROOT, None, &[], true, &[])
            .expect_err("必须报错")
            .to_string();
        assert!(error.contains("none of the given paths"), "{error}");
    }

    /// 目标整个落在 client root 之外（这里是它的兄弟目录）时是**报错**：归属是硬上限，
    /// 「参数说到哪就管到哪」会让一次 `--clean -a` 作用在另一个 client 的文件上。
    #[test]
    fn targets_outside_the_client_root_are_an_error() {
        let outside = if cfg!(windows) {
            "C:\\elsewhere"
        } else {
            "/elsewhere"
        };
        let file = config_of("{}");
        let targets = vec![entry(outside, EntryKind::Directory)];

        let error = combine_scope(ROOT, Some(&file), &targets, true, &[])
            .expect_err("整个落在 root 之外的目标必须报错")
            .to_string();

        assert!(error.contains("do not overlap"), "{error}");
    }

    /// 目标是 root 的父目录时不算越界：它包含 root，交出来就是 root 本身。
    #[test]
    fn a_parent_directory_target_clamps_to_the_client_root() {
        let parent = if cfg!(windows) { "C:\\" } else { "/" };

        let (includes, _) = combine(Some("{}"), &[entry(parent, EntryKind::Directory)], &[]);

        assert_eq!(paths(&includes), [ws_path(&[])]);
    }

    /// `--exclude-*` 与配置里的排除取并集，且无条件优先；相对路径以 client root 为基准。
    #[test]
    fn cli_exclusions_union_with_the_configured_ones() {
        let (_, excludes) = combine(
            Some(r#"{"exclude": [{"dir": "gen"}]}"#),
            &[entry(&ws_path(&[]), EntryKind::Directory)],
            &[
                entry(&ws_path(&["scratch"]), EntryKind::Directory),
                entry(&ws_path(&["local.txt"]), EntryKind::File),
            ],
        );

        assert_eq!(excludes.declared_len(), 3);
        assert!(excludes.excludes_key(&local_path_key(&ws_path(&["gen", "a.txt"]))));
        assert!(excludes.excludes_key(&local_path_key(&ws_path(&["scratch", "b.txt"]))));
        assert!(excludes.excludes_key(&local_path_key(&ws_path(&["local.txt"]))));
        // 文件排除只挡自己，不挡后代。
        assert!(!excludes.excludes_key(&local_path_key(&ws_path(&["local.txt.bak"]))));
    }

    /// 排除项把入口自己排掉时是**报错**而不是静默成功，且报错里要列出排除项——
    /// 不列的话这条消息看着自相矛盾（`scope: src` 配 `given: src/deep` 明明相交）。
    #[test]
    fn exclusions_that_swallow_the_target_report_themselves() {
        let file = config_of(r#"{"exclude": [{"dir": "src/deep"}]}"#);
        let targets = vec![entry(&ws_path(&["src", "deep"]), EntryKind::Directory)];

        let error = combine_scope(ROOT, Some(&file), &targets, true, &[])
            .expect_err("入口被排除项排掉时必须报错")
            .to_string();

        assert!(error.contains("do not overlap"), "{error}");
        assert!(error.contains("excluded: dir \""), "{error}");
        assert!(error.contains("src"), "{error}");
        assert!(error.contains("deep"), "{error}");
    }

    #[test]
    fn the_scope_file_excludes_itself() {
        let (includes, excludes) = combine(Some(r#"{"include": [{"dir": "."}]}"#), &[], &[]);

        assert_eq!(paths(&includes), [ws_path(&[])]);
        let key = local_path_key(&format!(
            "{ROOT}{}.p4delta-scope",
            std::path::MAIN_SEPARATOR
        ));
        assert!(excludes.excludes_key(&key), "配置文件自身必须两侧都排掉");
        assert_eq!(excludes.declared_len(), 0, "隐式排除不进面向用户的计数");
    }

    // ---- 配置项解析成绝对入口 ----

    #[test]
    fn config_entries_resolve_against_the_client_root() {
        let config = parse_scope_config(
            r#"{"include": [{"dir": "."}, {"dir": "a/b"}, {"file": "readme.txt"}]}"#,
        )
        .unwrap();

        let resolved: Vec<String> = config
            .include
            .as_ref()
            .unwrap()
            .iter()
            .map(|entry| resolve_config_entry(entry, ROOT).path)
            .collect();

        assert_eq!(
            resolved,
            [ws_path(&[]), ws_path(&["a", "b"]), ws_path(&["readme.txt"])]
        );
    }

    // ---- 共享契约向量 ----

    /// `tests/fixtures/scope-contract.json` 是 Rust 与 TypeScript **共用**的一批向量：
    /// 两侧各自求值、各自断言，而不是只比「解析没报错」。范围这套东西最容易在两处各理解
    /// 一寸（大小写、组件边界、`include: []` 与缺席的差别），向量把那些理解钉在一起。
    mod contract {
        use super::*;

        use clap::Parser;
        use serde_json::Value;

        use crate::test_util::TempTree;

        const CONTRACT: &str = include_str!("../tests/fixtures/scope-contract.json");

        fn platform() -> &'static str {
            if cfg!(windows) { "windows" } else { "unix" }
        }

        fn root() -> &'static str {
            if cfg!(windows) { "C:\\ws" } else { "/ws" }
        }

        /// `{root}` 展开成这台机器上的形状。配置正文里不出现它：配置条目本来就是 client
        /// root 相对的。
        fn expand(text: &str) -> String {
            text.replace("{root}", root())
        }

        /// client root 相对 POSIX 路径 → 绝对本地路径，走的是产品代码同一条变换，
        /// 否则断言的就不是产线那一套拼法。
        fn absolute(relative: &str) -> String {
            if relative == "." {
                return canonical_local_path(root());
            }
            let relative = relative.replace('/', std::path::MAIN_SEPARATOR_STR);
            canonical_local_path(&Path::new(root()).join(relative).display().to_string())
        }

        /// 反向：入口的绝对路径 → 向量里的 client root 相对形状（`"."` 表示 root 自身）。
        fn relative(path: &str) -> String {
            let Some(rest) = path.strip_prefix(root()) else {
                panic!("\"{path}\" 不在 client root 之下");
            };
            if rest.is_empty() {
                return ".".to_owned();
            }
            rest.trim_start_matches(std::path::MAIN_SEPARATOR)
                .replace(std::path::MAIN_SEPARATOR, "/")
        }

        fn kind(name: &str) -> EntryKind {
            match name {
                "directory" => EntryKind::Directory,
                "file" => EntryKind::File,
                other => panic!("未知的类型 \"{other}\""),
            }
        }

        fn entries(case: &Value, field: &str) -> Vec<ScopeEntry> {
            case.get(field)
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|item| {
                            let path = item["path"].as_str().expect("每条都要有 path");
                            let kind = kind(item["kind"].as_str().expect("每条都要有 kind"));
                            ScopeEntry::new(absolute(&expand(path)), kind)
                        })
                        .collect()
                })
                .unwrap_or_default()
        }

        /// `--exclude-*` 走真实的解析：「相对路径以 client root 为基准」正是编辑器要照着
        /// 做的一环，绕过去就只剩一句注释了。
        fn cli_excludes(case: &Value) -> Vec<ScopeEntry> {
            let mut args = vec!["p4delta".to_owned()];

            for item in case
                .get("cliExcludes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let flag = match kind(item["kind"].as_str().expect("每条都要有 kind")) {
                    EntryKind::Directory => "--exclude-dir",
                    EntryKind::File => "--exclude-file",
                };
                args.push(flag.to_owned());
                args.push(expand(item["path"].as_str().expect("每条都要有 path")));
            }

            resolve_cli_excludes(&Options::parse_from(args), root())
        }

        struct Evaluated {
            includes: Vec<ScopeEntry>,
            excludes: ExcludeSet,
        }

        fn evaluate(case: &Value) -> std::result::Result<Evaluated, String> {
            let config = match case.get("config") {
                None | Some(Value::Null) => None,
                Some(Value::String(text)) => {
                    Some(parse_scope_config(text).map_err(|error| error.to_string())?)
                }
                Some(other) => return Err(format!("config 得是字符串或 null，不是 {other}")),
            };

            let scope_file = config.map(|config| ScopeFile {
                path: absolute(".p4delta-scope"),
                config,
            });

            let targets = entries(case, "targets");
            let declared = !targets.is_empty();
            let cli_excludes = cli_excludes(case);

            let (includes, excludes) = combine_scope(
                root(),
                scope_file.as_ref(),
                &targets,
                declared,
                &cli_excludes,
            )
            .map_err(|error| error.to_string())?;

            Ok(Evaluated { includes, excludes })
        }

        /// 断言里的路径都按平台路径键排序，向量的书写顺序与实现的插入顺序就解耦了。
        fn sorted(entries: &[ScopeEntry]) -> Vec<String> {
            let mut pairs: Vec<(&str, String)> = entries
                .iter()
                .map(|entry| (entry.path_lower.as_str(), relative(&entry.path)))
                .collect();
            pairs.sort();
            pairs.into_iter().map(|(_, path)| path).collect()
        }

        fn listed(case: &Value, field: &str) -> Vec<String> {
            case["expect"][field]
                .as_array()
                .unwrap_or_else(|| panic!("expect 里要有 {field} 数组"))
                .iter()
                .map(|value| value.as_str().expect("路径是字符串").to_owned())
                .collect()
        }

        fn check(case: &Value) {
            let name = case["name"].as_str().expect("每条用例都要有名字");
            let expected_error = case["expect"]["error"].as_str();

            match (evaluate(case), expected_error) {
                (Err(message), Some(fragment)) => assert!(
                    message.contains(fragment),
                    "{name}: 期望错误里有 {fragment:?}，实际是 {message:?}"
                ),
                (Err(message), None) => panic!("{name}: 不该报错，实际是 {message:?}"),
                (Ok(_), Some(fragment)) => {
                    panic!("{name}: 期望错误 {fragment:?}，实际求值成功了")
                }
                (Ok(evaluated), None) => {
                    assert_eq!(
                        sorted(&evaluated.includes),
                        listed(case, "include"),
                        "{name}: include"
                    );
                    assert_eq!(
                        sorted(evaluated.excludes.entries()),
                        listed(case, "exclude"),
                        "{name}: exclude（只列声明的，不含隐式的配置文件自身）"
                    );

                    let members = case["expect"]["members"]
                        .as_object()
                        .unwrap_or_else(|| panic!("{name}: expect 里要有 members"));
                    for (path, expected) in members {
                        let expected = expected.as_bool().expect("members 的值是布尔");
                        let actual = scope_covers(
                            &evaluated.includes,
                            &evaluated.excludes,
                            &absolute(&expand(path)),
                        );
                        assert_eq!(actual, expected, "{name}: 成员关系 \"{path}\"");
                    }
                }
            }
        }

        #[test]
        fn the_shared_vectors_agree_with_this_implementation() {
            let document: Value = serde_json::from_str(CONTRACT).expect("契约向量必须是合法 JSON");
            let cases = document["cases"].as_array().expect("cases 必须是数组");

            let mut executed = 0;
            for case in cases {
                let case_platform = case["platform"].as_str().expect("每条用例都要有 platform");
                if case_platform != "any" && case_platform != platform() {
                    continue;
                }
                executed += 1;
                check(case);
            }

            assert!(executed * 2 >= cases.len(), "平台过滤吃掉了过半的向量");
            assert!(executed >= 20, "实际只跑了 {executed} 条向量");
        }

        /// 配置文件**字节**层面的向量：同一批 hex 两侧各自落盘、各自用本仓的读取入口读。
        ///
        /// 判的是「合法 UTF-8」与「坏了」的分界：EF BF BD 是合法的 U+FFFD 字符，必须当普通
        /// 文件名收下；孤立 0xFF 与截断的多字节序列才是错。按「正文里出现过替换字符」判坏会
        /// 把前一种错杀，两侧都得能过这一条。
        #[test]
        fn the_shared_file_byte_vectors_agree_with_this_implementation() {
            let document: Value = serde_json::from_str(CONTRACT).expect("契约向量必须是合法 JSON");
            let cases = document["fileBytes"]["cases"]
                .as_array()
                .expect("fileBytes.cases 必须是数组");
            assert!(cases.len() >= 5, "字节向量不该少于 5 条");

            for case in cases {
                let name = case["name"].as_str().expect("每条用例都要有名字");
                let bytes = hex::decode(case["hex"].as_str().expect("每条都要有 hex"))
                    .expect("向量里的 hex 必须合法");
                let tree = TempTree::new(&format!("scope-bytes-{name}"));
                fs::write(tree.root.join(SCOPE_FILE_NAME), &bytes).expect("写入配置字节");

                let expected_error = case["expect"]["error"].as_str();
                match (
                    read_scope_file(&tree.root.display().to_string(), false),
                    expected_error,
                ) {
                    (Err(error), Some(fragment)) => {
                        let message = format!("{error:#}");
                        assert!(
                            message.contains(fragment),
                            "{name}: 期望错误里有 {fragment:?}，实际是 {message:?}"
                        );
                    }
                    (Err(error), None) => panic!("{name}: 不该报错，实际是 {error:?}"),
                    (Ok(_), Some(fragment)) => {
                        panic!("{name}: 期望错误 {fragment:?}，实际读成功了")
                    }
                    (Ok(None), None) => panic!("{name}: 配置在那儿，不该读成「没有配置」"),
                    (Ok(Some(file)), None) => {
                        let includes: Vec<String> = file
                            .config
                            .include
                            .as_ref()
                            .expect("向量都写了 include")
                            .iter()
                            .map(|entry| entry.path.clone())
                            .collect();
                        assert_eq!(includes, listed(case, "include"), "{name}: include");
                        let excludes: Vec<String> = file
                            .config
                            .exclude
                            .iter()
                            .map(|entry| entry.path.clone())
                            .collect();
                        assert_eq!(excludes, listed(case, "exclude"), "{name}: exclude");
                    }
                }
            }
        }
    }
}
