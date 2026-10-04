//! `--json` 模式的机器可读输出。
//!
//! 契约见 `docs/json-contract.md`：打开 `--json` 后 **stdout 只出 JSON Lines**，人类可读的
//! 报告整体改道 stderr。这个模块就管这两件事——[`sayln!`] 负责改道，`emit_*` 负责往 stdout
//! 写记录。
//!
//! 状态是进程级的（模式开关、clientspec 的根与客户端名、summary 账目）。p4delta 是一次性
//! 进程，一轮只跑一个模式，用全局状态换掉一路透传的 `&mut` 上下文是划算的：这些值要在
//! 报告层最深处（`report_group`）、错误路径（`run` 的返回处）两处被读到，透传会把它们捅穿
//! 大半个调用图。

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde_json::json;

use crate::cli::Options;
use crate::p4::process::{FailureMode, run_p4_command_slice};
use crate::path::normalize_local_path;

/// 三模式。`class` 枚举必须配 `mode` 读（同一个 `delete` 在 open 与 clean 下含义相反）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Open,
    Clean,
    Sync,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Open => "open",
            Mode::Clean => "clean",
            Mode::Sync => "sync",
        }
    }
}

/// `class` → `action` 的唯一对照表，见契约文档。`mode` + `class` 定死一个动作。
///
/// 这张表就是「消费方不用猜」的全部依据：`action` 不是另一个人写一遍的东西，而是从这里
/// 派生。改 `class` 的名字必须同改这里，`every_class_maps_to_an_action` 钉住覆盖率。
///
/// `class:"handoff"` 刻意不在表里：那类记录没有逐文件动作，`action` 就是 `handoff` 字段
/// 点名的那条命令（见 [`emit_handoff_files`]）。表里给它塞一个占位值，等于让同一件事在
/// 线协议上有两种说法。
fn action_for(mode: Mode, class: &str) -> &'static str {
    match (mode, class) {
        (Mode::Open, "add") => "add",
        (Mode::Open, "edit") => "edit",
        (Mode::Open, "delete") => "delete",
        // 改开：磁盘状态说了算，动作分别是 edit / delete。
        (Mode::Open, "reopen_edit") => "edit",
        (Mode::Open, "reopen_delete") => "delete",
        // 撤销打开：文件离开 changelist，内容不动。
        (Mode::Open, "revert_add" | "revert_edit" | "revert_delete") => "revert",
        (Mode::Clean, "delete") => "deleting",
        (Mode::Clean, "revert") => "reverting",
        (Mode::Clean, "restore") => "restoring",
        (Mode::Sync, "update") => "updating",
        (Mode::Sync, "revert") => "reverting",
        (Mode::Sync, "restore") => "restoring",
        (Mode::Sync, "delete") => "deleting",
        _ => "unknown",
    }
}

static JSON_MODE: AtomicBool = AtomicBool::new(false);

/// 拼 client 语法要用的两样东西。`root` 是 clientspec 的根，`client` 是客户端名。
#[derive(Debug)]
struct ClientSpec {
    root: String,
    client: String,
}

static CLIENT_SPEC: Mutex<Option<ClientSpec>> = Mutex::new(None);

