//! 普通同步：把工作区拉到目标版本，判定权交给原生 p4。
//!
//! 与强制修复（`--sync --force`，走 `reconcile::sync`）的关系：目标相同（默认 head，
//! 可 `--to <CL>`），**分工不同**。普通同步定位成日常版本更新——覆盖保护、opened/resolve、
//! have 更新一律由原生 p4 判定；工具只做三件事：
//!
//! 1. 按范围入口问 p4「你打算传哪些文件」（`p4 -G sync -n`）；
//! 2. 把答案按范围过滤成候选，并让每个候选都精确到 `//depot/file#rev`；
//! 3. 把规格交回给原生 `p4 sync` 执行，由它再判一次 opened、noclobber、删除与 have。
//!
//! 它**不**读摘要、不扫工作区、不碰摘要缓存，也不删本地文件；更不承诺比原生 `p4 sync`
//! 快——省下的是 p4delta 自己的扫描与摘要，原生该传的字节一个不少。强制修复那条路仍然
//! 服务「无论本地改没改都修成目标版本」，它才是 `p4 sync -f` 的等价物。
//!
//! ## 为什么预演不能当成事务
//!
//! 预演是本轮候选/目标的一份**快照**，不是成功保证。预演之后才出现的候选要等下一轮；
//! 执行期间客户端状态变了（别人提交、本地被改、view 被改），仍由原生 p4 的保护判定兜住。
//! 工具与 p4 是两个进程，跨进程没有原子性可言——这一点在人类输出里明说。
//!
//! ## 已打开的文件：预演不给身份，靠补查
//!
//! 原生对「已打开、have 不在目标版本上」的文件**不写** `stat` 记录，只给一句 `info` 提示
//! （实测：`//depot/f#2 - is opened and not being changed`），路径埋在文本里，身份字段
//! 一个都没有。直接跳过它等于悄悄少做一件事——原生会把 have 拉到目标版本、把文件挂上
//! 待 resolve；而拿提示文本去猜文件名既不可靠也不能算「精确规格」。
//!
//! 所以这一类走两条**有界**的补查：`p4 opened` 只列范围内的已打开文件（规模由打开数
//! 决定，与工作区大小无关），`fstat` 再把它们翻成本地路径与**目标处**的版本。补查只在
//! 预演真的报了 `info` 时才跑；补查本身也失败时整轮停下，绝不拿一份不完整的答案去写。

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};

use crate::cli::Options;
use crate::json::{FileRecord, Mode, count, emit_file, emit_progress, sayln};
use crate::p4::marshal::MarshalRecord;
use crate::p4::process::{compute_batches, run_p4_marshal_batched, run_p4_marshal_slice};
use crate::path::local_path_key;
use crate::scope::Scope;

/// 预演：只问 p4 打算做什么，一个字都不改。
const PREVIEW_ARGS: &[&str] = &["-G", "sync", "-n"];

/// 应用：让原生 sync 自己再判一次覆盖保护、opened 与 have。
///
/// 刻意不带 `-f`：普通同步要的就是原生那套保护，`-f` 会绕过它（那正是 `--force` 那条
/// 强制修复路径的语义）。也不带 `-k`：那是「只动 have list、不动文件内容」，与「把工作区
/// 拉到目标版本」正好相反。
const APPLY_ARGS: &[&str] = &["-G", "sync"];

/// 补查第一步：列出范围内的已打开文件。规格用范围的 include 入口（不带 `@CL`——目标版本
/// 只影响「同步到哪一版」，不影响「打开了哪些」）。
const OPENED_ARGS: &[&str] = &["-G", "opened"];

/// 补查第二步：把 depot 路径翻成本地路径，并问出**目标处**的版本与动作。
///
/// 四个字段缺一不可：`clientFile` 是本地路径（范围判断与报告都用它），`headRev` 是目标处
/// 的版本（下发规格要钉它），`headAction` 是目标处的动作（`delete` 说明目标处这个路径
/// 不存在），`haveRev` 是本地 have 的版本（判据见 [`opened_candidates`]）。实测
/// `fstat <spec>@<CL>` 报的就是那个 CL 处的状态，不是 head 处的。
const FSTAT_ARGS: &[&str] = &[
    "-G",
    "fstat",
    "-T",
    "depotFile,clientFile,headRev,haveRev,headAction",
];

/// 普通同步的进度阶段表。与 open / clean / 强制修复那张不同：这条路没有 analyze / digest，
/// 却有 preview / filter / apply 三段。step 是下标 + 1，单调不回跳。
pub(crate) const PHASES: [&str; 5] = ["start", "preview", "filter", "apply", "done"];

