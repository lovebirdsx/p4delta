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
use crate::json::{P4Command, count_p4_command, say_phase, sayln};
use crate::model::HaveRecord;
use crate::p4::marshal::{MarshalRecord, MarshalRecordReader, MarshalStreamParser};

// argv 路径（ignores）保留原命令行预算；stdin 仅借它计算原有并发宽度，载荷另设上限。
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
    run_p4_have_with(crate::locate::p4_exe()?, options, work_dir, specs).await
}

/// [`run_p4_have`] 的本体，p4 可执行文件由调用方给：生产路径传定位结果，
/// 测试传受控的假子进程，让「三条管道并发推」的回归不必依赖机器上有没有 p4
/// （`p4_exe()` 有 OnceLock 缓存，测试进程里也换不掉）。
async fn run_p4_have_with(
    program: &Path,
    options: &Options,
    work_dir: &str,
    specs: &[String],
) -> Result<HashMap<String, HaveRecord>> {
    sayln!("   Querying file sync timestamps.");
    let start_time = Instant::now();

    let (mut cmd, payload) = build_p4_command(
        program,
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
    // 起来了才算一次调用，见 [`count_p4_command`]；上面那个 `?` 失败时早退，不会记账。
    count_p4_command(P4Command::Have);
    let P4Pipes {
        arguments,
        stdout,
        stderr,
    } = take_p4_pipes(&mut child, payload)?;

    // 三条管道必须并发推，理由与 [`run_p4_marshal_slice`] 相同：先写完 stdin 再读输出的话，
    // 一份塞满 stdout 管道的响应会把子进程堵在写上，于是它不来读 stdin，我们这边
    // 大载荷的 `write_all` 也到不了头——两条管道互相等，整轮挂死。
    let write_args = write_p4_arguments(arguments);

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

    let (_, records, stderr_bytes) = futures::join!(write_args, read_stdout, read_stderr);
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
/// 就正常，`ignores -v` 反倒认 `-x`）。整个 `ignores` 家族一律按不认处理，`-i -v` 也一样：
/// 少记一条「哪个变体认 `-x`」的例外更不容易出错，而这条家族的路径参数本来就该挂在命令行上
/// ——回显要逐字核对，走哪个通道不是可以随便换的细节（见
/// `crate::workspace::query_ignores_verbose`）。
///
/// 代价有两条，都指向同一个后果——非 ASCII 的路径名在 Windows 上要走命令行：
///
/// - 这些名字只能挂在命令行上，在 ANSI 代码页表达不了它们的机器上会被转换吃掉（p4 侧的硬限制）；
/// - `p4 ignores` 因此问不出这些名字的忽略状态。两条补救各按自己的口径走：文件级过滤改用同样
///   认 stdin 的 `p4 add -n` 补一个肯定结论（见 `crate::workspace::query_ignored_refusals`），
///   删除保护改用命令行上的 `p4 ignores -i -v` 并逐字核对回显（见
///   `crate::workspace::query_ignores_verbose`）。
///   目录级剪枝没有补救，那些目录照常完整扫描——只损失性能，不改变结论。
fn takes_paths_from_stdin(always_args: &[&str]) -> bool {
    !always_args.contains(&"ignores")
}

/// 该路径**静态**判定为不能挂在 p4 命令行上。
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
/// 代码页里表示得出来的非 ASCII 字符同样按不安全处理：p4 拿到的是命令行转写后的字节，
/// 却按命令字符集（P4COMMANDCHARSET，缺省随 P4CHARSET）解释，两边是否一致取决于**跑这
/// 一轮的机器**的 ACP；ASCII 是唯一在两处都必然同一串字节的集合。
///
/// 这是一道**静态**的分组，说的是「按平台与字符集能确定的风险」，不是「Windows 上非 ASCII
/// 一定不可用」：ACP 能表示这些字符的机器上（中文 Windows 默认 936）命令行其实是无损的，
/// 但这一点只有运行时才验得出来。唯一必须走命令行、又想要非 ASCII 的调用点是删除保护
/// （`p4 ignores` 不认 `-x`，见 [`takes_paths_from_stdin`]），它因此在运行时逐字核对回显，
/// 见 `crate::workspace::query_ignores_verbose`。
///
/// Unix 上没有这道转换，路径照常交给 p4。
pub(crate) fn command_line_safe(path: &str) -> bool {
    !cfg!(windows) || path.is_ascii()
}

/// 把一批路径劈成「静态判定能交给 p4 命令行的」与「交不出去的」两半，顺序各自保持。
///
/// 消费路径的迭代器而不是先要一个 `&[String]`：调用方的路径常散在自己的结构体里
/// （[`crate::workspace::apply_file_ignores`] 就是从 `WorkspaceFile` 里逐个借出），
/// 这样它们不必先克隆出一个中间 `Vec`——每个路径只在这里克隆一次，就是返回的两个
/// `Vec` 里各归其位的那一份。
///
/// 后半不是终点，但两个调用点的善后差得很远，一句通用文案说不准，所以这里只划分、
/// 不报告——报告交给各自的调用方，在 `-v` 下按自己的口径打印：
///
/// - 文件级过滤有补救：`p4 ignores` 不认 stdin，但 `p4 add -n` 认，于是那些名字改走
///   那条字节无损的通道补判（见 [`crate::workspace::query_ignored_refusals`]）。
/// - 目录级剪枝没有补救：交不出去的目录照常完整扫描，只损失性能。
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
///
/// 批次按 stdin 的硬上限规划（目标 1 批），不再走旧的命令行长度预算——预演下发的
/// 同一批规格数量可达几万条，旧预算会把它拆成二十几个串行进程。
pub(crate) async fn run_p4_marshal_batched(
    options: &Options,
    work_dir: &str,
    command: &str,
    category: P4Command,
    always_args: &[&'static str],
    batched_args: &[String],
) -> Result<MarshalRun> {
    let batches = plan_stdin_batches(batched_args, 1);
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
            category,
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
    category: P4Command,
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
    count_p4_command(category);
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
    /// （见 `crate::workspace::query_ignored_refusals`），报成警告只会误导。
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
    if matches!(mode, FailureMode::Silent) {
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
///
/// `command` 是**调用点显式给出的固定类别**（[`P4Command`]），只用于计数与分类上报；
/// 不从 `always_args` 的 argv 反推——转交路径的参数以 `-Mj` 打头，反推会把整批记错。
pub(crate) async fn run_p4_command_slice(
    options: &Options,
    work_dir: &str,
    command: P4Command,
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
    // 只有 `Ok` 那一支会走到这里：起不来的进程不占一次调用（见 [`count_p4_command`]）。
    count_p4_command(command);

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
    let lengths: Vec<usize> = batched_args.iter().map(|arg| arg.len()).collect();
    command_line_ranges(&lengths)
}

/// 命令行分批的**唯一**算式：`lengths` 是每个参数在命令行上占的字节（不含分隔符）。
/// 判断一格用不含分隔符的长度、累计一格含一个分隔符——这是 [`compute_batches`] 一直
/// 以来的口径，规划器与诊断侧对删除候选的估算都从它出发。算式只留一处：重写一份近似
/// 迟早会在边界上漂，而估算一旦偏了，拿它判断「缩短路径表示值不值得」就是拿错数当依据。
pub(crate) fn command_line_ranges(lengths: &[usize]) -> Vec<std::ops::Range<usize>> {
    let mut batches = Vec::new();
    let mut batch_start = 0;
    let mut batch_size = 0;

    for (index, &len) in lengths.iter().enumerate() {
        if batch_size > 0 && batch_size + len > ARGUMENT_LENGTH_MAX {
            batches.push(batch_start..index);
            batch_start = index;
            batch_size = 0;
        }

        batch_size += len + 1;
    }

    if batch_size > 0 {
        batches.push(batch_start..lengths.len());
    }

    batches
}

/// 走 stdin（`-x -`）的批次的两条硬上限。stdin 不受 Win32 命令行 32767 字符的约束，
/// 继续按 [`ARGUMENT_LENGTH_MAX`] 预算切批只会让一次调用拆出几十个进程；256 KiB
/// 让典型的大批量从二十几批降到个位数。条数上限兜的是另一种失衡——全是短路径时
/// 按字节切不出批。两者都只影响分片粒度（进程数），不影响语义。
///
/// 预算是**显式挂在调用上**的（见 [`StdinBudget`]）而不是所有调用点共用一个常量：
/// `p4 sync -f` 的两个动作组一次要下发的规格比别处大得多（同一个工作区实测 update 4,544
/// 条 + restore 85,074 条 + delete 84,941 条）。条数上限 8192 已经给这两组定下**至少 11 批**
/// 的下界，容量 256 KiB → 1 MiB 能把它压到多少要看规格的平均长度（两档都装不出 8 批：
/// 8 是并发目标，不是批数的可达下界）。放大容量是**只给这两条路**的额度——一批失败要重来的
/// 量、单个 p4 进程的内存与载荷编码开销都跟着涨，不该顺手给别处；它也不是耗时承诺。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StdinBudget {
    /// 单批参数载荷的字节上限。
    payload_max: usize,

    /// 单批参数条数上限。两种预算共用同一个值：它兜的是「全是短路径」那一种失衡，
    /// 与载荷预算无关。
    spec_max: usize,
}

impl StdinBudget {
    /// 默认预算：256 KiB / 8192 条。除强制修复的 `p4 sync` 外，所有 stdin 调用点都用它。
    pub(crate) const DEFAULT: Self = Self {
        payload_max: 256 * 1024,
        spec_max: 8192,
    };

    /// 强制修复（`--sync --force`）下 `sync_to_target` / `delete_from_workspace` 的预算：
    /// 1 MiB / 8192 条。
    pub(crate) const FORCE_SYNC: Self = Self {
        payload_max: 1024 * 1024,
        spec_max: 8192,
    };
}

/// 一个参数在 stdin 载荷里占的字节：按 [`argument_payload`] 的实际编码算（编码后的
/// 参数 + 一个换行）。非 ASCII 不用字符数近似——多字节编码与字符集的替代文本
/// （`&#NNNN;`）都会让真实载荷偏离字符数，按字符数切批会切出超载的进程输入。
fn stdin_argument_len(arg: &str, encoding: &'static Encoding) -> usize {
    let (bytes, _, _) = encoding.encode(arg);
    bytes.len() + 1
}

/// 把一段（至少两个参数的）连续 range 对半切成两半：前半按字节装到总长的一半为止，
/// 但至少给后半留一个参数。
fn split_range_in_half(
    lengths: &[usize],
    range: std::ops::Range<usize>,
) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
    let total: usize = lengths[range.clone()].iter().sum();
    let mut acc = 0usize;
    let mut split = range.start + 1;

    for (index, &len) in lengths
        .iter()
        .enumerate()
        .take(range.end - 1)
        .skip(range.start)
    {
        acc += len;
        split = index + 1;
        if acc * 2 >= total {
            break;
        }
    }

    (range.start..split, split..range.end)
}

/// 按「批容量不超过 `capacity`」装填的贪心切分，条数上限与超载参数独占照旧生效。
/// `capacity` 越大批数越少，批数随它单调不增——这是对它二分的依据。
fn capacity_greedy_ranges(
    lengths: &[usize],
    capacity: usize,
    budget: StdinBudget,
) -> Vec<std::ops::Range<usize>> {
    let limit = capacity.min(budget.payload_max);
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut payload = 0usize;
    let mut specs = 0usize;

    for (index, &len) in lengths.iter().enumerate() {
        if specs > 0 && (payload + len > limit || specs == budget.spec_max) {
            ranges.push(start..index);
            start = index;
            payload = 0;
            specs = 0;
        }

        payload += len;
        specs += 1;
    }

    if specs > 0 {
        ranges.push(start..lengths.len());
    }

    ranges
}

/// stdin 批次规划：在硬上限内把参数切成若干连续、不重不漏的 range。
///
/// `min_batches` 是**并发度目标**：并行路径传该次调用在旧预算（[`compute_batches`]）下的
/// 实际批数与 [`MAX_PARALLEL_P4_COMMANDS`] 的较小者——256 KiB 预算下最少批数往往远小于
/// 它，不补足的话一次调用的并发会从 8 掉到 1；串行路径传 1，批数就是硬约束下的最少批数。
///
/// 「总字节除以上限」只是批数的下界：长参数混短参数、条数上限、超载参数都会让同样
/// 的字节切出更多的批，这里不做「ceil 后精确装满」的假设。硬下界之上的批数靠二分
/// 「批容量」取齐：容量越小批数越多，能给出 ≤ 目标批数的最小容量同时把**最大批的
/// 字节**压到最优——并发下墙钟跟着最大批走，这正是要均衡的量。容量最优的批数仍不足
/// 目标时（长短悬殊的参数装不出更多批），把最大的批对半补足；对半是子集切分，载荷、
/// 条数与「超载参数独占」在子集上原样成立。目标批数封顶在参数个数——两个参数不为
/// 凑并发拆成八批。
pub(crate) fn plan_stdin_batches(
    batched_args: &[String],
    min_batches: usize,
) -> Vec<std::ops::Range<usize>> {
    plan_stdin_batches_with(batched_args, min_batches, StdinBudget::DEFAULT)
}

/// 同 [`plan_stdin_batches`]，载荷与条数上限由调用方给。两个预算复用这**一份**规划实现：
/// 差别只在容量，不重不漏、超载独占、并发补足这些不变量对两套预算必须同样成立。
pub(crate) fn plan_stdin_batches_with(
    batched_args: &[String],
    min_batches: usize,
    budget: StdinBudget,
) -> Vec<std::ops::Range<usize>> {
    if batched_args.is_empty() {
        return Vec::new();
    }

    plan_stdin_batches_with_lengths(&stdin_argument_lengths(batched_args), min_batches, budget)
}

/// 一段参数的载荷长度表：每个参数在 stdin 里实际占的字节（编码后的参数 + 一个换行）。
/// 规划与诊断共用它，免得同一批参数被编码两遍。
fn stdin_argument_lengths(batched_args: &[String]) -> Vec<usize> {
    let encoding = p4_command_encoding();
    batched_args
        .iter()
        .map(|arg| stdin_argument_len(arg, encoding))
        .collect()
}

/// [`plan_stdin_batches_with`] 的本体，长度表由调用方给（诊断要复用它）。
fn plan_stdin_batches_with_lengths(
    lengths: &[usize],
    min_batches: usize,
    budget: StdinBudget,
) -> Vec<std::ops::Range<usize>> {
    if lengths.is_empty() {
        return Vec::new();
    }

    let target = min_batches.min(lengths.len());

    let fewest = capacity_greedy_ranges(lengths, budget.payload_max, budget);
    if fewest.len() >= target {
        return fewest;
    }

    // 二分最小的批容量，使按它装填的批数 ≤ 目标。搜索下界是最大的单参数：容量再小
    // 连一个参数都装不下，只会让每个参数独占一批。
    let mut low = lengths.iter().copied().max().unwrap_or(1);
    let mut high = budget.payload_max.max(low);
    while low < high {
        let mid = low + (high - low) / 2;
        if capacity_greedy_ranges(lengths, mid, budget).len() <= target {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    let mut ranges = capacity_greedy_ranges(lengths, low, budget);

    // 容量最优也装不出目标批数时补批：反复把字节最大的批对半。
    while ranges.len() < target {
        let mut biggest: Option<(usize, usize)> = None;
        for (index, range) in ranges.iter().enumerate() {
            if range.end - range.start <= 1 {
                continue;
            }
            let bytes = lengths[range.start..range.end].iter().sum::<usize>();
            if biggest.is_none_or(|(_, known)| bytes > known) {
                biggest = Some((index, bytes));
            }
        }
        // 批数 < target ≤ 参数个数 时必有一批装着至少两个参数，否则批数早已等于
        // 参数个数。`break` 是给这个不变量被破坏时的保险，正常到不了。
        let Some((index, _)) = biggest else {
            break;
        };

        let range = ranges.remove(index);
        let (head, tail) = split_range_in_half(lengths, range);
        ranges.insert(index, tail);
        ranges.insert(index, head);
    }

    ranges
}

/// 一次批量调用的分片计划。
struct BatchPlan {
    /// 连续、不重不漏的批：每一批是 `batched_args` 上的一段。
    ranges: Vec<std::ops::Range<usize>>,

    /// stdin 通道下每个参数在载荷里占的字节。`None` 表示参数走命令行（argv 通道），
    /// 那时「载荷」就是参数自己的长度——规划与诊断共用这一段编码，不重复算两遍。
    stdin_lengths: Option<Vec<usize>>,

    /// 规划时给的并发度目标。
    width_target: usize,
}

/// 一次批量调用的分片：两种参数通道两套预算。stdin 批次按大载荷规划，但并发度维持该次
/// 调用在旧命令行预算下的水平（`min(旧批数, 8)`）——批数掉了就是并发掉了；`p4 ignores`
/// 这类参数留在命令行上的命令仍受 32767 字符约束，继续用 [`compute_batches`]。
fn plan_batches(always_args: &[&str], batched_args: &[String], budget: StdinBudget) -> BatchPlan {
    if takes_paths_from_stdin(always_args) {
        let target = compute_batches(batched_args)
            .len()
            .min(MAX_PARALLEL_P4_COMMANDS);
        let lengths = stdin_argument_lengths(batched_args);
        let ranges = plan_stdin_batches_with_lengths(&lengths, target, budget);

        BatchPlan {
            ranges,
            stdin_lengths: Some(lengths),
            width_target: target,
        }
    } else {
        BatchPlan {
            ranges: compute_batches(batched_args),
            stdin_lengths: None,
            width_target: MAX_PARALLEL_P4_COMMANDS,
        }
    }
}

/// 一行聚合的分批诊断：命令/组别、参数通道、参数总数、载荷字节、容量与条数上限、
/// 批数、单批条数/载荷范围、并发目标。
///
/// **只有聚合数字，没有逐条路径**：几万条候选逐条打印就是刷屏，而这里要回答的只是
/// 「切了几批、每批多大、并发多宽」。它也是下一次「调用数为什么是这么多」的唯一直接
/// 证据——现场的日志只有分类计数，没有载荷分布，反推不出任何结论。
///
/// argv 通道的字节数就是命令行上的字节（每个参数自己的长度 + 一个分隔符），与
/// [`compute_batches`] 用 [`ARGUMENT_LENGTH_MAX`] 做的预算同一口径；那些参数不经过
/// 字符集编码、也不上 stdin，所以不与 stdin 载荷混为一谈。
///
/// `context` 说明这一行属于谁（执行路径是命令名，预演是动作组名）；`executed` 为假时
/// 明确写上「估算、没有启动进程」——预演必须让人分得清哪些数字来自真实下发。
fn say_batch_plan(
    context: &str,
    plan: &BatchPlan,
    batched_args: &[String],
    budget: StdinBudget,
    executed: bool,
) {
    say_phase(
        "batches",
        &batch_plan_detail(context, plan, batched_args, budget, executed),
        None,
    );
}

/// [`say_batch_plan`] 的正文。与打印分开是为了它能被单测直接看：这行字是「调用数为什么
/// 是这么多」的唯一直接证据，它报错字段比报错数字更难发现。
fn batch_plan_detail(
    context: &str,
    plan: &BatchPlan,
    batched_args: &[String],
    budget: StdinBudget,
    executed: bool,
) -> String {
    let stdin = plan.stdin_lengths.is_some();
    let channel = if stdin { "stdin" } else { "argv" };
    let load = |range: &std::ops::Range<usize>| -> usize {
        match &plan.stdin_lengths {
            Some(lengths) => lengths[range.clone()].iter().sum(),
            None => batched_args[range.clone()]
                .iter()
                .map(|arg| arg.len() + 1)
                .sum(),
        }
    };

    let all = 0..batched_args.len();
    let total: usize = load(&all);
    let loads: Vec<usize> = plan.ranges.iter().map(load).collect();
    let specs: Vec<usize> = plan
        .ranges
        .iter()
        .map(|range| range.end - range.start)
        .collect();
    let summarize = |values: &[usize]| -> String {
        match (values.iter().min(), values.iter().max()) {
            (Some(min), Some(max)) => format!("{min}..{max}"),
            _ => "n/a".to_owned(),
        }
    };

    let payload_max = budget.payload_max;
    let spec_max = budget.spec_max;
    let capacity = if stdin {
        format!("{payload_max} byte(s)/{spec_max} spec(s)")
    } else {
        format!("{ARGUMENT_LENGTH_MAX} byte(s)")
    };
    let suffix = if executed {
        String::new()
    } else {
        ", estimated, nothing started".to_owned()
    };

    format!(
        "{context}, {channel} channel, {} argument(s), {total} byte(s), capacity {capacity}, \
         {} batch(es), specs {}, load {} byte(s), concurrency target {} (max \
         {MAX_PARALLEL_P4_COMMANDS} in flight){suffix}",
        batched_args.len(),
        plan.ranges.len(),
        summarize(&specs),
        summarize(&loads),
        plan.width_target,
    )
}

/// 只规划、不启动：预演用它报出各动作组的预计批数。
///
/// 与执行路径共用 [`plan_batches`] 与那行诊断，所以预演报的数就是真跑时该有的批数，
/// 不是另写一套近似。它**一个进程都不起、一次调用都不记**（`--json` 的 `p4Calls`
/// 因此完全不受影响）。
pub(crate) fn say_planned_batches(
    context: &str,
    always_args: &[&str],
    batched_args: &[String],
    budget: StdinBudget,
) {
    let plan = plan_batches(always_args, batched_args, budget);
    say_batch_plan(context, &plan, batched_args, budget, false);
}

/// 把成千上万个参数分批交给 p4：每一批是一个独立的进程，参数从它的 stdin 送进去。
/// `mode` 不是 [FailureMode::Warn] 时任何一批失败都算失败，调用方必须放弃基于输出的判断。
///
/// 这是默认预算（[`StdinBudget::DEFAULT`]）的入口；需要另一套预算的调用点走
/// [`run_p4_command_batched_with_budget`]。
pub(crate) async fn run_p4_command_batched(
    options: &Options,
    work_dir: &str,
    command: P4Command,
    always_args: &[&'static str],
    batched_args: &[String],
    use_changelist: bool,
    mode: FailureMode,
) -> Result<Vec<String>> {
    run_p4_command_batched_with_budget(
        options,
        work_dir,
        command,
        always_args,
        batched_args,
        use_changelist,
        mode,
        StdinBudget::DEFAULT,
    )
    .await
}

/// 同 [`run_p4_command_batched`]，分片预算由调用方给。只有强制修复的 `p4 sync` 两条路会传
/// [`StdinBudget::FORCE_SYNC`]：同一份规划与执行实现，差别只在单批容量。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_p4_command_batched_with_budget(
    options: &Options,
    work_dir: &str,
    command: P4Command,
    always_args: &[&'static str],
    batched_args: &[String],
    use_changelist: bool,
    mode: FailureMode,
    budget: StdinBudget,
) -> Result<Vec<String>> {
    let plan = plan_batches(always_args, batched_args, budget);

    sayln!(
        "      Running \"p4 {}\" with {} batches.",
        always_args[0],
        plan.ranges.len()
    );
    say_batch_plan(
        &format!("p4 {}", always_args[0]),
        &plan,
        batched_args,
        budget,
        true,
    );
    let batches = plan.ranges;

    // `buffered` 一次最多轮询 MAX_PARALLEL_P4_COMMANDS 个 future。子进程仍然并行跑：
    // async-process 把管道读取派给阻塞线程池，所以这里不需要 task::spawn，
    // 也就不需要当初不限并发时、为满足借用检查而写的那套 'static transmute。
    let mut slices = futures::stream::iter(batches)
        .map(|range| {
            run_p4_command_slice(
                options,
                work_dir,
                command,
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

    /// 两套预算的容量，用例里按名字引用比按字段访问短。
    const DEFAULT_PAYLOAD_MAX: usize = StdinBudget::DEFAULT.payload_max;
    const DEFAULT_SPEC_MAX: usize = StdinBudget::DEFAULT.spec_max;
    const FORCE_SYNC_PAYLOAD_MAX: usize = StdinBudget::FORCE_SYNC.payload_max;
    const FORCE_SYNC_SPEC_MAX: usize = StdinBudget::FORCE_SYNC.spec_max;

    #[test]
    fn empty_arguments_produce_no_batches() {
        assert!(compute_batches(&[]).is_empty());
    }

    /// stdin 批次的载荷字节数与 [`argument_payload`] 实际写出的字节一致：多字节编码
    /// 与字符集的替代文本（`&#NNNN;`）都会让真实载荷偏离字符数，planner 按字符数
    /// 切批就会切出超载的进程输入。
    #[test]
    fn stdin_argument_len_matches_the_payload_actually_written() {
        let samples = [
            "//depot/readme.txt",
            "//depot/使用说明.txt",
            "a",
            &"x".repeat(100_000),
        ];
        for arg in samples {
            for encoding in [UTF_8, WINDOWS_1252] {
                let (payload, _) = argument_payload(&[arg.to_owned()], encoding);
                assert_eq!(
                    payload.len(),
                    stdin_argument_len(arg, encoding),
                    "{arg} in {}",
                    encoding.name()
                );
            }
        }

        // 同一个名字在 winansi 里走数字引用，一个汉字膨胀到 8 字节——按字符数近似
        // 会把它算成 1。
        let (utf8_payload, _) = argument_payload(&["//depot/使.txt".to_owned()], UTF_8);
        let (winansi_payload, _) = argument_payload(&["//depot/使.txt".to_owned()], WINDOWS_1252);
        assert!(winansi_payload.len() > utf8_payload.len());
    }

    #[test]
    fn stdin_planning_of_no_arguments_is_no_batches() {
        assert!(plan_stdin_batches(&[], 1).is_empty());
        assert!(plan_stdin_batches(&[], 8).is_empty());
    }

    /// 小输入不膨胀：串行目标下两个短参数就是一批。并发目标高于参数个数时批数封顶在
    /// 参数个数（真实接线里目标来自 `min(旧批数, 8)`，小输入的旧批数是 1，到不了 8）。
    #[test]
    fn stdin_planning_does_not_fan_out_a_tiny_input() {
        let args = vec!["//depot/a.txt".to_owned(), "//depot/b.txt".to_owned()];
        assert_eq!(plan_stdin_batches(&args, 1), vec![0..2]);
        assert_eq!(plan_stdin_batches(&args, 8), vec![0..1, 1..2]);
    }

    /// 单个参数本身超过载荷上限时独占一批：绝不拆参数、也不切出空批。
    /// 前后的短参数各自成批，不与它合并（合不进去）。
    #[test]
    fn an_oversized_stdin_argument_gets_its_own_batch() {
        let huge = "x".repeat(DEFAULT_PAYLOAD_MAX);
        let args = vec!["//depot/a".to_owned(), huge, "//depot/b".to_owned()];

        assert_eq!(plan_stdin_batches(&args, 1), vec![0..1, 1..2, 2..3]);
        // 并发目标也不改变它的独占地位。
        assert_eq!(plan_stdin_batches(&args, 8), vec![0..1, 1..2, 2..3]);
    }

    /// 加进来正好等于载荷上限不切，再多一个字节就切。边界按「参数编码 + 换行」累计。
    #[test]
    fn a_stdin_payload_that_fills_the_limit_exactly_stays_in_one_batch() {
        let half = "x".repeat(DEFAULT_PAYLOAD_MAX / 2 - 1);
        let args = vec![half.clone(), half];
        assert_eq!(plan_stdin_batches(&args, 1), vec![0..2]);

        let args = vec![
            "x".repeat(DEFAULT_PAYLOAD_MAX / 2 - 1),
            "x".repeat(DEFAULT_PAYLOAD_MAX / 2),
        ];
        assert_eq!(plan_stdin_batches(&args, 1), vec![0..1, 1..2]);
    }

    /// 全是短参数时条数上限兜底：8193 条切成 2 批，第一批恰好 8192 条。
    #[test]
    fn stdin_planning_respects_the_spec_count_limit() {
        let args: Vec<String> = (0..DEFAULT_SPEC_MAX + 1)
            .map(|index| format!("//depot/f{index}"))
            .collect();

        let batches = plan_stdin_batches(&args, 1);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].end - batches[0].start, DEFAULT_SPEC_MAX);
        assert_eq!(batches[1].end - batches[1].start, 1);
    }
    /// 串行目标 1：批数就是硬约束下的贪心最少批数，不被拉到 8。
    #[test]
    fn serial_stdin_planning_takes_the_fewest_batches_the_limits_allow() {
        // 每条 17.5KB：旧命令行预算下一条一批，256 KiB 预算下一批装 14 条。
        let args: Vec<String> = (0..60).map(|_| "x".repeat(17_500)).collect();

        assert_eq!(plan_stdin_batches(&args, 1).len(), 5);
    }

    /// 并行目标补足：硬下界只有 5 批时，批容量二分把目标 8 批的最大批字节压到最优，
    /// 并发宽度是该次调用原有的，大载荷不该让它掉下去。
    #[test]
    fn parallel_stdin_planning_fills_up_to_the_target_width_and_balances_bytes() {
        let args: Vec<String> = (0..60).map(|_| "x".repeat(17_500)).collect();
        let total: usize = 60 * 17_501;

        let batches = plan_stdin_batches(&args, 8);
        assert_eq!(batches.len(), 8);

        // 等长参数下「最大批」的下界是 ceil(60/8) = 8 条：再小批数就超过目标。
        // 最优切分应恰好达到这个下界，没有一批吃掉别人的份额。
        let payload_of = |range: &std::ops::Range<usize>| {
            args[range.clone()]
                .iter()
                .map(|arg| arg.len() + 1)
                .sum::<usize>()
        };
        let bytes: Vec<usize> = batches.iter().map(payload_of).collect();
        assert_eq!(
            bytes.iter().copied().max(),
            Some(8 * 17_501),
            "最大批没有压到等长参数的下界：{bytes:?}"
        );
        assert_eq!(bytes.iter().sum::<usize>(), total, "字节守恒：{bytes:?}");
    }

    /// 旧预算下的批数决定并行目标：目标批数不因载荷变大而低于 `min(旧批数, 8)`，
    /// 也不会超过参数个数。
    #[test]
    fn parallel_stdin_planning_never_drops_below_the_legacy_width() {
        // 旧预算（30719 字节）下每条一批：60 批 → 目标 min(60, 8) = 8。
        let args: Vec<String> = (0..60).map(|_| "x".repeat(20_000)).collect();
        let legacy = compute_batches(&args).len();
        assert_eq!(legacy, 60);
        assert_eq!(plan_stdin_batches(&args, legacy.min(8)).len(), 8);

        // 旧预算只有 1 批的小输入：目标 1（= min(1, 8)，真实接线里的形状），新 planner 不放大并发。
        let small = vec!["a".to_owned(), "b".to_owned()];
        assert_eq!(compute_batches(&small).len(), 1);
        assert_eq!(plan_stdin_batches(&small, 1), vec![0..2]);
    }

    /// 任何输入、任何并发目标、**任何一套预算**下：批内连续、拼接恰好覆盖全部参数、每批
    /// 满足该预算的硬上限（超载参数独占是唯一例外）、批数不超过参数个数。
    ///
    /// 两套预算跑同一份断言是刻意的：预算只该改容量，不重不漏、超载独占、条数上限这些
    /// 不变量对 1 MiB 与 256 KiB 必须同样成立——同一份规划实现，差别只在一个数。
    #[test]
    fn stdin_batches_stay_continuous_bounded_and_exhaustive() {
        let mut args: Vec<String> = vec![
            "//depot/使用说明.txt".to_owned(),
            "x".repeat(DEFAULT_PAYLOAD_MAX + 5),
            "x".repeat(DEFAULT_PAYLOAD_MAX / 2),
            String::new(),
            "y".repeat(17_500),
        ];
        args.extend((0..DEFAULT_SPEC_MAX + 7).map(|index| format!("//depot/f{index}")));
        args.push("z".repeat(DEFAULT_PAYLOAD_MAX));

        for budget in [StdinBudget::DEFAULT, StdinBudget::FORCE_SYNC] {
            let payload_max = budget.payload_max;
            let spec_max = budget.spec_max;

            for min_batches in [1usize, 2, 8, 100] {
                let batches = plan_stdin_batches_with(&args, min_batches, budget);

                let mut covered = Vec::new();
                for range in &batches {
                    assert!(!range.is_empty(), "空批：{min_batches}");
                    let specs = range.end - range.start;
                    let payload: usize = args[range.clone()]
                        .iter()
                        .map(|arg| stdin_argument_len(arg, UTF_8))
                        .sum();
                    assert!(
                        payload <= payload_max || (specs == 1 && payload > payload_max),
                        "批超载且不是超载参数独占：{min_batches} {range:?} {payload}"
                    );
                    assert!(specs <= spec_max, "批条数超限：{min_batches} {range:?}");
                    covered.extend(range.clone());
                }
                assert_eq!(
                    covered,
                    (0..args.len()).collect::<Vec<_>>(),
                    "覆盖不完整或乱序：{min_batches}"
                );
                assert!(batches.len() <= args.len());
            }
        }
    }

    /// 两套预算的容量就是这两个数：默认档一个字节都不动（现场与既有基准都靠它），
    /// 强制修复那两条路才拿 1 MiB。条数上限两套相同——它兜的是「全是短路径」那种失衡，
    /// 与载荷预算无关。
    #[test]
    fn the_two_budgets_are_256_kib_and_1_mib() {
        assert_eq!(StdinBudget::DEFAULT.payload_max, 256 * 1024);
        assert_eq!(StdinBudget::FORCE_SYNC.payload_max, 1024 * 1024);
        assert_eq!(StdinBudget::DEFAULT.spec_max, 8192);
        assert_eq!(StdinBudget::FORCE_SYNC.spec_max, 8192);

        // 默认入口 = 显式传默认预算：加预算参数不该顺手改掉任何既有调用点的行为。
        let args: Vec<String> = (0..20_000)
            .map(|index| format!("//depot/f{index}"))
            .collect();
        assert_eq!(
            plan_stdin_batches(&args, 1),
            plan_stdin_batches_with(&args, 1, StdinBudget::DEFAULT)
        );
        // 20,000 条短路径按条数上限切 3 批；载荷预算从 256 KiB 涨到 1 MiB 也翻不动它。
        assert_eq!(
            plan_stdin_batches_with(&args, 1, StdinBudget::DEFAULT).len(),
            3
        );
        assert_eq!(
            plan_stdin_batches_with(&args, 1, StdinBudget::FORCE_SYNC).len(),
            3
        );
    }

    /// 强制修复的预算要解决的正是这一件事：**单个动作组**的载荷超过默认预算的并发宽度
    /// （8 × 256 KiB）时，默认预算给出的批数会溢过宽度，1 MiB 预算仍落在宽度内。
    ///
    /// 夹具必须真的越线：9000 条深路径那种形状**两边都是 8 批**（见
    /// `the_nine_thousand_spec_fixture_cannot_tell_the_two_budgets_apart`），拿它当优化
    /// 证据是错的。这里按现场同形的规格长度（约 103 字节/条）取 24,000 条。
    #[test]
    fn the_force_sync_budget_collapses_a_group_the_default_budget_splits() {
        let specs: Vec<String> = deep_specs(24_000);
        let payload: usize = specs
            .iter()
            .map(|spec| stdin_argument_len(spec, UTF_8))
            .sum();
        assert!(
            payload > 8 * DEFAULT_PAYLOAD_MAX,
            "夹具必须单组越过 8 × 256 KiB：{payload}"
        );

        let default =
            plan_stdin_batches_with(&specs, MAX_PARALLEL_P4_COMMANDS, StdinBudget::DEFAULT);
        let force =
            plan_stdin_batches_with(&specs, MAX_PARALLEL_P4_COMMANDS, StdinBudget::FORCE_SYNC);

        assert!(
            default.len() > MAX_PARALLEL_P4_COMMANDS,
            "默认预算该切出多于并发宽度的批数：{}",
            default.len()
        );
        assert_eq!(
            force.len(),
            MAX_PARALLEL_P4_COMMANDS,
            "1 MiB 预算该刚好铺满 8 个进程：{}",
            force.len()
        );
        for range in &force {
            let bytes: usize = specs[range.clone()]
                .iter()
                .map(|spec| stdin_argument_len(spec, UTF_8))
                .sum();
            assert!(
                bytes <= FORCE_SYNC_PAYLOAD_MAX,
                "{range:?} 超过 1 MiB：{bytes}"
            );
            assert!(range.end - range.start <= FORCE_SYNC_SPEC_MAX, "{range:?}");
        }

        // 两套预算切的是同一批参数，一个都不多、一个都不少。
        assert_eq!(
            default
                .iter()
                .map(|range| range.end - range.start)
                .sum::<usize>(),
            specs.len()
        );
    }

    /// 现有 9000 条夹具（update 6000 + restore 3000）**区分不了两套预算**：两边都是 8 批。
    ///
    /// 钉住这一点是为了下次别再把它的数字当这次改动的证据——它证明的是并发补足（0.1.13
    /// 那一版就有的行为），不是 1 MiB 预算带来的批数下降。新证据必须来自真正越线的夹具。
    #[test]
    fn the_nine_thousand_spec_fixture_cannot_tell_the_two_budgets_apart() {
        let update: Vec<String> = deep_specs(6_000);
        let restore: Vec<String> = deep_specs(3_000);

        for args in [&update, &restore] {
            let target = compute_batches(args).len().min(MAX_PARALLEL_P4_COMMANDS);
            assert_eq!(
                target, MAX_PARALLEL_P4_COMMANDS,
                "夹具要够宽才谈得上并发补足"
            );
            assert_eq!(
                plan_stdin_batches_with(args, target, StdinBudget::DEFAULT).len(),
                MAX_PARALLEL_P4_COMMANDS
            );
            assert_eq!(
                plan_stdin_batches_with(args, target, StdinBudget::FORCE_SYNC).len(),
                MAX_PARALLEL_P4_COMMANDS
            );
        }
    }

    /// 1 MiB 预算下的边界与独占规则和 256 KiB 那份完全一样：正好装满不切、多一个字节就切、
    /// 单个超载参数独占一批（前后两个短参数不与之合并）。
    #[test]
    fn the_force_sync_budget_keeps_the_same_boundary_rules() {
        let half = "x".repeat(FORCE_SYNC_PAYLOAD_MAX / 2 - 1);
        let args = vec![half.clone(), half];
        assert_eq!(
            plan_stdin_batches_with(&args, 1, StdinBudget::FORCE_SYNC),
            vec![0..2]
        );

        let args = vec![
            "x".repeat(FORCE_SYNC_PAYLOAD_MAX / 2 - 1),
            "x".repeat(FORCE_SYNC_PAYLOAD_MAX / 2),
        ];
        assert_eq!(
            plan_stdin_batches_with(&args, 1, StdinBudget::FORCE_SYNC),
            vec![0..1, 1..2]
        );

        let args = vec![
            "//depot/a".to_owned(),
            "x".repeat(FORCE_SYNC_PAYLOAD_MAX + 1),
            "//depot/b".to_owned(),
        ];
        for target in [1usize, MAX_PARALLEL_P4_COMMANDS] {
            assert_eq!(
                plan_stdin_batches_with(&args, target, StdinBudget::FORCE_SYNC),
                vec![0..1, 1..2, 2..3]
            );
        }
    }

    /// 1 MiB 预算也要补足并发宽度，并把最大批的字节压到等长参数的下界：并发下墙钟跟着
    /// 最大批走，预算变大不该变成「一个大批 + 七个空转」。
    #[test]
    fn the_force_sync_budget_still_fills_the_concurrency_target() {
        const COUNT: usize = 40_000;
        const EACH: usize = 100;

        let args: Vec<String> = (0..COUNT).map(|_| "x".repeat(EACH)).collect();
        let batches =
            plan_stdin_batches_with(&args, MAX_PARALLEL_P4_COMMANDS, StdinBudget::FORCE_SYNC);
        assert_eq!(batches.len(), MAX_PARALLEL_P4_COMMANDS);

        let bytes: Vec<usize> = batches
            .iter()
            .map(|range| args[range.clone()].iter().map(|arg| arg.len() + 1).sum())
            .collect();
        let expected = COUNT / MAX_PARALLEL_P4_COMMANDS * (EACH + 1);
        assert_eq!(
            bytes.iter().copied().max(),
            Some(expected),
            "最大批没有压到等长参数的下界：{bytes:?}"
        );
        assert_eq!(bytes.iter().sum::<usize>(), COUNT * (EACH + 1));

        // 预算只改容量：条数上限照旧压在 8192。
        assert!(
            batches
                .iter()
                .all(|range| range.end - range.start <= FORCE_SYNC_SPEC_MAX)
        );
    }

    /// 强制修复下发时用的命令形状：`p4 sync -f`（见 `crate::reconcile::sync::SYNC_ARGS`）。
    const SYNC_ARGS_LIKE: &[&str] = &["sync", "-f"];

    /// 与现场同形的深路径规格：`//depot/main/<五层目录>/fNNNN.txt#2`。
    /// 一条约 103 字节，24,000 条就是 2.4 MB——单组越过 8 × 256 KiB。
    fn deep_specs(count: usize) -> Vec<String> {
        const DIR: &str =
            "bulk/d0123456789abcdef/d0123456789abcdef/d0123456789abcdef/d0123456789abcdef";

        (0..count)
            .map(|index| format!("//depot/main/{DIR}/f{index:04}.txt#2"))
            .collect()
    }

    /// 一行聚合诊断要报齐「命令、通道、参数数、载荷字节、容量、批数、单批范围、并发目标」，
    /// 而且**一个路径原文都不出现**——这行是要落盘的，现场日志不该带工作区路径。
    #[test]
    fn the_batch_diagnostic_reports_counts_without_paths() {
        let specs = deep_specs(24_000);
        let plan = plan_batches(SYNC_ARGS_LIKE, &specs, StdinBudget::FORCE_SYNC);
        let line = batch_plan_detail("p4 sync", &plan, &specs, StdinBudget::FORCE_SYNC, true);

        assert!(line.contains("p4 sync, stdin channel"), "{line}");
        assert!(line.contains("24000 argument(s)"), "{line}");
        assert!(
            line.contains("capacity 1048576 byte(s)/8192 spec(s)"),
            "{line}"
        );
        assert!(line.contains("8 batch(es)"), "{line}");
        assert!(line.contains("concurrency target 8"), "{line}");
        assert!(line.contains("specs 2988..3017"), "{line}");
        let total: usize = specs.iter().map(|spec| spec.len() + 1).sum();
        assert!(line.contains(&format!("{total} byte(s)")), "{line}");
        assert!(!line.contains("//depot"), "路径原文不该落盘：{line}");
        assert!(!line.contains("f0000"), "文件名不该落盘：{line}");

        // argv 通道报的是命令行字节与命令行预算（`p4 ignores` 的路径只能挂在命令行上），
        // 与 stdin 载荷不是同一笔账。
        let paths: Vec<String> = (0..300)
            .map(|index| format!(r"C:\ws\dir{index}\file.txt"))
            .collect();
        let plan = plan_batches(&IGNORES_ARGS, &paths, StdinBudget::DEFAULT);
        let line = batch_plan_detail("p4 ignores", &plan, &paths, StdinBudget::DEFAULT, true);

        assert!(line.contains("p4 ignores, argv channel"), "{line}");
        assert!(line.contains("capacity 30719 byte(s)"), "{line}");
        assert!(
            !line.contains("8192 spec(s)"),
            "argv 通道没有条数上限：{line}"
        );
        assert!(!line.contains("//depot"), "{line}");
    }

    /// 预演那一行必须自证没执行过：后缀写明「估算、没有启动进程」。空参数同样要出一行
    /// （0 批），否则「这一组一个规格都没发」与「这一组没被规划」在日志里分不出来。
    #[test]
    fn the_planned_batch_line_says_nothing_was_started() {
        let specs = deep_specs(100);
        let plan = plan_batches(SYNC_ARGS_LIKE, &specs, StdinBudget::FORCE_SYNC);
        let line = batch_plan_detail(
            "plan: Update group (p4 sync)",
            &plan,
            &specs,
            StdinBudget::FORCE_SYNC,
            false,
        );
        assert!(
            line.starts_with("plan: Update group (p4 sync), stdin channel"),
            "{line}"
        );
        assert!(line.ends_with("estimated, nothing started"), "{line}");

        let empty: Vec<String> = Vec::new();
        let plan = plan_batches(SYNC_ARGS_LIKE, &empty, StdinBudget::FORCE_SYNC);
        let line = batch_plan_detail("p4 sync", &plan, &empty, StdinBudget::FORCE_SYNC, true);
        assert!(line.contains("0 argument(s)"), "{line}");
        assert!(line.contains("0 batch(es)"), "{line}");
        assert!(line.contains("specs n/a"), "{line}");
    }

    /// 预演走的是同一个规划入口（[`say_planned_batches`]）：它报的批数就是执行路径会切出的
    /// 批数，而且**一次调用都不记**——预演的全部意义就是不下发，`p4Calls` 必须一个不动。
    #[test]
    fn the_planned_batches_are_the_executor_batches_and_start_nothing() {
        let specs = deep_specs(24_000);

        let before = crate::json::p4_command_calls(P4Command::Sync);
        say_planned_batches(
            "plan: Update group (p4 sync)",
            SYNC_ARGS_LIKE,
            &specs,
            StdinBudget::FORCE_SYNC,
        );
        assert_eq!(
            crate::json::p4_command_calls(P4Command::Sync),
            before,
            "预演规划不许记进 p4Calls"
        );

        assert_eq!(
            plan_batches(SYNC_ARGS_LIKE, &specs, StdinBudget::FORCE_SYNC)
                .ranges
                .len(),
            MAX_PARALLEL_P4_COMMANDS,
            "预演报的批数要与执行路径一致"
        );
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

    /// 两半各自保持原顺序；高风险路径交给调用方选择 add 预演或逐字校验的 ignores 查询。
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
    ///
    /// 带 `-v` 的变体（删除保护用的那条）同样如此：它只多要一段更明确的输出，不换通道。
    #[test]
    fn build_p4_command_keeps_ignores_paths_on_the_command_line() {
        let paths = vec!["./a.txt".to_owned()];

        let (cmd, payload) =
            build_p4_command(Path::new("p4"), "/ws", &IGNORES_ARGS, &paths, None, None);

        assert_eq!(argv_of(&cmd), ["ignores", "-i", "./a.txt"]);
        assert!(payload.is_none());

        let (cmd, payload) = build_p4_command(
            Path::new("p4"),
            "/ws",
            &crate::workspace::IGNORES_VERBOSE_ARGS,
            &paths,
            None,
            None,
        );

        assert_eq!(argv_of(&cmd), ["ignores", "-i", "-v", "./a.txt"]);
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
            P4Command::Reconcile,
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
            P4Command::Reconcile,
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

    /// 假 p4 的源码（现场编译，见 [`crate::test_util::compile_fake_exe`]）：在读 stdin
    /// **之前**把 stdout 灌满 4 MiB 的合法 marshal 记录、把 stderr 灌满 1 MiB。它复现的
    /// 是「父进程先写完 stdin 再读输出」的调用方式——大载荷把父进程堵在 stdin 写上，
    /// 假进程堵在 stdout 写上，两条管道互相等。走 stdin 的载荷必须超过管道容量
    /// （Windows 上 64 KiB），否则一次 `write_all` 就送完了，死锁复现不出来。
    const FAKE_P4_SOURCE: &str = r#"
use std::io::{Read, Write};

fn mstring(value: &[u8]) -> Vec<u8> {
    let mut out = vec![b's'];
    out.extend_from_slice(&(value.len() as i32).to_le_bytes());
    out.extend_from_slice(value);
    out
}

fn main() {
    // 卡住时由假进程自行终止，父调用仍走 status().await 回收，不遗留管道或子进程。
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(30));
        std::process::exit(124);
    });
    // 合法 marshal 字典：'{' + 键值对 + '0' 终止。code 取 info，have 解析器对它
    // 照常消费、不投影成记录——这里只需要格式合法的体积。
    let filler = vec![b'x'; 64 * 1024];
    let mut record = vec![b'{'];
    record.extend(mstring(b"code"));
    record.extend(mstring(b"info"));
    record.extend(mstring(b"data"));
    record.extend(mstring(&filler));
    record.push(b'0');

    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    for _ in 0..64 {
        let _ = stdout.write_all(&record);
    }
    let _ = stdout.flush();

    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let err_chunk = vec![b'e'; 64 * 1024];
    for _ in 0..16 {
        let _ = stderr.write_all(&err_chunk);
    }
    let _ = stderr.flush();

    let mut payload = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut payload);
    // cwd 由被测代码设成 work_dir，落盘路径因此可预测。
    let _ = std::fs::write("fake-stdin-payload.bin", &payload);
}
"#;

    /// [`run_p4_have`] 的三条管道必须并发推。假 p4 在读 stdin 前灌满 stdout 与 stderr，
    /// 「先写完 stdin 再读输出」的旧实现会死锁在「我们等 stdin 写完、它等 stdout 排空」
    /// 上。假进程的 30 秒看门狗会终止阻塞，父调用等待并回收后以失败退出。
    #[test]
    fn have_does_not_deadlock_when_the_fake_fills_stdout_before_reading_stdin() {
        use std::time::{Duration, Instant};

        let fake = compile_fake_exe("fake-p4", FAKE_P4_SOURCE)
            .expect("必须能编译假 p4，背压回归不能静默跳过");

        let tree = TempTree::new("p4-have-backpressure");
        let work_dir = tree.root.to_string_lossy().to_string();
        // specs 全 ASCII：任何命令字符集下编码一致，期望载荷可以在本地构造。
        // 条数要撑到**单次写入超过 1 MiB**：force sync 那一档预算允许一批写到 1 MiB，
        // 管道背压不能只在 256 KiB 那一档上验证过。
        let specs: Vec<String> = (0..30_000)
            .map(|index| format!("//depot/fake/long_path_{index:04}/file.bin"))
            .collect();
        let mut expected_payload = Vec::new();
        for spec in &specs {
            expected_payload.extend_from_slice(spec.as_bytes());
            expected_payload.push(b'\n');
        }
        assert!(
            expected_payload.len() > 1024 * 1024,
            "载荷要超过 1 MiB（force sync 的单批上限），实际 {}",
            expected_payload.len()
        );

        let options = Options::parse_from(["p4delta", "-w", "p4delta-test"]);
        let started = Instant::now();
        let outcome = task::block_on(run_p4_have_with(&fake, &options, &work_dir, &specs));

        // 假进程的输出合法但不是 have 记录（code 是 info）：解析照常完成，记录为空。
        assert!(
            outcome.is_ok(),
            "parsing the fake marshal stream failed: {outcome:?}"
        );

        // 假进程真的读到了完整的 stdin 载荷——三路并发推的直接证据。
        let payload_path = tree.root.join("fake-stdin-payload.bin");
        let payload = std::fs::read(&payload_path).unwrap_or_else(|error| {
            panic!(
                "the fake p4 never wrote its stdin payload to {}: {error}",
                payload_path.display()
            )
        });
        assert_eq!(payload, expected_payload);
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the call only finishes inside the timeout budget"
        );
    }
}