/// 本轮的账目：`counts` 按 `class` 累加，入口匹配数由 unmatched 侧反过来算。
#[derive(Debug, Default)]
struct Ledger {
    counts: Vec<(&'static str, usize)>,
    unmatched: usize,
    /// 发过 `ok:false` 的失败原因；`None` 表示还没失败。
    reason: Option<&'static str>,
}

static LEDGER: Mutex<Ledger> = Mutex::new(Ledger {
    counts: Vec::new(),
    unmatched: 0,
    reason: None,
});

/// 进度阶段的固定序列。`step` 是它在表里的下标 + 1，单调不回跳。
///
/// 只收「顺序主干」上的阶段：`depot` / `scan` / `have` 三路是并发跑的（`futures::join!`），
/// 从并发分支里发进度会让 `step` 回跳，而回跳的进度条比没有进度条更难读。
const PHASES: [&str; 5] = ["start", "analyze", "digest", "report", "done"];

pub(crate) fn set_json_mode(enabled: bool) {
    JSON_MODE.store(enabled, Ordering::Relaxed);
}

pub(crate) fn is_json() -> bool {
    JSON_MODE.load(Ordering::Relaxed)
}

// ---- 人类可读输出改道 ----

/// 一行人类可读输出：文本模式走 stdout，`--json` 下改走 stderr。
pub(crate) fn say_line(line: &str) {
    if is_json() {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

/// `println!` 的改道版。报告层的每一行都该用它，别直接 `println!`——那是往 JSON 流里
/// 掺人类文字，消费方解析当场失败。
macro_rules! sayln {
    () => { $crate::json::say_line("") };
    ($($arg:tt)*) => { $crate::json::say_line(&format!($($arg)*)) };
}

pub(crate) use sayln;

// ---- client 语法 ----

/// 定下拼 client 语法要用的根与客户端名。
///
/// `--client-root` 给了就用它（编辑器正是靠这个省掉一次 `p4 info` 往返）；没给就问一次
/// `p4 info`，取它报的 `clientRoot`。两者都拿不到时留 `None`，[`client_syntax`] 退化成
/// 原样返回本地路径——那种输出消费方仍能用（它本来就把非 `//` 开头的值当本地路径），
/// 但会丢掉「两条引擎给出同一种拼法」这个好处，所以留一行 warning 在 stderr 上。
pub(crate) async fn resolve_client_spec(options: &Options, work_dir: &str, client: &str) {
    if !is_json() {
        return;
    }

    if let Some(root) = &options.client_root {
        *CLIENT_SPEC.lock().unwrap() = Some(ClientSpec {
            root: root.clone(),
            client: client.to_owned(),
        });
        return;
    }

    // `info` 不吃文件参数，走 slice 版：batched 版按文件参数切片，零参数等于一次都不跑。
    let args: [&str; 3] = ["-Mj", "-Ztag", "info"];
    let root =
        match run_p4_command_slice(options, work_dir, &args, &[], false, FailureMode::Warn).await {
            Ok(lines) => lines.iter().find_map(|line| {
                let record: serde_json::Value = serde_json::from_str(line).ok()?;
                record["clientRoot"].as_str().map(str::to_owned)
            }),
            Err(error) => {
                eprintln!("Warning: failed to read the client root from p4 info: {error}");
                None
            }
        };

    match root {
        Some(root) => {
            *CLIENT_SPEC.lock().unwrap() = Some(ClientSpec {
                root,
                client: client.to_owned(),
            });
        }
        None => eprintln!(
            "Warning: no client root available, clientFile will be the local path \
             (pass --client-root to avoid this)."
        ),
    }
}

/// 本地路径 → client 语法（`//<client>/<相对路径>`）。
///
/// 拼不出来（没有根、路径不在根下）时**原样返回本地路径**：契约允许这种退化，消费方把
/// 不以 `//` 开头的值当本地路径用。绝不返回空串或半截路径。
///
/// 退化不是无声的：整根拿不到时 [`resolve_client_spec`] 留过一行 warning，路径不在根下时
/// 这里补一行（见 [`warn_degraded_client_file`]）。真实触发面不只是「用户写错根」——编辑器
/// 自己算 root，盘符大小写不一致、junction/symlink 形式的根、client view 把文件映射到根
/// 之外，都会让一部分记录悄悄降级成另一种拼法。
pub(crate) fn client_syntax(local: &str) -> String {
    let spec = CLIENT_SPEC.lock().unwrap();
    let Some(spec) = spec.as_ref() else {
        // 根整个拿不到：`resolve_client_spec` 已经为这一档留过 warning，这里只记账。
        DEGRADED_CLIENT_FILES.fetch_add(1, Ordering::Relaxed);
        return local.to_owned();
    };

    match to_client_syntax(local, &spec.root, &spec.client) {
        Some(client_file) => client_file,
        None => {
            warn_degraded_client_file(local, &spec.root);
            local.to_owned()
        }
    }
}

/// 路径不在根下时的退化告警：**只点名第一条**，其余靠整轮汇总。
///
/// 一个工作区上万个文件时逐条打就是上万行 stderr，而这条信息并不需要逐条重复——看一条
/// 就知道根对不上，剩下的只是条数（[`report_degraded_client_files`] 负责报）。
fn warn_degraded_client_file(local: &str, root: &str) {
    if DEGRADED_CLIENT_FILES.fetch_add(1, Ordering::Relaxed) == 0 {
        eprintln!(
            "Warning: \"{local}\" is not under the client root \"{root}\", its clientFile will \
             be the local path. Pass --client-root with the client's Root to fix this; further \
             records are only summarized at the end."
        );
    }
}

/// 退化成「本地路径」的 clientFile 条数。
///
/// 逐条告警会刷屏：一个工作区上万个文件就是上万行 stderr。[`client_syntax`] 只点名第一条，
/// 整轮结束时 [`report_degraded_client_files`] 汇总一句——消费方既不会被淹，也不会对一份
/// 「client 语法与本地路径混排」的记录流毫无察觉。
static DEGRADED_CLIENT_FILES: AtomicUsize = AtomicUsize::new(0);

/// 整轮结束时汇总退化的 clientFile，见 [`DEGRADED_CLIENT_FILES`]。
///
/// 只有一条时不再多说：那一条已经逐条点名过了（整根拿不到时是 `resolve_client_spec` 那句）。
pub(crate) fn report_degraded_client_files() {
    if !is_json() {
        return;
    }

    let degraded = DEGRADED_CLIENT_FILES.load(Ordering::Relaxed);
    if degraded > 1 {
        eprintln!(
            "Warning: {degraded} record(s) had clientFile fall back to the local path \
             (the first one is named above)."
        );
    }
}

/// 相对化的**匹配**走大小写/分隔符归一后的键（大小写折叠只在 Windows 上做，见
/// [`comparison_key`]），但返回的路径按**输入的原文**切片——大小写敏感的服务端上，
/// `//depot/A.txt` 与 `//depot/a.txt` 是两个文件，拼 client 语法时不能把用户路径的大小写
/// 折掉。归一与切片能共用同一个下标，是因为归一化是逐字节等长的（`/`→`\` 一对一，ASCII
/// 小写一对一）。
fn to_client_syntax(local: &str, root: &str, client: &str) -> Option<String> {
    let local_normalized = normalize_local_path(local);
    let root_normalized = normalize_local_path(root.trim_end_matches(['/', '\\']));

    let root_key = comparison_key(&root_normalized);
    if root_key.is_empty() {
        return None;
    }

    let local_key = comparison_key(&local_normalized);
    let rest = local_key.strip_prefix(&root_key)?;
    // 组件边界：`C:\ws2` 不在 `C:\ws` 底下。
    if !rest.starts_with(std::path::MAIN_SEPARATOR) {
        return None;
    }

    let relative = local_normalized[root_key.len()..].trim_start_matches(std::path::MAIN_SEPARATOR);
    // 根自己不是文件：拼出来会是 `//cli/`（一个目录规格），不如退回本地路径。
    if relative.is_empty() {
        return None;
    }

    Some(format!(
        "//{client}/{}",
        relative.replace(std::path::MAIN_SEPARATOR, "/")
    ))
}

/// 前缀匹配用的键。**大小写折叠只在 Windows 上做**：那里的文件系统不区分大小写，盘符与
/// 路径段怎么拼都指同一个目录，client view 的映射也跟着它走（p4 回路径时的大小写未必与
/// 用户给的一致）。
///
/// 其余平台大小写敏感：Linux 上 `/WS/a.txt` 与根 `/ws` 是两个目录，折掉大小写会把一个
/// **不在根下**的路径拼成 `//cli/a.txt`——一个根本不存在的 client 路径，比退化成
/// 本地路径更糟。返回值的拼写始终按输入原文，折的只是判据。
fn comparison_key(path: &str) -> String {
    if cfg!(windows) {
        path.to_ascii_lowercase()
    } else {
        path.to_owned()
    }
}

// ---- 记录 ----

/// 一条文件记录。路径字段已经是消费方要的形态（`client_file` 传本地路径即可，这里会翻）。
pub(crate) struct FileRecord<'a> {
    pub(crate) mode: Mode,
    pub(crate) class: &'a str,
    pub(crate) depot_file: &'a str,
    /// 本地路径，写记录时翻成 client 语法。
    pub(crate) client_file: &'a str,
    /// have / 目标版本号。新增文件没有这一项（原生 `-Mj -Ztag reconcile` 那边叫 `workRev`，
    /// 且 add 也带——但那是「将要成为的版本」而不是 have，翻译时按 class 摘掉）。
    pub(crate) rev: Option<u32>,
    pub(crate) applied: bool,
}

pub(crate) fn emit_file(record: &FileRecord<'_>) {
    if !is_json() {
        return;
    }

    let mut value = json!({
        "kind": "file",
        "mode": record.mode.as_str(),
        "class": record.class,
        "action": action_for(record.mode, record.class),
        "depotFile": record.depot_file,
        "clientFile": client_syntax(record.client_file),
        "applied": record.applied,
    });

    let object = value.as_object_mut().expect("json! 造出来的一定是对象");
    if let Some(rev) = record.rev {
        object.insert("rev".to_owned(), json!(rev.to_string()));
    }

    emit(&value);
}

/// 一批转交给原生 p4 的文件：depot 路径用来点名，本地路径翻成 client 语法进记录。
///
/// 两个路径都留着：`depotFile` 是消费方与原生输出对齐用的，`clientFile` 是它拿去和自己
/// 的文档/路径比对的——缺了后者的记录在消费方那边只能走特判。
#[derive(Clone)]
pub(crate) struct HandoffFile {
    pub(crate) depot_file: String,
    /// 本地路径，写记录时翻成 client 语法。
    pub(crate) client_file: String,
}

/// 只能整批报「交给谁来处理」的那几条转交路径。三类调用方：clean 的预演与应用、
/// open 的应用、sync 的预演与应用。
///
/// `action` 就是 `handoff` 字段点名的那条命令：这类记录没有逐文件动作，`class` 也统一是
/// `handoff`——消费方读到的信息是「这批我处理不了，交给原生 p4 了」，而不是「这批不存在」。
pub(crate) fn emit_handoff_files(
    mode: Mode,
    handoff: &str,
    files: impl IntoIterator<Item = HandoffFile>,
    applied: bool,
) {
    if !is_json() {
        return;
    }

    for file in files {
        emit(&json!({
            "kind": "file",
            "mode": mode.as_str(),
            "class": "handoff",
            "action": handoff,
            "handoff": handoff,
            "depotFile": file.depot_file,
            "clientFile": client_syntax(&file.client_file),
            "applied": applied,
        }));
    }
}

/// 把原生 `p4 reconcile -n -Mj -Ztag` 的记录逐条翻译成本工具的文件记录。
///
/// 转交出去的那批（δ 算不出摘要的 Apple / Resource 之类）在预演时必须照实报出来：
/// 预演的全部意义就是预告真实动作，把转交的那批藏起来，预告出来的清单就比实际要小。
/// 因此这条路径只用于 open 的预演，报出来的是正常 file 记录（带逐文件动作），不是
/// `class:"handoff"`——后者说不清这一批到底要被做成什么。
///
/// 两个字段要就着原生记录的形状翻：
///
/// - `clientFile` **不能直接透传**：原生 reconcile 回的是**本地路径**（同族的 `opened` /
///   `where` 回 client 语法，只有它不一样，2024.1 实测），要按契约翻成 client 语法；
/// - `rev` 对应的是原生的 `workRev`（2024.1 没有 `rev` 字段），见 [`FileRecord::rev`]。
///
/// `class` 收敛回本工具的枚举（add / edit / delete）：p4 那边还有 `move/add`、`branch`、
/// `integrate` 这些动作，落到磁盘上都是「这份内容要被开出来」，一律按 `edit` 报。判据同
/// `analyze` 认不出动作时的立场——宁可报得粗，不可丢行。
pub(crate) fn emit_native_reconcile_records(mode: Mode, lines: &[String], applied: bool) {
    if !is_json() {
        return;
    }

    for line in lines {
        let Some(file) = translate_native_file(line) else {
            continue;
        };

        count(file.class, 1);
        emit_file(&FileRecord {
            mode,
            class: file.class,
            depot_file: &file.depot_file,
            client_file: &file.client_file,
            rev: file.rev,
            applied,
        });
    }
}

/// 一条原生 reconcile 记录翻译出来的字段。
struct NativeFile {
    /// 收敛回本工具枚举的 `class`。
    class: &'static str,

    depot_file: String,

    /// 本地路径，交给 [`emit_file`] 翻成 client 语法。
    client_file: String,

    /// 原生的 `workRev`。新增文件那一档是空的，见 [`FileRecord::rev`]。
    rev: Option<u32>,
}

/// 翻译一条原生 `-Mj -Ztag reconcile` 记录，见 [`emit_native_reconcile_records`]。
///
/// 两个路径字段缺一不可：缺了就没法把这条记录拼成消费方能用的形态。错误行（`severity`、
/// `generic` 那类）本来就没有这两个字段，将来的 p4 版本换形状也是同样的表现——宁可少报
/// 一行，也不发一条消费方读不懂的记录。
fn translate_native_file(line: &str) -> Option<NativeFile> {
    let record: serde_json::Value = serde_json::from_str(line).ok()?;

    let depot_file = record["depotFile"].as_str()?.to_owned();
    let client_file = record["clientFile"].as_str()?.to_owned();

    let class = match record["action"].as_str() {
        Some("add") => "add",
        Some("delete") => "delete",
        _ => "edit",
    };

    Some(NativeFile {
        class,
        depot_file,
        client_file,
        rev: (class != "add")
            .then(|| record["workRev"].as_str())
            .flatten()
            .and_then(|rev| rev.parse().ok()),
    })
}

/// 一个入口在 depot 与磁盘上都没匹配到东西。
pub(crate) fn emit_unmatched(path: &str) {
    LEDGER.lock().unwrap().unmatched += 1;
    if !is_json() {
        return;
    }
    emit(&json!({"kind": "unmatched", "path": path}));
}

/// 整轮失败的那一条错误记录（`run` 拿到 `Err` 时发），`clientFile` 可选。
///
/// **逐文件的失败不走这里**：`p4 add` 拒收某个名字、protections 拒绝 open 这类失败在
/// 下发命令的那一层就被拼成一条错误消息，由 summary 的 `ok:false` 与 stderr 上的人类
/// 可读文本承担（契约的 error 一节写的就是这个分工）。消费方目前不读 error 记录，
/// 多造一种逐文件的错误记录只会多一份没人对账的线协议。
pub(crate) fn emit_error(client_file: Option<&str>, message: &str) {
    if !is_json() {
        return;
    }

    let mut value = json!({"kind": "error", "message": message});
    if let Some(client_file) = client_file {
        value
            .as_object_mut()
            .expect("json! 造出来的一定是对象")
            .insert("clientFile".to_owned(), json!(client_file));
    }
    emit(&value);
}

/// 记一类变更的数量与文件数。`counts` 里只出现非零项，缺席即 0。
pub(crate) fn count(class: &'static str, files: usize) {
    let mut ledger = LEDGER.lock().unwrap();
    match ledger.counts.iter_mut().find(|(name, _)| *name == class) {
        Some((_, count)) => *count += files,
        None => ledger.counts.push((class, files)),
    }
}

pub(crate) fn set_reason(reason: &'static str) {
    LEDGER.lock().unwrap().reason = Some(reason);
}

/// 进度提示。只走 stderr，且**只许被当成提示**——结论只认 summary。
pub(crate) fn emit_progress(phase: &str, message: Option<&str>) {
    if !is_json() {
        return;
    }

    let step = PHASES
        .iter()
        .position(|known| *known == phase)
        .map(|index| index + 1)
        .unwrap_or(0);

    emit_to(
        &mut std::io::stderr(),
        &json!({
            "kind": "progress",
            "phase": phase,
            "step": step,
            "total": PHASES.len(),
            "message": message,
        }),
    );
}

/// 一条 summary。**任何**经 `run()` 返回的路径都要发——没有 summary 就等于没有结论。
pub(crate) fn emit_summary(mode: Mode, ok: bool, applied: bool, elapsed_ms: u128) {
    if !is_json() {
        return;
    }

    let ledger = LEDGER.lock().unwrap();
    let total: usize = ledger.counts.iter().map(|(_, count)| count).sum();
    let counts: serde_json::Map<String, serde_json::Value> = ledger
        .counts
        .iter()
        .filter(|(_, count)| *count > 0)
        .map(|(class, count)| ((*class).to_owned(), json!(count)))
        .collect();

    // 失败原因的三档：成功是 null，入口全落空是专门的一档（消费方要按它区分「这里确实
    // 什么都没有」与「跑挂了」），其余失败统一 error。
    let reason = if ok {
        None
    } else {
        Some(ledger.reason.unwrap_or("error"))
    };

    emit(&json!({
        "kind": "summary",
        "mode": mode.as_str(),
        "ok": ok,
        "applied": applied,
        "total": total,
        "counts": counts,
        "scopeMatched": SCOPE_MATCHED.load(Ordering::Relaxed),
        "unmatched": ledger.unmatched,
        "elapsedMs": elapsed_ms,
        "reason": reason,
    }));
}

/// 记一轮实际处理的入口数。入口总数减掉 unmatched 就是它，但 unmatched 只在
/// `warn_unmatched_entries` 里数得到，所以这里由那一处一并记下。
static SCOPE_MATCHED: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn set_scope_matched(matched: usize) {
    SCOPE_MATCHED.store(matched, Ordering::Relaxed);
}

/// 写一条记录到 stdout。管道断了（`| head`）不该让整轮变成崩溃——报告写不出去是消费方
/// 的事，不是这一轮工作的失败。
fn emit(value: &serde_json::Value) {
    emit_to(&mut std::io::stdout(), value);
}

fn emit_to(writer: &mut dyn Write, value: &serde_json::Value) {
    let _ = writeln!(writer, "{value}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 契约表里每一个 `class` 都要有动作，一个都不能落到 `unknown`：`unknown` 对消费方
    /// 是「这条记录我读不懂」，而它出现在线协议里就是实现了却没进表。
    ///
    /// `handoff` 不在表里，见 [`action_for`]：那类记录的 `action` 是 `handoff` 字段的值，
    /// 由 [`emit_handoff_files`] 直接写出来。
    #[test]
    fn every_class_maps_to_an_action() {
        for (mode, classes) in [
            (
                Mode::Open,
                [
                    "add",
                    "edit",
                    "delete",
                    "reopen_edit",
                    "reopen_delete",
                    "revert_add",
                    "revert_edit",
                    "revert_delete",
                ]
                .as_slice(),
            ),
            (Mode::Clean, ["delete", "revert", "restore"].as_slice()),
            (
                Mode::Sync,
                ["update", "revert", "restore", "delete"].as_slice(),
            ),
        ] {
            for class in classes {
                assert_ne!(
                    action_for(mode, class),
                    "unknown",
                    "{mode:?} 的 {class} 没进动作表"
                );
            }
        }

        // 反过来钉一句：`handoff` 不许悄悄混进表里，它的动作另有来源。
        assert_eq!(action_for(Mode::Open, "handoff"), "unknown");
    }

    /// `clientFile` 是消费方拿去跟自己的路径比对的字段，拼法必须与 p4 一致：
    /// 正斜杠、带客户端名、大小写照原文。
    #[test]
    fn client_syntax_matches_the_p4_spelling() {
        if cfg!(windows) {
            assert_eq!(
                to_client_syntax(r"E:\ws\Sub\A.txt", r"E:\ws", "cli").unwrap(),
                "//cli/Sub/A.txt"
            );
            // p4 自己回的路径可能一半正斜杠一半反斜杠，根的大小写也未必一致。
            assert_eq!(
                to_client_syntax("E:/ws/suB/A.txt", r"e:\WS", "cli").unwrap(),
                "//cli/suB/A.txt"
            );
        } else {
            assert_eq!(
                to_client_syntax("/ws/Sub/A.txt", "/ws", "cli").unwrap(),
                "//cli/Sub/A.txt"
            );
            // Unix 上反斜杠是普通文件名字符，不能被当成路径分隔符。
            assert_eq!(
                to_client_syntax("/ws/back\\slash.txt", "/ws", "cli").unwrap(),
                "//cli/back\\slash.txt"
            );
        }
    }

    /// 根末尾的分隔符与大小写都不该让人拼出半截路径；路径不在根下时必须原样退化，
    /// 而不是拼出一个不存在的 client 路径。
    #[test]
    fn client_syntax_refuses_what_it_cannot_relativize() {
        let (root, inside, outside) = if cfg!(windows) {
            (r"E:\ws\", r"E:\ws\a.txt", r"E:\ws2\a.txt")
        } else {
            ("/ws/", "/ws/a.txt", "/ws2/a.txt")
        };

        assert_eq!(
            to_client_syntax(inside, root, "cli").unwrap(),
            "//cli/a.txt",
            "根末尾的分隔符该被吃掉"
        );
        assert_eq!(
            to_client_syntax(outside, root, "cli"),
            None,
            "同前缀但不是一个目录（ws2 vs ws）不算在根下"
        );
        assert_eq!(to_client_syntax(root, root, "cli"), None, "根自己不是文件");
    }

    /// 前缀匹配判据按平台分叉：Windows 的文件系统不区分大小写（p4 回的路径大小写未必与
    /// 用户给的一致），其余平台敏感。
    ///
    /// Linux 上 `/WS/a.txt` 与根 `/ws` 是两个目录，折了大小写就会把一个**不在根下**的路径
    /// 拼成 `//cli/a.txt`——一个根本不存在的 client 路径，比退化成 本地路径更糟。
    #[test]
    fn the_prefix_match_folds_case_only_on_windows() {
        if cfg!(windows) {
            assert_eq!(
                to_client_syntax(r"E:\WS\Sub\A.txt", r"e:\ws", "cli").unwrap(),
                "//cli/Sub/A.txt"
            );
        } else {
            assert_eq!(to_client_syntax("/WS/Sub/A.txt", "/ws", "cli"), None);
            assert_eq!(to_client_syntax("/ws/Sub/A.txt", "/WS", "cli"), None);
            // 拼写一致时照常拼出来：折的只是判据，不是功能本身。
            assert_eq!(
                to_client_syntax("/ws/Sub/A.txt", "/ws", "cli").unwrap(),
                "//cli/Sub/A.txt"
            );
        }
    }

    /// 路径不在根下时原样退回本地路径（契约允许的退化），并记账：告警只点名第一条，
    /// 整轮结束时由 [`report_degraded_client_files`] 汇总——逐条告警会把 stderr 淹掉。
    #[test]
    fn a_path_outside_the_root_degrades_to_the_local_path() {
        *CLIENT_SPEC.lock().unwrap() = Some(ClientSpec {
            root: if cfg!(windows) {
                r"E:\ws".to_owned()
            } else {
                "/ws".to_owned()
            },
            client: "cli".to_owned(),
        });

        let before = DEGRADED_CLIENT_FILES.load(Ordering::Relaxed);

        let outside = if cfg!(windows) {
            r"E:\other\a.txt"
        } else {
            "/other/a.txt"
        };
        assert_eq!(client_syntax(outside), outside);
        assert_eq!(
            DEGRADED_CLIENT_FILES.load(Ordering::Relaxed),
            before + 1,
            "退化要记账，否则整轮结束时汇总不出来"
        );

        let inside = if cfg!(windows) {
            r"E:\ws\a.txt"
        } else {
            "/ws/a.txt"
        };
        assert_eq!(client_syntax(inside), "//cli/a.txt");
        assert_eq!(
            DEGRADED_CLIENT_FILES.load(Ordering::Relaxed),
            before + 1,
            "拼得出来的不该进账"
        );

        *CLIENT_SPEC.lock().unwrap() = None;
    }

    /// 原生 `-Mj -Ztag reconcile` 的记录 → 本工具的字段。三件事各自钉一条：
    /// `clientFile` 回的是**本地路径**（不能透传）、`rev` 取的是 `workRev`
    /// （2024.1 没有 `rev` 字段）、认不出的动作收敛成 `edit`。
    #[test]
    fn native_reconcile_records_are_translated_by_their_fields() {
        let edit = translate_native_file(
            r#"{"action":"edit","clientFile":"/ws/a.txt","depotFile":"//depot/a.txt","type":"text","workRev":"3"}"#,
        )
        .expect("正常记录该被翻译");
        assert_eq!(edit.class, "edit");
        assert_eq!(edit.client_file, "/ws/a.txt");
        assert_eq!(edit.depot_file, "//depot/a.txt");
        assert_eq!(edit.rev, Some(3));

        let delete = translate_native_file(
            r#"{"action":"delete","clientFile":"/ws/gone.txt","depotFile":"//depot/gone.txt","workRev":"2"}"#,
        )
        .unwrap();
        assert_eq!(delete.class, "delete");
        assert_eq!(delete.rev, Some(2));

        // add 的 `workRev` 是「将要成为的版本」（实测新增文件回 `"workRev":"1"`），
        // 不是 have：契约规定新增文件没有 `rev`。
        let add = translate_native_file(
            r#"{"action":"add","clientFile":"/ws/new.txt","depotFile":"//depot/new.txt","workRev":"1"}"#,
        )
        .unwrap();
        assert_eq!(add.class, "add");
        assert_eq!(add.rev, None);

        // p4 那边还有这些动作，落到磁盘上都是「这份内容要被开出来」，一律按 edit 报。
        for action in ["move/add", "branch", "integrate", "whatever-2027-adds"] {
            let line = format!(
                r#"{{"action":"{action}","clientFile":"/ws/a.txt","depotFile":"//depot/a.txt","workRev":"2"}}"#
            );
            let file = translate_native_file(&line).unwrap();
            assert_eq!(file.class, "edit", "{action}");
            assert_eq!(file.rev, Some(2), "{action}");
        }

        // 错误行没有路径字段。将来 p4 换了形状也是这个表现：少报一行，而不是发一条
        // 消费方读不懂的记录。
        assert!(
            translate_native_file(
                r#"{"data":"No file(s) to reconcile.\n","generic":17,"severity":2}"#
            )
            .is_none()
        );
        assert!(translate_native_file("not json at all").is_none());
    }

    #[test]
    fn phases_are_stable_and_ordered() {
        assert_eq!(PHASES[0], "start");
        assert_eq!(PHASES[PHASES.len() - 1], "done");
        // 表里的顺序就是 step 的顺序：调换表项会静默改掉消费方看到的进度。
        assert_eq!(PHASES, ["start", "analyze", "digest", "report", "done"]);
    }
}