/// 原生报出的文件动作。
///
/// 认不出的动作**不**默认为 update：它随后会变成一条真正的写命令下发给 p4。宁可这一轮
/// 报错停下，也不能凭一个猜出来的动作去动用户的工作区。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeAction {
    /// 目标处有、工作区没有：写进来。
    Added,

    /// 工作区有一份、但不是目标版本：就地更新。
    Updated,

    /// 目标时刻该路径不在库：从工作区移除，并让 p4 清掉 have 记录。
    Deleted,

    /// `-f` 语义下的「重写一遍」。普通同步不带 `-f`，理论上到不了这里；留着是因为它一旦
    /// 出现，含义与 Updated 完全一样（把文件写成目标版本），而当成本轮失败反而更糟。
    Refreshed,

    /// 已打开的文件：原生把 have 推到目标版本、必要时挂上待 resolve，**内容一个字不改**。
    ///
    /// 它**不是**原生会打印的动作词，[`NativeAction::parse`] 永远不会产出它：原生对这类
    /// 事件只有一句 `info` 提示（"is opened and not being changed"），身份要靠补查
    /// `p4 opened` 才拿得到。所以 [`NativeAction::as_str`] 对它返回 `None`——记录里宁可
    /// 诚实地说「原生没给动作词」，也不去编一个它不会打印的词。
    Opened,
}

impl NativeAction {
    fn parse(text: &str) -> Result<Self> {
        match text {
            "added" => Ok(NativeAction::Added),
            "updated" => Ok(NativeAction::Updated),
            "deleted" => Ok(NativeAction::Deleted),
            "refreshed" => Ok(NativeAction::Refreshed),
            other => bail!(
                "p4 reported an action p4delta does not know (\"{other}\"); \
                 refusing to act on a guess."
            ),
        }
    }

    /// 原生打印的动作词。补查来的 [`NativeAction::Opened`] 没有对应的词，返回 `None`。
    fn as_str(self) -> Option<&'static str> {
        match self {
            NativeAction::Added => Some("added"),
            NativeAction::Updated => Some("updated"),
            NativeAction::Deleted => Some("deleted"),
            NativeAction::Refreshed => Some("refreshed"),
            NativeAction::Opened => None,
        }
    }

    /// 记录里的 `class`。见 `docs/json-contract.md`。
    fn class(self) -> &'static str {
        match self {
            NativeAction::Added => "add",
            NativeAction::Updated | NativeAction::Refreshed => "update",
            NativeAction::Deleted => "delete",
            NativeAction::Opened => "resolve",
        }
    }

    /// 给人看的名字：原生没给动作词的（[`NativeAction::Opened`]）退回 `class` 的名字。
    fn describe(self) -> &'static str {
        self.as_str().unwrap_or(self.class())
    }

    /// 下发给 p4 的文件规格。
    ///
    /// 删除用 `#none`：它在 p4 语法里就是「这个文件在工作区里不该有」，删本地文件之外还
    /// **顺带清掉 have 记录**——清记录是它的独家本事，留下一条指向已消失文件的 have 记录，
    /// 之后普通 `p4 sync` 会认为「已是最新」而永不写回。
    ///
    /// 其余各类必须钉死 `<rev>`：不钉就是同步到 head，而 `--to <CL>` 下那正是要避免的
    /// （拉到比目标更新的版本），从输出上还看不出来。已打开的文件同样钉死——实测
    /// `p4 sync //depot/f#rev` 与原生整棵范围的 `p4 sync ...@CL` 在内容、have、opened、
    /// 待 resolve 四处逐字相同，而不钉死会在「目标早于 have」时把文件推过头。
    fn spec(self, depot_file: &str, rev: Option<u32>) -> Result<String> {
        match self {
            NativeAction::Deleted => Ok(format!("{depot_file}#none")),
            _ => {
                let rev = rev.ok_or_else(|| {
                    anyhow!("p4 reported {depot_file} without a revision to sync to")
                })?;
                Ok(format!("{depot_file}#{rev}"))
            }
        }
    }
}

/// 一条原生候选：p4 说它要对这个文件做这件事。
///
/// 两个路径的转义状态**不同**，别混用：`depot_file` 是 p4 自己回的、已经转义好的形式
/// （`weird#1.txt` 回 `weird%231.txt`），拼规格时直接接 `#rev` 就行，再转义一次会变成一个
/// 不存在的文件名；`client_file` 是本地文件系统路径的原文，范围匹配用的就是它。
#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    depot_file: String,
    client_file: String,
    rev: Option<u32>,
    action: NativeAction,
}

impl Candidate {
    /// 原生动作原文，进记录的 `nativeAction`。补查来的已打开文件没有原文，给 `None`。
    fn native_action(&self) -> Option<&'static str> {
        self.action.as_str()
    }
}

/// 一类变更在报告上的样子。顺序即输出顺序：前三类是内容动作，`resolve` 是原生对已打开
/// 文件的记账动作，放在最后。
struct GroupSpec {
    id: &'static str,
    label: &'static str,
    title: &'static str,
}

