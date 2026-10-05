//! p4 子进程的调用与批次切分。

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Error, Result, anyhow, bail};
use async_process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use encoding_rs::Encoding;
use futures::io::{AsyncBufReadExt, AsyncWriteExt};
use futures::{AsyncReadExt, StreamExt};

use crate::READ_BUFFER_SIZE;
use crate::charset::{
    decode_p4_bytes, p4_command_encoding, p4_encoding, strip_bom, trim_line_ending,
};
use crate::cli::Options;
use crate::json::sayln;
use crate::model::HaveRecord;
use crate::p4::marshal::{MarshalRecord, MarshalRecordReader, MarshalStreamParser};

// 参数改成从 stdin 交给 p4 之后（见 [`argument_payload`]），这个数字不再是 Win32 命令行
// 长度上限（32767 字符，https://learn.microsoft.com/en-us/troubleshoot/windows-client/shell-experience/command-line-string-limitation）。
// 保留原值有两个理由：一是让分片粒度不变（片数也就是 [`run_p4_command_batched`] 的并发度），
// 二是 [`takes_paths_from_stdin`] 说不认 `-x` 的命令（`p4 ignores`）仍把路径挂在命令行上，
// 它们依旧受那个上限约束。
pub(crate) const ARGUMENT_LENGTH_MAX: usize = 32767 - 2048;

/// 交给 p4 的参数载荷：一行一个参数，按 `encoding`（p4 的命令字符集）编码。
/// 同时数出多少个参数在这个字符集里表达不了，供调用方提醒一次。
///
/// 路径参数刻意不走命令行。p4 把命令行参数当**字节流**、按命令字符集解码（见 `p4 help charset`），
/// 而 Windows 会先把 UTF-16 命令行按系统 ANSI 代码页转成字节：en-US 机器是 1252，
/// 中文名在那一步就变成 `?????.txt`，p4 拿到的路径与磁盘上的对不上。`-x -` 绕开命令行，
/// 字节由 p4delta 按 p4 将要用的字符集写出，中间不再有操作系统插手。
///
/// 代价：`-x` 是「一行一个参数」，路径里含换行符时会被拆成两条。含换行的路径在 Windows 上
/// 本来就建不出来，在 Unix 上也是极少见的病态输入；换掉的是「非 ASCII 路径在 Windows 上
/// 完全不可用」。
pub(crate) fn argument_payload(args: &[String], encoding: &'static Encoding) -> (Vec<u8>, usize) {
    let mut payload = Vec::new();
    let mut unrepresentable = 0;

    for arg in args {
        // 字符集表达不了的字符会退化成 `&#NNNN;` 形式的数字引用：那种配置下 p4 也无法
        // 表示这个名字，让它去报「找不到文件」，比自己在这里猜一个名字更诚实。
        let (bytes, _, had_errors) = encoding.encode(arg);
        if had_errors {
            unrepresentable += 1;
        }

        payload.extend_from_slice(&bytes);
        payload.push(b'\n');
    }

    (payload, unrepresentable)
}

/// 已经提醒过「有路径写不进当前字符集」。
/// 载荷按分片各算一次，同一个问题会重复出现，刷屏没有信息量。
static WARNED_UNREPRESENTABLE: AtomicBool = AtomicBool::new(false);

/// 提醒一次：这些路径 p4 一定匹配不上。它是「p4 报 no such file(s)」的原因，
/// 不说的话用户只会看到一串语焉不详的失败。
fn warn_unrepresentable_paths(unrepresentable: usize, total: usize) {
    if unrepresentable == 0 || WARNED_UNREPRESENTABLE.swap(true, Ordering::Relaxed) {
        return;
    }

    eprintln!(
        "Warning: {unrepresentable} of {total} path(s) cannot be written in {}; \
         p4 will not match them. Point P4CHARSET/P4COMMANDCHARSET at a charset that can.",
        p4_command_encoding().name()
    );
}

/// 把这一批参数改成从 stdin 传（`p4 -x - -b <个数>`），返回要写进 stdin 的载荷。
///
/// 全局选项必须排在命令之前，所以调用方要在加命令之前调用它（[`build_p4_command`] 已经这样做了）。
/// 参数为空时什么都不做并返回 `None`：`-x -` 配上空的 stdin 会让 p4 一个参数都拿不到。
/// `-b` 是 p4 内部把参数分组处理的大小（`p4 help usage`），给足本批的参数个数可以让这一批
/// 一次收完，不必按 p4 的默认值再来回几趟。
pub(crate) fn feed_arguments_via_stdin(cmd: &mut Command, args: &[String]) -> Option<Vec<u8>> {
    if args.is_empty() {
        return None;
    }

    let (payload, unrepresentable) = argument_payload(args, p4_command_encoding());
    warn_unrepresentable_paths(unrepresentable, args.len());

    cmd.arg("-x").arg("-").arg("-b").arg(args.len().to_string());
    cmd.stdin(Stdio::piped());

    Some(payload)
}

/// 把载荷写进 p4 的 stdin，然后关掉管道让它读到 EOF。
///
/// 写入失败不上报：p4 可能在读完全部参数之前就退出（比如整批失败），那时写入拿到的是
/// EPIPE，它不是新信息——p4 自己的退出状态才是结论。
pub(crate) async fn write_p4_arguments(arguments: Option<(ChildStdin, Vec<u8>)>) {
    let Some((mut stdin, payload)) = arguments else {
        return;
    };

    let _ = stdin.write_all(&payload).await;
    // 关掉管道，p4 才读得到 EOF；只 drop 也行，但显式关掉更清楚。
    let _ = stdin.close().await;
}

/// `p4 have` 的参数。`-G` 让它输出 marshal 格式（见 [`crate::p4::marshal`]）。
const HAVE_ARGS: &[&str] = &["-G", "have"];