const GROUPS: [GroupSpec; 4] = [
    GroupSpec {
        id: "add",
        label: "Add",
        title: "      Writing {} files into the workspace at the depot revision.",
    },
    GroupSpec {
        id: "update",
        label: "Update",
        title: "      Updating {} files in the workspace to the depot revision.",
    },
    GroupSpec {
        id: "delete",
        label: "Delete",
        title: "      Removing {} files from the workspace.",
    },
    GroupSpec {
        id: "resolve",
        label: "Opened",
        title: "      Handing {} opened files to p4 to move their have revision.",
    },
];

/// 普通同步的完整一轮。
///
/// 流程固定为：**严格 scope → 原生预演 → 完整读取并验证候选 → 范围过滤 → 报告或精确应用**。
/// 没有「无排除快路」：只读查询允许覆盖将被排除的部分（原生不认 p4delta 的排除配置），
/// 但那些候选既不会被报告，也不会被下发。
pub(crate) async fn run_normal_sync(options: &Options, scope: &Scope) -> Result<()> {
    crate::json::set_phases(&PHASES);

    let work_dir = scope.first_dir.as_str();
    let specs = scope.query_specs(options.to);

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

    // 预演失败必须在任何写入之前挡住：p4 的答案不完整时，「要做什么」本身就是错的。
    emit_progress("preview", None);
    sayln!("   Asking p4 which files it would transfer.");
    let preview =
        run_p4_marshal_batched(options, work_dir, "sync -n", PREVIEW_ARGS, &specs).await?;
    if !preview.failures.is_empty() {
        bail!(
            "Failed to preview the sync:\n  {}",
            preview.failures.join("\n  ")
        );
    }

    emit_progress("filter", None);
    let mut candidates = candidates_from(&preview.records)?;
    if !preview.notices.is_empty() {
        // 预演报了没进正文记录的事（实测只有已打开文件那一类）：身份得补查，
        // 否则这些文件会被悄悄跳过，而原生会动它们。
        sayln!("   Asking p4 which of them are opened (its preview does not name them).");
        candidates.extend(opened_candidates(options, work_dir, scope, &preview.notices).await?);
    }
    let candidates = dedupe(candidates)?;
    let (candidates, excluded) = filter_by_scope(candidates, scope)?;
    if excluded > 0 {
        sayln!("   Left {excluded} file(s) outside the scope untouched.",);
    }

    if candidates.is_empty() {
        sayln!("No files to sync, everything up to date.");
        return Ok(());
    }

    if !options.apply {
        report_groups(options, &candidates, "preview");
        sayln!("Re-run with -a to sync the workspace.");
        return Ok(());
    }

    emit_progress("apply", None);
    sayln!(
        "   Applying the transfer with p4 (opened files and writable-file protection stay p4's call)."
    );

    apply(options, work_dir, &candidates, scope).await?;

    Ok(())
}