/// 跑 `p4 -G have`，解析它的 marshal 二进制输出，取回同步时间戳（syncTime），
/// 供时间戳快筛使用。
///
/// `specs` 是本轮范围的 file spec 列表，一个入口一条。它们走 `-x -` 送进 stdin：
/// 入口可以是含非 ASCII 字符的路径，挂在命令行上会被 Windows 的 ANSI 代码页转换吃掉，
/// 一个这样的路径就足以让整批查询失败（见 [`command_line_safe`]）。
pub(crate) async fn run_p4_have(
    options: &Options,
    work_dir: &str,
    specs: &[String],
) -> Result<HashMap<String, HaveRecord>> {
    sayln!("   Querying file sync timestamps.");
    let start_time = Instant::now();

    let (mut cmd, payload) = build_p4_command(
        crate::locate::p4_exe()?,
        work_dir,
        HAVE_ARGS,
        specs,
        options.workspace.as_deref(),
        None,
    );

    if options.verbose {
        sayln!("    Running: p4 -G have {}", specs.join(" "));
    }

    let mut child = cmd.spawn()?;
    let P4Pipes {
        arguments,
        stdout,
        stderr,
    } = take_p4_pipes(&mut child, payload)?;

    // 参数在 stdin 上：写完关掉管道，p4 才读得到 EOF。
    write_p4_arguments(arguments).await;

    // 边读边解析：大工作区的原始响应有好几 GB，整份缓冲下来正是当初把内存撑爆的原因。
    let read_stdout = async move {
        let mut parser = MarshalStreamParser::new(p4_encoding());
        let mut stdout = stdout;
        let mut chunk = vec![0u8; READ_BUFFER_SIZE];

        loop {
            let read = stdout.read(&mut chunk).await?;
            if read == 0 {
                break;
            }
            parser.push_chunk(&chunk[..read])?;
        }

        Ok::<_, Error>(parser.finish())
    };

    let read_stderr = read_p4_stderr(stderr);

    let (records, stderr_bytes) = futures::join!(read_stdout, read_stderr);
    let records = records?;

    let status = child.status().await?;
    if !status.success() {
        bail!(
            "p4 -G have failed: {}",
            String::from_utf8_lossy(&stderr_bytes)
        );
    }

    sayln!(
        "      Query complete in {} seconds.",
        start_time.elapsed().as_secs_f32()
    );

    Ok(records)
}

/// p4 是否接受从 stdin（`-x -`）送来的路径参数。
///
/// `p4 ignores` 不接受：`-i` 只数命令行上的参数，`-x` 送来的一个都看不见，直接报
/// `At least one file path must provided.` 并退出 1（p4 r24.1 实测；同一批路径挂在命令行上
/// 就正常，`ignores -v` 反倒认 `-x`）。整个 `ignores` 家族一律按不认处理：`-v` 目前只会
/// 被无参数调用，用不上 stdin，而少记一条「哪个变体认 `-x`」的例外更不容易出错。
///
/// 代价有两条，都指向同一个后果——非 ASCII 的路径名在 Windows 上失配：
///
/// - 这些名字只能挂在命令行上，在 ANSI 代码页表达不了它们的机器上会被转换吃掉（p4 侧的硬限制）；
/// - `p4 ignores` 因此问不出这些名字的忽略状态。文件级过滤有一条回退：改用同样
///   认 stdin 的 `p4 add -n` 补判（见 `crate::workspace::apply_file_ignores`）。
///   目录级剪枝没有回退，那些目录照常完整扫描——只损失性能，不改变结论。
fn takes_paths_from_stdin(always_args: &[&str]) -> bool {
    !always_args.contains(&"ignores")
}

/// 该路径能不能安全地挂在 p4 命令行上。
///
/// 只有 Windows 需要这道限制。p4 在 Windows 上把命令行解析**两遍**——宽字符一遍
/// （`GetCommandLineW`）、ANSI 一遍（`GetCommandLineA`）——再比对两遍得到的参数**个数**，
/// 不一致就报 `Argument parsing ambiguity.` 并以 -1 退出（`clientmain.cc` 里
/// `n_argc != w_argc` 那条检查）。ANSI 那一遍会把当前代码页表示不了的字符换成 `?`，
/// 而 Win32 的文件名匹配里 `?` 可以匹**零个**字符：实测 `????.txt` 会同时匹到
/// `c.txt`、`lib.txt` 与 `使用说明.txt`。p4 自己还会展开通配符，于是同一个参数在两遍里
/// 展开成不同个数，**整批**查询直接失败——一个中文名足以让同批所有 ASCII 路径的忽略
/// 判断一起失效（CI 的 windows-latest 是 en-US，CP1252 下 `使用说明.txt` 必然变成
/// `????.txt`，`tests/e2e_prune.rs` 的三个用例因此全红）。
///
/// 代码页里表示得出来的非 ASCII 字符同样不该送进去：p4 拿到的是代码页字节，却按命令
/// 字符集（P4COMMANDCHARSET，缺省随 P4CHARSET）解释，两边不一致时照样失配——那正是
/// 路径参数改走 stdin 的原因（见 [`argument_payload`]）。ASCII 是唯一在两处都是同一串
/// 字节的集合。
///
/// Unix 上没有这道转换，路径照常交给 p4。
pub(crate) fn command_line_safe(path: &str) -> bool {
    !cfg!(windows) || path.is_ascii()
}

/// 把一批路径劈成「能交给 p4 命令行的」与「交不出去的」两半，顺序各自保持。
///
/// 消费路径的迭代器而不是先要一个 `&[String]`：调用方的路径常散在自己的结构体里
/// （[`crate::workspace::apply_file_ignores`] 就是从 `WorkspaceFile` 里逐个借出），
/// 这样它们不必先克隆出一个中间 `Vec`——每个路径只在这里克隆一次，就是返回的两个
/// `Vec` 里各归其位的那一份。
///
/// 后半不是终点，但两个调用点的善后差得很远，一句通用文案说不准，所以这里只划分、
/// 不报告——报告交给各自的调用方，在 `-v` 下按自己的口径打印：
///
/// - 文件级过滤有回退：`p4 ignores` 不认 stdin，但 `p4 add -n` 认，于是那些名字改走
///   那条字节无损的通道补判（见 [`crate::workspace::apply_file_ignores`]）。
/// - 目录级剪枝没有回退：交不出去的目录照常完整扫描，只损失性能。
pub(crate) fn split_command_line_paths<'a>(
    paths: impl IntoIterator<Item = &'a String>,
) -> (Vec<String>, Vec<String>) {
    let mut ready = Vec::new();
    let mut unreadable = Vec::new();

    for path in paths {
        if command_line_safe(path) {
            ready.push(path.clone());
        } else {
            unreadable.push(path.clone());
        }
    }

    (ready, unreadable)
}