/// 补查已打开文件的身份。
///
/// 预演对它们的 have 推进只给一句 `info` 提示，路径埋在文本里，正文记录一条都没有。范围
/// 是硬边界，所以不能拿提示文本去猜文件名——改用两条有界查询：
///
/// 1. `p4 opened <include 规格>`：范围内的已打开文件（规模由打开数决定，与工作区大小无关）；
/// 2. `p4 fstat -T ... <这些 depot 路径[@CL]>`：本地路径、**目标处**的版本与动作。
///
/// 目标处没有东西的路径直接跳过：库里没有内容可传，而原生也不会去动一个已打开的文件
/// （实测：已打开的文件在整棵范围同步下内容一律不变）。目标处是删除状态的路径则停下——
/// 那种现场（文件开着时被别人删掉）没法在沙箱里复现，也就无从证明原生会怎么做。
///
/// `p4 opened` 给的是范围内**全部**已打开文件，比原生那句话提到的多：have 已经落在目标
/// 版本上的那些，原生连一句都不说（实测 `haveRev == headRev` 的已打开文件不在任何记录
/// 或提示里），工具也不该把它们报成待办。判据因此是 `haveRev == 目标版本 → 跳过`，而
/// **不是** `haveRev < 目标版本`——实测 have 走在目标**前面**时原生照样把它拉回目标
/// （`haveRev 2` 的目标 `#1` 在整棵 `...@1` 与 `#1` 两条路上都把 have 落到 1），漏掉
/// 这种就和原生不一致了。
async fn opened_candidates(
    options: &Options,
    work_dir: &str,
    scope: &Scope,
    notices: &[String],
) -> Result<Vec<Candidate>> {
    let encoding = crate::charset::p4_encoding();
    let specs = scope.query_specs(None);

    let opened = run_p4_marshal_batched(options, work_dir, "opened", OPENED_ARGS, &specs).await?;
    if !opened.failures.is_empty() {
        bail!(
            "Failed to list the opened files:\n  {}",
            opened.failures.join("\n  ")
        );
    }

    let mut depot_files = Vec::with_capacity(opened.records.len());
    for record in &opened.records {
        let depot_file = record
            .text("depotFile", encoding)?
            .ok_or_else(|| anyhow!("p4 listed an opened file without a depotFile"))?;
        depot_files.push(depot_file);
    }

    if depot_files.is_empty() {
        // 预演说它要动一些文件，补查却一个已打开的文件都解释不了：这份答案不完整。
        // 照着它写下去就是「拿一份少了东西的答案动工作区」。
        bail!(
            "p4 reported changes it did not name:\n  {}\n  \
             No opened file inside the scope explains them; \
             refusing to act on an incomplete answer.",
            notices.join("\n  ")
        );
    }

    // `@CL` 只钉在版本查询上：它是 per-spec 的，漏掉哪条哪条问到的就是 head 处的状态。
    let target_specs: Vec<String> = depot_files
        .iter()
        .map(|depot_file| match options.to {
            Some(changelist) => format!("{depot_file}@{changelist}"),
            None => depot_file.clone(),
        })
        .collect();

    let fstat =
        run_p4_marshal_batched(options, work_dir, "fstat", FSTAT_ARGS, &target_specs).await?;
    if !fstat.failures.is_empty() {
        bail!(
            "Failed to read the opened files' target revisions:\n  {}",
            fstat.failures.join("\n  ")
        );
    }

    let mut candidates = Vec::with_capacity(fstat.records.len());
    for record in &fstat.records {
        let depot_file = record
            .text("depotFile", encoding)?
            .ok_or_else(|| anyhow!("p4 returned an opened-file record without a depotFile"))?;
        let client_file = record
            .text("clientFile", encoding)?
            .ok_or_else(|| anyhow!("p4 returned {depot_file} without a clientFile"))?;

        let Some(head_rev) = record.text("headRev", encoding)? else {
            // 目标处这个路径在库里什么都没有：没有内容可传，原生也不会删一个已打开的文件。
            continue;
        };
        let target_rev = head_rev
            .parse::<u32>()
            .with_context(|| format!("p4 returned an unusable revision \"{head_rev}\""))?;

        match record.text("headAction", encoding)?.as_deref() {
            Some("delete") => bail!(
                "{depot_file} is opened here but does not exist at the target revision; \
                 p4delta cannot prove what a native sync would do with it, \
                 so nothing was applied. Revert or submit that file first."
            ),
            Some(_) => {}
            None => bail!("p4 returned {depot_file} without a head action"),
        }

        // 有 headRev 就必然有 haveRev：能开成 edit/delete 的文件都在 have list 里，而
        // 不在 have list 里的（open for add 的新文件）连 headRev 都没有，上面已经跳过。
        // 两个字段一个有一个没有，说明我们对这份答案的理解是错的——别猜，停下。
        let have_rev = record
            .text("haveRev", encoding)?
            .ok_or_else(|| {
                anyhow!(
                    "p4 returned {depot_file} without a have revision; \
                     p4delta cannot prove what a native sync would do with it."
                )
            })?
            .parse::<u32>()
            .with_context(|| format!("p4 returned {depot_file} with an unusable have revision"))?;

        if have_rev == target_rev {
            // 原生对 have 已在目标版本上的已打开文件什么也不说、什么也不做。
            continue;
        }

        candidates.push(Candidate {
            depot_file,
            client_file,
            rev: Some(target_rev),
            action: NativeAction::Opened,
        });
    }

    // 走到这里说明预演报了没进正文记录的事，而范围内一个能解释它的已打开文件都没有：
    // 照着这份答案写下去就是「拿一份少了东西的答案动工作区」。
    if candidates.is_empty() {
        bail!(
            "p4 reported changes it did not name:\n  {}\n  \
             None of the opened files inside the scope is at a revision other than the target; \
             refusing to act on an incomplete answer.",
            notices.join("\n  ")
        );
    }

    Ok(candidates)
}

/// 把候选下发成精确规格，并报告**真实执行**的结果。
///
/// 报告的是 apply 自己的记录，不是预演那份：两者在正常情况下应当一致，但「p4 实际做了什么」
/// 只有一个可靠来源，就是它这一次的回答。预演那份已经在 `-a` 时不输出了。
///
/// 例外是已打开的文件：原生对它们连 apply 也只有 `info` 提示，正文里一条记录都没有，
/// 所以它们的执行结果由「我们下发了哪些规格」来说明，单独报在最后。
///
/// 分批顺序执行：不同文件的执行错误汇总后一起报，解析或管道损坏立即停止后续批次——
/// 那时「p4 说了什么」已经不完整，再往下发只会让状态更难判断。已经落地的动作不回滚。
async fn apply(
    options: &Options,
    work_dir: &str,
    candidates: &[Candidate],
    scope: &Scope,
) -> Result<()> {
    let mut specs = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        specs.push(
            candidate
                .action
                .spec(&candidate.depot_file, candidate.rev)?,
        );
    }

    let mut failures: Vec<String> = Vec::new();
    let mut applied: Vec<Candidate> = Vec::new();

    for range in compute_batches(&specs) {
        // 管道或解析损坏在这里变成 Err：直接向上抛，已经报告过的批次留在输出里。
        let run =
            run_p4_marshal_slice(options, work_dir, "sync", APPLY_ARGS, &specs[range]).await?;

        let records = candidates_from(&run.records)?;
        let records = dedupe(records)?;
        // 范围校验在这里是**动作之后**的一道闸：真出现范围外的候选，说明 view 或映射在
        // 两个进程之间漂移了，继续下发的每一条都可能是越界写入。
        let (records, _) = filter_by_scope(records, scope)?;
        report_groups(options, &records, "apply");
        applied.extend(records);
        failures.extend(run.failures);
    }

    let opened: Vec<Candidate> = candidates
        .iter()
        .filter(|candidate| candidate.action == NativeAction::Opened)
        .cloned()
        .collect();
    if !opened.is_empty() {
        // 这些文件已经按预演确认过的规格下发过了，原生的回答（"is opened and not being
        // changed"）也已经原样转给了用户；`resolve` 在分组顺序里本就排在最后。
        report_groups(options, &opened, "apply");
        applied.extend(opened);
    }

    if !failures.is_empty() {
        // 逐文件的失败即便 p4 退出码是 0 也要报（`severity >= 3` 那条判据），否则
        // 「这一轮跑完了」会被当成「工作区已经是目标版本」。
        bail!(
            "Failed to sync {} file(s):\n  {}",
            failures.len(),
            failures.join("\n  ")
        );
    }

    sayln!("      Synced {} files.", applied.len());
    Ok(())
}

/// 把原生记录翻成候选。
///
/// 身份字段缺一不可：`depotFile` 与 `clientFile` 少任何一个，这条记录既没法拼下发规格、
/// 也没法做范围判断，只能算这一轮失败——少报一个文件的动作，比报错糟糕得多。
fn candidates_from(records: &[MarshalRecord]) -> Result<Vec<Candidate>> {
    let encoding = crate::charset::p4_encoding();
    let mut candidates = Vec::with_capacity(records.len());

    for record in records {
        let depot_file = record
            .text("depotFile", encoding)?
            .ok_or_else(|| anyhow!("p4 returned a sync record without a depotFile"))?;
        let client_file = record
            .text("clientFile", encoding)?
            .ok_or_else(|| anyhow!("p4 returned a sync record without a clientFile"))?;
        let action_text = record.text("action", encoding)?.ok_or_else(|| {
            anyhow!("p4 returned a sync record for {depot_file} without an action")
        })?;
        let action = NativeAction::parse(&action_text)?;

        let rev = match record.text("rev", encoding)? {
            Some(text) => Some(
                text.parse::<u32>()
                    .with_context(|| format!("p4 returned an unusable revision \"{text}\""))?,
            ),
            None => None,
        };

        candidates.push(Candidate {
            depot_file,
            client_file,
            rev,
            action,
        });
    }

    Ok(candidates)
}

/// 按 depot 身份去重。
///
/// 同一个文件出现多条事件是允许的（原生对一次同步可能分几条说），但两条给出的动作或目标
/// 版本**不一致**时不能任取一条：那两条描述的是不同的结果，猜哪一条都可能写错。
fn dedupe(candidates: Vec<Candidate>) -> Result<Vec<Candidate>> {
    let mut order: Vec<String> = Vec::new();
    let mut by_depot: HashMap<String, Candidate> = HashMap::new();

    for candidate in candidates {
        match by_depot.get(&candidate.depot_file) {
            None => {
                order.push(candidate.depot_file.clone());
                by_depot.insert(candidate.depot_file.clone(), candidate);
            }
            Some(existing) if existing == &candidate => {}
            Some(existing) => bail!(
                "p4 described conflicting actions for {}: {} #{} and {} #{}",
                existing.depot_file,
                existing.action.describe(),
                existing.rev.map_or("-".to_owned(), |rev| rev.to_string()),
                candidate.action.describe(),
                candidate.rev.map_or("-".to_owned(), |rev| rev.to_string()),
            ),
        }
    }

    Ok(order
        .into_iter()
        .filter_map(|depot_file| by_depot.remove(&depot_file))
        .collect())
}