/// 组装一次「一批路径参数」的 p4 调用，返回命令与要写进 stdin 的载荷。
///
/// 命令行形状（顺序即下面的构造顺序；全局选项必须排在命令之前，已实测）：
///
/// ```text
/// p4 -x - -b <本批参数个数> [-c <client>] <命令...> [-c <changelist>]
///         └─ stdin：一行一个参数，按 p4 的命令字符集编码
/// ```
///
/// 路径参数怎么送只在这里决定：默认走 stdin（载荷由调用方写进去），只有
/// [`takes_paths_from_stdin`] 说不认的命令才留在命令行上——那时返回的载荷是 `None`，
/// 一个字节都不该往 stdin 写。
///
/// 已知的缺口：`-c <client>` 是全局选项，必须排在命令之前，所以客户端名仍留在命令行上，
/// 非 ASCII 的客户端名同样会被 ANSI 代码页转换吃掉。这一条 p4delta 目前无解。
///
/// `program` 由调用方给（见 [`crate::locate::p4_exe`]）而不是在这里现找：这样本函数只
/// 负责拼命令行，纯 argv 的用例不必依赖跑测试的机器上有没有 p4。
pub(crate) fn build_p4_command(
    program: &Path,
    work_dir: &str,
    always_args: &[&str],
    batched_args: &[String],
    client: Option<&str>,
    changelist: Option<u32>,
) -> (Command, Option<Vec<u8>>) {
    let mut cmd = Command::new(program);
    cmd.current_dir(work_dir);
    // p4 在 Windows 上按继承来的 PWD 找 .p4config，显式指定 cwd 时必须同时清掉 PWD。
    cmd.env_remove("PWD");

    let from_stdin = takes_paths_from_stdin(always_args);
    let payload = if from_stdin {
        feed_arguments_via_stdin(&mut cmd, batched_args)
    } else {
        None
    };

    if let Some(client) = client {
        cmd.arg("-c");
        cmd.arg(client);
    }

    cmd.args(always_args);

    if let Some(changelist) = changelist {
        cmd.arg("-c");
        cmd.arg(changelist.to_string());
    }

    if !from_stdin {
        // 只认命令行路径的命令，见 [`takes_paths_from_stdin`]。
        cmd.args(batched_args);
    }

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    (cmd, payload)
}

/// 严格版参数载荷：每个参数都必须能**无损**写进 p4 的命令字符集，且不含会把
/// `-x` 的一行拆成两行的字符。
///
/// 与 [`argument_payload`] 的差别是立场，不是编码：那一版把表达不了的字符换成数字引用、
/// 只提醒一句（适合 `p4 ignores` 这类「判不出来就放宽」的调用），这一版直接拒绝。
/// 普通同步下发的每条参数都是「p4 必须原样认出的东西」——一个文件规格认不出来，
/// 结果就是那个文件静默地没被同步，而整轮照报成功。
fn strict_argument_payload(args: &[String]) -> Result<Vec<u8>> {
    let encoding = p4_command_encoding();
    let mut payload = Vec::new();

    for arg in args {
        // `-x` 是「一行一个参数」，路径里含换行或 NUL 会被拆成两条、或截断成一个别的
        // 规格。这种名字在 Windows 上建不出来，在 Unix 上是极少见的病态输入——
        // 拒绝它，而不是让 p4 拿到一个没人写过的路径。
        if arg.contains(['\n', '\r', '\0']) {
            bail!("Refusing to send a p4 argument containing a line break or NUL: {arg:?}");
        }

        let (bytes, _, had_errors) = encoding.encode(arg);
        if had_errors {
            bail!(
                "\"{arg}\" cannot be written in {}; point P4COMMANDCHARSET at a charset that can.",
                encoding.name()
            );
        }

        payload.extend_from_slice(&bytes);
        payload.push(b'\n');
    }

    Ok(payload)
}

/// 组装一次结构化查询的 p4 调用：参数一律走 stdin，编码**严格**校验。
///
/// 与 [`build_p4_command`] 分开而不是加个开关：后者的宽松编码策略是 [FailureMode::Warn]
/// 那条路径的一部分（`p4 ignores` 认不出名字也要照跑），两者的取舍相反，混在一个函数里
/// 早晚会有人把开关传错。
fn build_p4_command_strict(
    program: &Path,
    work_dir: &str,
    always_args: &[&str],
    batched_args: &[String],
    client: Option<&str>,
) -> Result<(Command, Option<Vec<u8>>)> {
    let payload = strict_argument_payload(batched_args)?;

    let mut cmd = Command::new(program);
    cmd.current_dir(work_dir);
    // p4 在 Windows 上按继承来的 PWD 找 .p4config，显式指定 cwd 时必须同时清掉 PWD。
    cmd.env_remove("PWD");

    if !payload.is_empty() {
        cmd.arg("-x")
            .arg("-")
            .arg("-b")
            .arg(batched_args.len().to_string());
        cmd.stdin(Stdio::piped());
    }

    if let Some(client) = client {
        cmd.arg("-c");
        cmd.arg(client);
    }

    cmd.args(always_args);

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    Ok((cmd, (!payload.is_empty()).then_some(payload)))
}

/// 一次结构化 `-G` 调用的产出。
pub(crate) struct MarshalRun {
    /// `code:"stat"` 的记录：命令的正文。
    pub(crate) records: Vec<MarshalRecord>,

    /// `code:"info"` 的提示。与 [`RecordVerdict::Diagnostic`] 分开，因为它们的含义相反：
    /// 那类「无事可做」的提示说明正文已经说完了，而 `info` 说明 p4 打算做的事**没进正文
    /// 记录**——路径只写在消息文本里。普通同步据此知道该去补查已打开文件的身份。
    pub(crate) notices: Vec<String>,

    /// p4 报出的失败。进程起不来、参数编不出来、管道或解析损坏时直接 `Err`——
    /// 那些情况下连「p4 说了什么」都不完整，汇总也就无从谈起。
    pub(crate) failures: Vec<String>,
}