/// 按范围过滤候选，返回 (保留的, 被排除的条数)。
///
/// 两类落空分开处理，因为补救动作完全不同：
///
/// - **被排除**（显式 exclude 或隐式 `.p4delta-scope`）：静默丢弃，只报个数。只读查询带上
///   了整棵目录，落在排除项里的候选本来就该被滤掉。
/// - **不在任何入口之内**：报错。范围是硬上限，一个证明不了属于范围的候选意味着 view 或
///   映射与求值 scope 时不一样了；照着它写下去就是越界。
///
/// 只按路径文本判断，**不要求磁盘上存在**：「本地已删除、depot 还有」的候选正是要处理的。
fn filter_by_scope(candidates: Vec<Candidate>, scope: &Scope) -> Result<(Vec<Candidate>, usize)> {
    let mut kept = Vec::with_capacity(candidates.len());
    let mut excluded = 0;

    for candidate in candidates {
        let key = local_path_key(&candidate.client_file);

        if scope.excludes.excludes_key(&key) {
            excluded += 1;
            continue;
        }

        if !scope.includes_key(&key) {
            bail!(
                "p4 offered to sync \"{}\", which is not inside the requested scope. \
                 The client view or the mapping may have changed; nothing was applied for it.",
                candidate.client_file
            );
        }

        kept.push(candidate);
    }

    Ok((kept, excluded))
}