/// 一条 marshal 记录在「这一轮算不算成功」上的分量。
///
/// 判据来自沙箱实测（2024.1），不是从文档推的：
///
/// | 情形 | `code` | `severity` | 退出码 |
/// |---|---|---|---|
/// | 文件动作（正文） | `stat` | 缺席 | 0 |
/// | `file(s) up-to-date.` / `no such file(s).` | `error` | 2 | 0 |
/// | `is opened and not being changed` 那类 | `info` | 缺席（有 `level`） | 0 |
/// | `Can't clobber writable file` | `error` | 3 | 1 |
/// | `Unintelligible revision specification` | `error` | 3 | 1 |
///
/// 两个 2 级的情形**无法**只靠结构化字段分开——`generic` 也都是 17。所以这里不试着
/// 分类它们：2 级一概当提示转出去，而整轮的成败由「有没有 3 级 / 退出码非零」决定。
/// 未知 `code` 或 `severity` 缺失一律按失败处理：p4 换一种说法时宁可红一次，
/// 也不能把「这条记录我没读懂」当成「这条记录说没事」。
///
/// `info` 单独归 [`RecordVerdict::Notice`]：它和 2 级错误一样不影响成败，但它是**唯一**
/// 会告诉我们「p4 还打算动一些没进正文记录的文件」的信号（实测：已打开文件的 have 推进
/// 只以 `info` 出现，路径埋在消息文本里）。
fn classify_record(record: &MarshalRecord, command: &str) -> RecordVerdict {
    let encoding = p4_encoding();
    let Ok(code) = record.text("code", encoding) else {
        return RecordVerdict::Failure(format!("p4 {command} returned an undecodable record"));
    };

    let message = || {
        record
            .text("data", encoding)
            .ok()
            .flatten()
            .unwrap_or_else(|| "(no message)".to_owned())
            .trim_end()
            .to_owned()
    };

    match code.as_deref() {
        Some("stat") => RecordVerdict::Body,
        Some("info") => RecordVerdict::Notice(message()),
        Some("error") => {
            let severity = record
                .text("severity", encoding)
                .ok()
                .flatten()
                .and_then(|text| text.parse::<i32>().ok());
            match severity {
                // 3 = error、4 = fatal（`p4 help` 里的 severity 分级）。
                Some(level) if level >= 3 => RecordVerdict::Failure(message()),
                Some(_) => RecordVerdict::Diagnostic(message()),
                None => RecordVerdict::Failure(format!(
                    "p4 {command} returned an error record without a usable severity"
                )),
            }
        }
        Some(other) => RecordVerdict::Failure(format!(
            "p4 {command} returned a record with an unknown code \"{other}\""
        )),
        None => RecordVerdict::Failure(format!("p4 {command} returned a record without a code")),
    }
}

enum RecordVerdict {
    /// 不带结论的正文（`stat`）。
    Body,
    /// p4 打算做点什么的提示：那件事**没有**对应的正文记录，只有这行文本。
    Notice(String),
    /// 可以忽略的提示：p4 对「无事可做」也写字，那是正常结果而不是失败。
    Diagnostic(String),
    /// 失败：消息直接进整轮的错误汇总。
    Failure(String),
}