/// 按三类分组报告。`stage` 进记录，让消费方分得清「这是预告」还是「这是刚做完的」。
fn report_groups(options: &Options, candidates: &[Candidate], stage: &'static str) {
    for spec in &GROUPS {
        let group: Vec<&Candidate> = candidates
            .iter()
            .filter(|candidate| candidate.action.class() == spec.id)
            .collect();
        if group.is_empty() {
            continue;
        }

        sayln!("{}", spec.title.replace("{}", &group.len().to_string()));
        count(spec.id, group.len());

        for candidate in &group {
            if options.list {
                sayln!("         {} \"{}\".", spec.label, candidate.client_file);
            }

            emit_file(&FileRecord {
                mode: Mode::Sync,
                class: spec.id,
                depot_file: &candidate.depot_file,
                client_file: &candidate.client_file,
                rev: candidate.rev,
                applied: options.apply,
                stage: Some(stage),
                native_action: candidate.native_action(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::p4::marshal::{MarshalRecordReader, TYPE_NULL};
    use crate::scope::{EntryKind, ExcludeSet, ScopeEntry};
    use crate::test_util::{marshal_dict, marshal_string};
    use encoding_rs::UTF_8;

    /// 借道真实解析器造一条记录：字段名与类型的口径与生产路径完全一致。
    fn record(
        depot_file: &str,
        client_file: &str,
        action: &str,
        rev: Option<&str>,
    ) -> MarshalRecord {
        let mut data = marshal_dict(&[
            ("code", "stat"),
            ("depotFile", depot_file),
            ("clientFile", client_file),
            ("action", action),
        ]);

        if let Some(rev) = rev {
            // `marshal_dict` 只写字符串字段，多出来的那一条手工追加在终止符之前。
            data.pop();
            data.extend_from_slice(&marshal_string("rev"));
            data.extend_from_slice(&marshal_string(rev));
            data.push(TYPE_NULL);
        }

        let mut reader = MarshalRecordReader::new(UTF_8);
        let mut records = Vec::new();
        reader
            .push_chunk(&data, &mut |record| {
                records.push(record.clone());
                Ok(())
            })
            .unwrap();
        reader.finish().unwrap();

        records.pop().expect("exactly one record")
    }

    fn scope_of(includes: &[(&str, EntryKind)], excludes: &[&str]) -> Scope {
        let entries: Vec<ScopeEntry> = includes
            .iter()
            .map(|(path, kind)| ScopeEntry {
                path: (*path).to_owned(),
                path_lower: local_path_key(path),
                kind: *kind,
            })
            .collect();

        Scope {
            includes: entries,
            excludes: ExcludeSet::from_dir_keys(excludes),
            first_dir: String::new(),
        }
    }

    fn candidate(client_file: &str, action: NativeAction, rev: u32) -> Candidate {
        Candidate {
            depot_file: format!(
                "//depot/{}",
                client_file.rsplit(['\\', '/']).next().unwrap()
            ),
            client_file: client_file.to_owned(),
            rev: Some(rev),
            action,
        }
    }

    // ---- 动作解析 ----

    /// 认不出的动作必须报错，不能落到 update 上：它随后会变成一条真正的写命令。
    #[test]
    fn an_unknown_native_action_is_refused() {
        assert_eq!(NativeAction::parse("added").unwrap(), NativeAction::Added);
        assert_eq!(
            NativeAction::parse("updated").unwrap(),
            NativeAction::Updated
        );
        assert_eq!(
            NativeAction::parse("deleted").unwrap(),
            NativeAction::Deleted
        );
        assert_eq!(
            NativeAction::parse("refreshed").unwrap(),
            NativeAction::Refreshed
        );

        let error = NativeAction::parse("invented-by-a-future-p4").unwrap_err();
        assert!(error.to_string().contains("does not know"), "{error}");
    }

    /// 删除下发 `#none`（顺带清 have 记录），其余各类必须钉死版本号。
    #[test]
    fn specs_pin_the_revision_except_for_deletes() {
        assert_eq!(
            NativeAction::Updated
                .spec("//depot/a.txt", Some(3))
                .unwrap(),
            "//depot/a.txt#3"
        );
        assert_eq!(
            NativeAction::Added.spec("//depot/b.txt", Some(1)).unwrap(),
            "//depot/b.txt#1"
        );
        assert_eq!(
            NativeAction::Deleted
                .spec("//depot/gone.txt", Some(2))
                .unwrap(),
            "//depot/gone.txt#none"
        );
        // 已打开的文件同样钉死：不钉就是同步到 head，而 `--to` 下会把文件推过头
        // （实测：目标早于 have 时，整棵范围 `...@CL` 不动，`#headRev` 会把 have 推过去）。
        assert_eq!(
            NativeAction::Opened
                .spec("//depot/open.txt", Some(4))
                .unwrap(),
            "//depot/open.txt#4"
        );

        // 要同步却没给版本：目标就是 head，而 `--to` 下那正是要避免的。
        assert!(NativeAction::Updated.spec("//depot/a.txt", None).is_err());
        assert!(NativeAction::Opened.spec("//depot/a.txt", None).is_err());
    }

    /// 补查来的已打开文件没有原生动作词，记录里给 `null` 而不是编一个 p4 不会打印的词。
    #[test]
    fn the_opened_class_has_no_native_action_word() {
        assert_eq!(NativeAction::Opened.as_str(), None);
        assert_eq!(NativeAction::Opened.class(), "resolve");

        // 原生永远不会打印这个词，所以解析器也不认它。
        let error = NativeAction::parse("opened").unwrap_err();
        assert!(error.to_string().contains("does not know"), "{error}");
    }

    // ---- 候选解析与去重 ----

    /// 两个路径都要按原样留着，且**不**互相翻译：`depotFile` 已经是 p4 转义好的形式，
    /// 再转义一次会指向一个不存在的文件。
    #[test]
    fn candidates_keep_both_paths_and_the_native_action() {
        let records = [
            record(
                "//depot/main/weird%231.txt",
                r"C:\ws\weird#1.txt",
                "updated",
                Some("2"),
            ),
            record(
                "//depot/main/gone.txt",
                r"C:\ws\gone.txt",
                "deleted",
                Some("1"),
            ),
        ];

        let candidates = candidates_from(&records).unwrap();

        assert_eq!(
            candidates[0],
            Candidate {
                depot_file: "//depot/main/weird%231.txt".to_owned(),
                client_file: r"C:\ws\weird#1.txt".to_owned(),
                rev: Some(2),
                action: NativeAction::Updated,
            }
        );
        assert_eq!(candidates[1].action, NativeAction::Deleted);
    }

    /// 身份字段缺失就是这一轮失败：少一个路径，这条记录既拼不出规格也做不了范围判断。
    #[test]
    fn a_record_without_identity_is_an_error() {
        let mut data = marshal_dict(&[("code", "stat"), ("action", "updated")]);
        data.pop();
        data.extend_from_slice(&marshal_string("depotFile"));
        data.extend_from_slice(&marshal_string("//depot/a.txt"));
        data.push(TYPE_NULL);

        let mut reader = MarshalRecordReader::new(UTF_8);
        let mut records = Vec::new();
        reader
            .push_chunk(&data, &mut |record| {
                records.push(record.clone());
                Ok(())
            })
            .unwrap();
        reader.finish().unwrap();

        let error = candidates_from(&records).unwrap_err();
        assert!(error.to_string().contains("clientFile"), "{error}");
    }

    #[test]
    fn duplicate_identical_events_collapse_to_one() {
        let one = candidate(r"C:\ws\a.txt", NativeAction::Updated, 2);

        assert_eq!(dedupe(vec![one.clone(), one.clone()]).unwrap(), [one]);
    }

    /// 同一个 depot 文件的两条事件给出不同结果时不能任取一条。
    #[test]
    fn conflicting_events_for_one_file_are_refused() {
        let first = candidate(r"C:\ws\a.txt", NativeAction::Updated, 2);
        let second = candidate(r"C:\ws\a.txt", NativeAction::Updated, 3);

        let error = dedupe(vec![first, second]).unwrap_err();
        assert!(error.to_string().contains("conflicting actions"), "{error}");
    }

    // ---- 范围过滤 ----

    #[test]
    fn a_candidate_inside_a_directory_entry_is_kept() {
        let scope = scope_of(&[(r"C:\ws", EntryKind::Directory)], &[]);
        let candidates = vec![candidate(r"C:\ws\sub\a.txt", NativeAction::Added, 1)];

        let (kept, excluded) = filter_by_scope(candidates, &scope).unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(excluded, 0);
    }

    /// 候选不在任何入口里 → 报错，而不是悄悄放过。
    #[test]
    fn a_candidate_outside_every_entry_is_refused() {
        let scope = scope_of(&[(r"C:\ws\src", EntryKind::Directory)], &[]);
        let candidates = vec![candidate(r"C:\other\a.txt", NativeAction::Added, 1)];

        let error = filter_by_scope(candidates, &scope).unwrap_err();
        assert!(
            error.to_string().contains("not inside the requested scope"),
            "{error}"
        );
    }

    /// 被排除的候选只丢不报，且计入「Left N files outside the scope untouched」。
    #[test]
    fn an_excluded_candidate_is_dropped_without_failing() {
        let scope = scope_of(&[(r"C:\ws", EntryKind::Directory)], &[r"C:\ws\gen"]);
        let candidates = vec![
            candidate(r"C:\ws\gen\out.txt", NativeAction::Added, 1),
            candidate(r"C:\ws\kept.txt", NativeAction::Added, 1),
        ];

        let (kept, excluded) = filter_by_scope(candidates, &scope).unwrap();

        assert_eq!(excluded, 1);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].client_file, r"C:\ws\kept.txt");
    }

    /// 文件入口按精确路径匹配，目录入口按子树匹配。
    #[test]
    fn a_file_entry_matches_only_its_own_path() {
        let scope = scope_of(&[(r"C:\ws\a.txt", EntryKind::File)], &[]);

        assert!(scope.includes_key(&local_path_key(r"C:\ws\a.txt")));
        assert!(!scope.includes_key(&local_path_key(r"C:\ws\a.txt.bak")));
        assert!(!scope.includes_key(&local_path_key(r"C:\ws\sub\a.txt")));
    }

    // ---- 报告与命令 ----

    /// 四类分组必须覆盖全部动作，否则那个文件会被静默丢掉。
    #[test]
    fn groups_cover_every_native_action_class() {
        let classes: Vec<&str> = GROUPS.iter().map(|spec| spec.id).collect();
        assert_eq!(classes, ["add", "update", "delete", "resolve"]);

        for action in [
            NativeAction::Added,
            NativeAction::Updated,
            NativeAction::Refreshed,
            NativeAction::Deleted,
            NativeAction::Opened,
        ] {
            assert!(
                classes.contains(&action.class()),
                "{action:?} 没有对应的分组"
            );
        }
    }

    /// 每个标题恰好一个占位符，且前导空格与其余模式一致。
    #[test]
    fn every_title_has_one_placeholder_and_the_output_indent() {
        for spec in &GROUPS {
            assert_eq!(spec.title.matches("{}").count(), 1, "{}", spec.id);
            assert!(spec.title.starts_with("      "), "{}", spec.id);
        }
    }

    /// 进度阶段表就是普通同步实际走的那五段。
    #[test]
    fn phases_are_the_normal_sync_path() {
        assert_eq!(PHASES, ["start", "preview", "filter", "apply", "done"]);
    }

    /// 普通同步不做强制修复，也不碰只记账的 `-k`。
    #[test]
    fn the_apply_command_never_forces_or_touches_have() {
        assert_eq!(APPLY_ARGS, ["-G", "sync"].as_slice());
        assert_eq!(PREVIEW_ARGS, ["-G", "sync", "-n"].as_slice());
        for args in [APPLY_ARGS, PREVIEW_ARGS] {
            assert!(!args.contains(&"-f"), "普通同步不做强制修复");
            assert!(!args.contains(&"-k"), "不碰 have list 的只记账语义");
        }
    }

    /// 补查的两条命令都是只读的，而且模板字段就是候选要用的那三个。
    #[test]
    fn the_supplementary_queries_are_read_only() {
        assert_eq!(OPENED_ARGS, ["-G", "opened"].as_slice());
        assert_eq!(FSTAT_ARGS[0..2], ["-G", "fstat"]);
        for args in [OPENED_ARGS, FSTAT_ARGS] {
            for forbidden in ["-f", "-k", "sync", "submit", "revert"] {
                assert!(!args.contains(&forbidden), "{args:?} 不该带 {forbidden}");
            }
        }

        let template = FSTAT_ARGS[3];
        for field in [
            "depotFile",
            "clientFile",
            "headRev",
            "haveRev",
            "headAction",
        ] {
            assert!(template.contains(field), "模板缺 {field}：{template}");
        }
    }
}