/// 顺序跑完一批结构化 `-G` 调用，收齐记录与失败。
///
/// **顺序**而不是并发：这条路只服务普通同步，而它的写入动作由 p4 自己判定覆盖保护与
/// opened 状态，多进程并发下发并没有被验证过；先要正确，再谈快。
pub(crate) async fn run_p4_marshal_batched(
    options: &Options,
    work_dir: &str,
    command: &str,
    always_args: &[&'static str],
    batched_args: &[String],
) -> Result<MarshalRun> {
    let batches = compute_batches(batched_args);
    let mut run = MarshalRun {
        records: Vec::new(),
        notices: Vec::new(),
        failures: Vec::new(),
    };

    for range in batches {
        // 解析或管道损坏会在这里变成 Err，调用方据此停止后续批次——已经落地的动作不回滚，
        // 但也不会在「p4 说了什么」不完整的情况下继续往下发。
        let slice = run_p4_marshal_slice(
            options,
            work_dir,
            command,
            always_args,
            &batched_args[range],
        )
        .await?;
        run.records.extend(slice.records);
        run.notices.extend(slice.notices);
        run.failures.extend(slice.failures);
    }

    Ok(run)
}

/// 跑一批结构化调用中的一片。
pub(crate) async fn run_p4_marshal_slice(
    options: &Options,
    work_dir: &str,
    command: &str,
    always_args: &[&'static str],
    batched_args: &[String],
) -> Result<MarshalRun> {
    let program = crate::locate::p4_exe()?;
    let (mut cmd, payload) = build_p4_command_strict(
        program,
        work_dir,
        always_args,
        batched_args,
        options.workspace.as_deref(),
    )?;

    let mut child = cmd.spawn()?;
    let P4Pipes {
        arguments,
        stdout,
        stderr,
    } = take_p4_pipes(&mut child, payload)?;

    // 三条管道并发推：只读 stdout 时，塞满 stderr 管道的子进程会永远阻塞；
    // 参数那一侧同理——写不完的载荷会把 p4 堵在读取上，而它又在等我们读输出。
    let write_args = write_p4_arguments(arguments);
    let read_stdout = read_marshal_records(stdout);
    let read_stderr = read_p4_stderr(stderr);

    let (_, records, stderr_bytes) = futures::join!(write_args, read_stdout, read_stderr);
    let records = records?;

    let status = child.status().await?;

    let mut run = MarshalRun {
        records: Vec::new(),
        notices: Vec::new(),
        failures: Vec::new(),
    };
    for record in &records {
        match classify_record(record, command) {
            RecordVerdict::Body => run.records.push(record.clone()),
            // 原生提示照原样转出去：它就是用户手工跑 p4 时会看到的那几行，
            // 而「preview 与真实动作一致」的保证正建立在用户看得见它们上。
            RecordVerdict::Notice(message) => {
                eprintln!("{message}");
                run.notices.push(message);
            }
            RecordVerdict::Diagnostic(message) => eprintln!("{message}"),
            RecordVerdict::Failure(message) => run.failures.push(message),
        }
    }

    let stderr_text = String::from_utf8_lossy(&stderr_bytes);
    let stderr_text = stderr_text.trim();
    if !status.success() {
        run.failures
            .push(format!("p4 {command} exited with {status}: {stderr_text}"));
    } else if !stderr_text.is_empty() {
        // 退出码为 0 但 stderr 有输出：实测 `-G` 下 p4 把逐文件错误写在记录流里、
        // stderr 是空的，所以这里有东西就意味着出现了没进记录流的说法。宁可红一次。
        run.failures
            .push(format!("p4 {command} wrote to stderr: {stderr_text}"));
    }

    Ok(run)
}

/// 逐块读 p4 的 `-G` 输出并解析成 marshal 记录。
///
/// 与 [`read_p4_lines`] 的分工：那个解码文本行（`p4 ignores` / `p4 where` 那类给人看的
/// 输出），这个读二进制记录流。两者都不适合几千万行的响应，但结构化查询的正文就是
/// 「每个要动的文件一条记录」，规模由候选决定。
async fn read_marshal_records(stream: async_process::ChildStdout) -> Result<Vec<MarshalRecord>> {
    let mut reader = MarshalRecordReader::new(p4_encoding());
    let mut stdout = stream;
    let mut chunk = vec![0u8; READ_BUFFER_SIZE];
    let mut records = Vec::new();

    loop {
        let read = stdout.read(&mut chunk).await?;
        if read == 0 {
            break;
        }

        reader.push_chunk(&chunk[..read], &mut |record| {
            records.push(record.clone());
            Ok(())
        })?;
    }

    reader.finish()?;
    Ok(records)
}

/// 一次 p4 调用的三条管道。
pub(crate) struct P4Pipes {
    /// 要写进 stdin 的参数：句柄与载荷。`None` 表示这一批没有参数要送。
    pub(crate) arguments: Option<(ChildStdin, Vec<u8>)>,
    pub(crate) stdout: ChildStdout,
    pub(crate) stderr: ChildStderr,
}

/// 取走子进程的三条管道。
///
/// `payload` 非空时 stdin 由 [`feed_arguments_via_stdin`] 提前开了管道，取不到就是这一对
/// 不变量被破坏，报错而不是 panic。
pub(crate) fn take_p4_pipes(child: &mut Child, payload: Option<Vec<u8>>) -> Result<P4Pipes> {
    let arguments = payload
        .map(|payload| {
            child
                .stdin
                .take()
                .map(|stdin| (stdin, payload))
                .ok_or_else(|| anyhow!("Failed to capture p4 stdin"))
        })
        .transpose()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to capture p4 stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Failed to capture p4 stderr"))?;

    Ok(P4Pipes {
        arguments,
        stdout,
        stderr,
    })
}

/// p4 起不来时给 lenient 调用方的告警。定位失败与 spawn 失败都走这里，
/// 免得同一句话在两处各写一遍、日后改一处漏一处。
fn warn_p4_could_not_start(command: &str, reason: &dyn std::fmt::Display) {
    eprintln!("Warning: p4 {command} could not be started: {reason}");
}

/// 一次 p4 调用失败与否的判据。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureMode {
    /// 退出码与 stderr 都不作判据：调用方自己解析输出来下结论。
    ///
    /// `p4 add -n` 遇到被忽略的文件**必然**退出 1，而那条正是调用方要的信号
    /// （见 `crate::workspace::apply_file_ignores` 的补充判据），报成警告只会误导。
    /// p4 起不来时仍按 lenient 降级，不中断整轮。
    Silent,
    /// 只告警，不失败。查询与预演用它。
    Warn,
    /// 退出码非零即失败，stderr 上的提示不算失败。
    ExitCode,
    /// 退出码非零，**或退出码为 0 但 stderr 有输出**，都算失败。
    ///
    /// p4 把逐文件错误写在 stderr 上，退出码却仍是 0：实测
    /// `no permission for operation on file(s).`（protections 拒绝 open）与
    /// `file(s) not on client.` 都是 exit 0。p4delta 自己点数的改状态命令必须用它，
    /// 否则「一个文件都没打开」会被报成 "Applied N changes / Inconsistencies fixed."。
    ///
    /// 反例是转交 p4 reconcile/clean 的兜底路径：p4 对「无事可做」也往 stderr 写
    /// `<path> - no file(s) to reconcile.`（同样 exit 0），那里只能用 [Self::ExitCode]。
    ExitCodeOrStderr,
}

impl FailureMode {
    /// 不会让调用方失败：p4 起不来时只告警并交出空结果，由调用方保守处理。
    fn is_lenient(self) -> bool {
        matches!(self, Self::Warn | Self::Silent)
    }
}

/// 依退出状态与 stderr 判断一次 p4 调用是否失败，失败时给出可直接上报的消息。
///
/// `success` 与 `status` 分开传是为了让判据脱离真实子进程也能单测：`ExitStatus`
/// 没有跨平台的构造函数。`status` 只在真要报错时才被格式化。
fn p4_failure_message(
    command: &str,
    success: bool,
    status: impl std::fmt::Display,
    stderr: &str,
    mode: FailureMode,
) -> Option<String> {
    // Silent 连退出码都不看：调用方要的裁决就写在 stdout 上，非零退出正是其中一种结论。
    if mode == FailureMode::Silent {
        return None;
    }

    if !success {
        return Some(format!("p4 {command} exited with {status}: {stderr}"));
    }

    // 退出码为 0 也可能是失败，见 [FailureMode::ExitCodeOrStderr]。
    (mode == FailureMode::ExitCodeOrStderr && !stderr.is_empty())
        .then(|| format!("p4 {command} reported: {stderr}"))
}

/// 跑一批 p4 调用中的一片（例如 `p4 stuff a100 a101 ... a198 a199`）。
pub(crate) async fn run_p4_command_slice(
    options: &Options,
    work_dir: &str,
    always_args: &[&'static str],
    batched_args_slice: &[String],
    use_changelist: bool,
    mode: FailureMode,
) -> Result<Vec<String>> {
    // `-c` 是二义的：作为全局选项是客户端，作为命令选项是 changelist。
    let changelist = (use_changelist && options.changelist != 0).then_some(options.changelist);

    // 定位不到 p4（没装、不在 PATH、P4V 目录里也没有）与原来 spawn 拿到 NotFound 是
    // 同一件事，降级策略照旧：strict 调用方必须失败，但文件级过滤这类 lenient 调用方
    // 沿用「p4 报错只告警，不改变已有结果」的契约，返回空结果而不是中断整轮。
    // 真实运行时 p4 缺失早在 fstat/have 阶段就退出，走不到这里，因此这个降级掩盖不了
    // 真实问题。
    let program = match crate::locate::p4_exe() {
        Ok(program) => program,
        Err(reason) if mode.is_lenient() => {
            warn_p4_could_not_start(always_args[0], &reason);
            return Ok(Vec::new());
        }
        Err(reason) => return Err(reason),
    };

    let (mut cmd, payload) = build_p4_command(
        program,
        work_dir,
        always_args,
        batched_args_slice,
        options.workspace.as_deref(),
        changelist,
    );

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        // 定位到 spawn 之间文件被删掉或换成不可执行这一小段窗口，以及其它启动失败。
        // 上面那道 locate 已经拦住了「根本没装」，这里只剩真正的启动问题。
        Err(error) if mode.is_lenient() && error.kind() == io::ErrorKind::NotFound => {
            warn_p4_could_not_start(always_args[0], &error);
            return Ok(Vec::new());
        }
        Err(error) => return Err(error.into()),
    };

    let P4Pipes {
        arguments,
        stdout,
        stderr,
    } = take_p4_pipes(&mut child, payload)?;

    // 三条管道必须并发推：只读 stdout 时，塞满 stderr 管道的子进程会永远阻塞；
    // 参数那一侧同理——写不完的大载荷会把 p4 堵在读取上，而它又在等我们读输出。
    let write_args = write_p4_arguments(arguments);
    let read_stdout = read_p4_lines(stdout);
    let read_stderr = read_p4_stderr(stderr);

    let (_, lines, stderr_bytes) = futures::join!(write_args, read_stdout, read_stderr);
    let lines = lines?;

    let status = child.status().await?;
    // 非零状态不一定说明输出不可用：`p4 ignores` 有没有匹配都返回 0，其他命令的
    // 普通 warning 也可能带非零状态。但目录级剪枝不能拿半截输出做判断，严格时直接失败。
    let stderr_text = String::from_utf8_lossy(&stderr_bytes);
    if let Some(message) = p4_failure_message(
        always_args[0],
        status.success(),
        status,
        stderr_text.trim(),
        mode,
    ) {
        if mode.is_lenient() {
            eprintln!("Warning: {message}");
        } else {
            bail!("{message}");
        }
    }

    Ok(lines)
}

/// 逐行读 p4 的输出流，每行按解析好的字符集解码。
///
/// 逐行读让大响应的原始字节不必整份缓冲，但**并没有**把结果流式交给调用方：解码后的每一行
/// 都攒在内存里，返回的 `Vec` 装着整个响应。所以它只适合响应规模可控的命令——转交批次、
/// `p4 ignores`、`p4 add -n`、`p4 where`；`p4 fstat` 那种几千万行的响应走 `p4/fstat.rs`
/// 里边读边解析的 `FstatParser`，不经过这里。
pub(crate) async fn read_p4_lines(stream: async_process::ChildStdout) -> io::Result<Vec<String>> {
    let encoding = p4_encoding();
    let mut reader = futures::io::BufReader::with_capacity(READ_BUFFER_SIZE, stream);
    let mut raw = Vec::new();
    let mut result = Vec::new();
    let mut is_first_line = true;

    loop {
        raw.clear();
        if reader.read_until(b'\n', &mut raw).await? == 0 {
            break;
        }

        // BOM 只会出现在流的最开头。
        let line = if is_first_line {
            is_first_line = false;
            strip_bom(&raw, encoding)
        } else {
            &raw
        };

        result.push(
            decode_p4_bytes(trim_line_ending(line), encoding)
                .0
                .into_owned(),
        );
    }

    Ok(result)
}

/// 收完 p4 的 stderr。它只在退出状态非零时才被读作错误消息，所以这里只管把字节收干净
/// （管道不读完，子进程可能永远阻塞在写上）。
pub(crate) async fn read_p4_stderr(stream: ChildStderr) -> Vec<u8> {
    let mut buffer = Vec::new();
    let mut stream = stream;
    let _ = stream.read_to_end(&mut buffer).await;
    buffer
}

/// 一次批量调用最多同时跑多少个 p4 进程：它就是 [`run_p4_command_batched`] 里
/// `buffered(..)` 的宽度，`run_p4_fstat_batched` 也用同一个值。这是**单次调用**的上限，
/// 不是全局上限：它只管那一次调用的流，别处同时跑的批量调用不在其内。
///
/// 服务端配的是 maxParallel=8，比这更宽的并发只会白白增加进程启动开销与内存压力。
/// 早先不限并发时，同时在飞的 p4.exe 有过几万个。
pub(crate) const MAX_PARALLEL_P4_COMMANDS: usize = 8;

/// 按参数长度把参数切成若干片：每片交给一个 p4 进程，片数也就是并发度。
/// 单个参数超过上限时自己独占一批，而不是切出一个空批。
pub(crate) fn compute_batches(batched_args: &[String]) -> Vec<std::ops::Range<usize>> {
    let mut batches = Vec::new();
    let mut batch_start = 0;
    let mut batch_size = 0;

    for (index, arg) in batched_args.iter().enumerate() {
        if batch_size > 0 && batch_size + arg.len() > ARGUMENT_LENGTH_MAX {
            batches.push(batch_start..index);
            batch_start = index;
            batch_size = 0;
        }

        batch_size += arg.len() + 1;
    }

    if batch_size > 0 {
        batches.push(batch_start..batched_args.len());
    }

    batches
}

/// 把成千上万个参数分批交给 p4：每一批是一个独立的进程，参数从它的 stdin 送进去。
/// `mode` 不是 [FailureMode::Warn] 时任何一批失败都算失败，调用方必须放弃基于输出的判断。
pub(crate) async fn run_p4_command_batched(
    options: &Options,
    work_dir: &str,
    always_args: &[&'static str],
    batched_args: &[String],
    use_changelist: bool,
    mode: FailureMode,
) -> Result<Vec<String>> {
    let batches = compute_batches(batched_args);

    sayln!(
        "      Running \"p4 {}\" with {} batches.",
        always_args[0],
        batches.len()
    );

    // `buffered` 一次最多轮询 MAX_PARALLEL_P4_COMMANDS 个 future。子进程仍然并行跑：
    // async-process 把管道读取派给阻塞线程池，所以这里不需要 task::spawn，
    // 也就不需要当初不限并发时、为满足借用检查而写的那套 'static transmute。
    let mut slices = futures::stream::iter(batches)
        .map(|range| {
            run_p4_command_slice(
                options,
                work_dir,
                always_args,
                &batched_args[range],
                use_changelist,
                mode,
            )
        })
        .buffered(MAX_PARALLEL_P4_COMMANDS);

    let mut results = Vec::new();
    let mut first_error: Option<Error> = None;

    // 失败之后仍把每一批收完，与「先跑完所有批次再报告」的旧行为一致。中途 drop 掉流会
    // 取消在途批次，留下「部分应用、且无从判断哪些落地」的写命令。
    while let Some(slice) = slices.next().await {
        match slice {
            Ok(lines) if first_error.is_none() => results.extend(lines),
            // 既然已经在报告失败了，再攒结果没有意义。
            Ok(_) => {}
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }

    match first_error {
        Some(e) => Err(e),
        None => Ok(results),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;

    use async_global_executor as task;
    use clap::Parser;
    use encoding_rs::{UTF_8, WINDOWS_1252};

    use crate::prune::IGNORES_ARGS;

    #[test]
    fn empty_arguments_produce_no_batches() {
        assert!(compute_batches(&[]).is_empty());
    }

    /// p4 按命令字符集解码 stdin 上的参数（实测：非法字节会换来
    /// `No Translation for parameter`），所以载荷必须用同一个字符集编码。
    /// 这正是整个改动的要点——名字挂在命令行上会被 Windows 的 ANSI 代码页转换吃掉。
    #[test]
    fn arguments_are_encoded_in_the_p4_charset_one_per_line() {
        let args = vec!["使用说明.txt".to_owned(), "readme.txt".to_owned()];

        let expected = [CHINESE_NAME_UTF8, b"\nreadme.txt\n"].concat();
        let (payload, unrepresentable) = argument_payload(&args, UTF_8);
        assert_eq!(payload, expected);
        assert_eq!(unrepresentable, 0);

        // winansi 里没有汉字：退化成数字引用，而不是丢字节或 panic。
        // 这种配置下 p4 本来也认不出这个名字，让它去报「找不到文件」比猜一个名字诚实；
        // 数出来的那一条会让调用方提醒用户一次。
        let (fallback, _, had_errors) = WINDOWS_1252.encode("使用说明.txt");
        assert!(had_errors, "汉字在 winansi 里没有对应字符");
        let (payload, unrepresentable) = argument_payload(&args, WINDOWS_1252);
        assert_eq!(payload, [fallback.as_ref(), b"\nreadme.txt\n"].concat());
        assert_eq!(unrepresentable, 1, "只有中文名写不进 winansi");
    }

    /// Windows 上非 ASCII 的名字不能挂在 p4 命令行上：ANSI 那一遍会把它变成 `?`，
    /// 而 `?` 能匹零个字符，两遍解析的参数个数就对不上，p4 报
    /// `Argument parsing ambiguity.` 并以 -1 退出，整批查询作废。
    /// Unix 没有这道转换，照常交出去。
    #[test]
    fn non_ascii_paths_are_kept_off_the_windows_command_line() {
        assert!(command_line_safe(r"C:\ws\readme.txt"));
        assert!(command_line_safe("/ws/readme.txt"));

        assert_eq!(command_line_safe(r"C:\ws\使用说明.txt"), !cfg!(windows));
        assert_eq!(command_line_safe("/ws/使用说明.txt"), !cfg!(windows));
    }

    /// 两半各自保持原顺序。交不出去的那半不是被丢掉，而是留给 `p4 add -n` 的补充判据。
    #[test]
    fn split_command_line_paths_separates_what_p4_cannot_read() {
        let paths = vec![
            r"C:\ws\readme.txt".to_owned(),
            r"C:\ws\使用说明.txt".to_owned(),
            r"C:\ws\build".to_owned(),
        ];

        let (ready, unreadable) = split_command_line_paths(&paths);

        if cfg!(windows) {
            assert_eq!(ready, [paths[0].clone(), paths[2].clone()]);
            assert_eq!(unreadable, [paths[1].clone()]);
        } else {
            assert_eq!(ready, paths);
            assert!(unreadable.is_empty());
        }
    }

    /// 接口消费路径的迭代器：调用方（例如 [`crate::workspace::apply_file_ignores`]）从
    /// 自己的结构体里逐个借出 `&String` 即可，不必先克隆出一个中间 `Vec`——每个路径只在
    /// split 里克隆一次。两半同样各自保持原顺序。
    #[test]
    fn split_command_line_paths_consumes_a_borrowing_iterator() {
        struct Entry {
            path: String,
        }

        let entries: Vec<Entry> = [r"C:\ws\readme.txt", r"C:\ws\使用说明.txt", r"C:\ws\build"]
            .into_iter()
            .map(|path| Entry {
                path: path.to_owned(),
            })
            .collect();

        let (ready, unreadable) = split_command_line_paths(entries.iter().map(|entry| &entry.path));

        let paths: Vec<String> = entries.iter().map(|entry| entry.path.clone()).collect();
        if cfg!(windows) {
            assert_eq!(ready, [paths[0].clone(), paths[2].clone()]);
            assert_eq!(unreadable, [paths[1].clone()]);
        } else {
            assert_eq!(ready, paths);
            assert!(unreadable.is_empty());
        }

        // 空集合照样给回两个空半边，调用方不必自己特判。
        let none: Vec<String> = Vec::new();
        let (empty_ready, empty_unreadable) = split_command_line_paths(&none);
        assert!(empty_ready.is_empty() && empty_unreadable.is_empty());
    }

    /// 没有参数时不能加 `-x -`：那会让 p4 去读一个空的 stdin，一个参数都拿不到。
    #[test]
    fn empty_arguments_do_not_switch_to_stdin() {
        let mut cmd = Command::new("p4");

        assert!(feed_arguments_via_stdin(&mut cmd, &[]).is_none());
        assert!(
            cmd.get_args().next().is_none(),
            "空参数不该往命令行上加东西"
        );
    }

    fn argv_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// 参数走 stdin 时，`-x -b` 必须排在命令之前，命令行上不能再出现那些路径
    /// （重复一遍就等于把它们又送回 ANSI 代码页去转换）。
    #[test]
    fn build_p4_command_moves_paths_to_stdin_ahead_of_the_command() {
        let paths = vec!["使用说明.txt".to_owned(), "b.txt".to_owned()];

        let (cmd, payload) = build_p4_command(
            Path::new("p4"),
            "/ws",
            &["edit"],
            &paths,
            Some("ws"),
            Some(7),
        );

        assert_eq!(
            argv_of(&cmd),
            ["-x", "-", "-b", "2", "-c", "ws", "edit", "-c", "7"]
        );
        assert!(payload.is_some(), "这一批的参数该从 stdin 走");
    }

    /// `p4 ignores` 不认 `-x`（见 [`takes_paths_from_stdin`]）：路径留在命令行上，
    /// 载荷必须是空的——给它开一条没人读的 stdin，p4 会连命令行上的参数一起忽略掉。
    #[test]
    fn build_p4_command_keeps_ignores_paths_on_the_command_line() {
        let paths = vec!["./a.txt".to_owned()];

        let (cmd, payload) =
            build_p4_command(Path::new("p4"), "/ws", &IGNORES_ARGS, &paths, None, None);

        assert_eq!(argv_of(&cmd), ["ignores", "-i", "./a.txt"]);
        assert!(payload.is_none());
    }

    #[test]
    fn splits_batches_within_the_argument_limit() {
        let small = vec!["a".to_string(), "b".to_string()];
        assert_eq!(compute_batches(&small), vec![0..2]);

        // 超过上限的参数独占一批。这里过去还会多切出一个空批，
        // 那个空批会以零参数去跑 `p4 ignores`。
        let huge = vec!["x".repeat(ARGUMENT_LENGTH_MAX + 1)];
        assert_eq!(compute_batches(&huge), vec![0..1]);
    }

    #[test]
    fn batches_cover_every_argument_exactly_once() {
        let args: Vec<String> = (0..1000)
            .map(|i| format!("path/to/file{}.txt", i))
            .collect();
        let batches = compute_batches(&args);

        let mut covered = Vec::new();
        for range in &batches {
            assert!(!range.is_empty(), "batches must never be empty");
            covered.extend(range.clone());
        }

        assert_eq!(covered, (0..args.len()).collect::<Vec<_>>());
    }

    /// 边界是「加进来正好等于上限」不切、再多一个字符就切。预算按 `arg.len() + 1`
    /// 累计（含分隔空格），所以算式里要带上那个 1。
    #[test]
    fn an_argument_that_fills_the_limit_exactly_is_not_split() {
        let args = vec!["a".repeat(100), "b".repeat(ARGUMENT_LENGTH_MAX - 101)];

        // 101 + 30618 == 30719 == ARGUMENT_LENGTH_MAX，不满足 `>`，所以留在同一批。
        assert_eq!(compute_batches(&args), vec![0..2]);
    }

    #[test]
    fn one_character_over_the_limit_starts_a_new_batch() {
        let args = vec!["a".repeat(100), "b".repeat(ARGUMENT_LENGTH_MAX - 100)];

        // 101 + 30619 == 30720 > ARGUMENT_LENGTH_MAX。
        assert_eq!(compute_batches(&args), vec![0..1, 1..2]);
    }

    /// 上一批正好填满之后，下一个参数必须自己开一批。
    #[test]
    fn a_filled_batch_is_closed_before_the_next_argument() {
        let args = vec![
            "a".repeat(100),
            "b".repeat(ARGUMENT_LENGTH_MAX - 101),
            "c".to_string(),
        ];

        assert_eq!(compute_batches(&args), vec![0..2, 2..3]);
    }

    #[test]
    fn respects_the_argument_limit_per_batch() {
        let args: Vec<String> = (0..500)
            .map(|i| format!("{}{}", "d".repeat(200), i))
            .collect();

        for range in compute_batches(&args) {
            // 每个参数贡献自己的长度，外加一个分隔空格。
            let size: usize = args[range].iter().map(|arg| arg.len() + 1).sum();
            assert!(
                size <= ARGUMENT_LENGTH_MAX,
                "batch exceeds the command line limit"
            );
        }
    }

    #[test]
    fn strict_p4_queries_reject_failed_output() {
        let tree = TempTree::new("p4-strict");

        if !p4_available() {
            eprintln!("skipping: p4 is not available");
            return;
        }

        let work_dir = tree.root.to_string_lossy().to_string();
        let options = Options::parse_from(["p4delta", "-w", "p4delta-test"]);
        let unknown = ["definitely-not-a-p4-command"];

        // 旧行为（文件级过滤）：非零状态只告警，仍然返回已经拿到的输出
        let lenient = task::block_on(run_p4_command_slice(
            &options,
            &work_dir,
            &unknown,
            &[],
            false,
            FailureMode::Warn,
        ));
        assert!(
            lenient.is_ok(),
            "lenient mode must keep the warning-only behavior"
        );

        // 新行为（目录级剪枝）：非零状态必须失败，半截输出不能变成剪枝计划
        let strict = task::block_on(run_p4_command_slice(
            &options,
            &work_dir,
            &unknown,
            &[],
            false,
            FailureMode::ExitCode,
        ));
        assert!(
            strict.is_err(),
            "a failed p4 query must not be usable for pruning"
        );
    }

    /// p4 对**逐文件**错误返回的退出码是 0（实测 `no permission for operation on file(s).`
    /// 与 `file(s) not on client.` 都是 exit 0）。判据必须把「退出码 0 但 stderr 有输出」
    /// 也算失败，否则会出现「一个文件都没打开，却打印 Applied N changes /
    /// Inconsistencies fixed.」——用户报的就是这一例。
    #[test]
    fn stderr_output_fails_a_call_that_p4_exited_zero_on() {
        let all_modes = [
            FailureMode::Warn,
            FailureMode::ExitCode,
            FailureMode::ExitCodeOrStderr,
        ];

        // 退出码 0 且 p4 没说话：成功。
        for mode in all_modes {
            assert_eq!(
                p4_failure_message("edit", true, "exit status: 0", "", mode),
                None
            );
        }

        // 退出码 0，但 p4 把逐文件错误写到了 stderr：只有改状态命令用的判据拦得住。
        let reported = p4_failure_message(
            "edit",
            true,
            "exit status: 0",
            "E:\\ws\\a.js - no permission for operation on file(s).",
            FailureMode::ExitCodeOrStderr,
        )
        .expect("exit 0 with stderr output must be a failure");
        assert_eq!(
            reported,
            "p4 edit reported: E:\\ws\\a.js - no permission for operation on file(s)."
        );

        // 转交 p4 reconcile/clean 的兜底路径必须放过同样的形状：
        // p4 对「无事可做」也往 stderr 写，那是正常结果而不是失败。
        assert_eq!(
            p4_failure_message(
                "reconcile",
                true,
                "exit status: 0",
                "E:\\ws\\a.js - no file(s) to reconcile.",
                FailureMode::ExitCode,
            ),
            None
        );

        // 非零退出：两种严格判据都失败，消息里带退出状态与 p4 原文。
        for mode in [FailureMode::ExitCode, FailureMode::ExitCodeOrStderr] {
            let message = p4_failure_message("edit", false, "exit status: 1", "boom", mode)
                .expect("non-zero exit must be a failure");
            assert_eq!(message, "p4 edit exited with exit status: 1: boom");
        }

        // 宽松模式也拿得到消息，只是调用方把它降级成一行 warning，不中断整轮。
        assert!(
            p4_failure_message("edit", false, "exit status: 1", "boom", FailureMode::Warn)
                .is_some(),
            "Warn 模式仍要给出消息，由调用方决定怎么处置"
        );
    }
}
