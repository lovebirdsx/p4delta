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
use crate::model::HaveRecord;
use crate::p4::marshal::MarshalStreamParser;

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

/// Run `p4 -G have` and parse the binary marshal output to get sync timestamps
pub(crate) async fn run_p4_have(
    options: &Options,
    work_dir: &str,
) -> Result<HashMap<String, HaveRecord>> {
    println!("   Querying file sync timestamps.");
    let start_time = Instant::now();

    let mut cmd = Command::new(crate::locate::p4_exe()?);
    cmd.current_dir(work_dir);
    // p4 在 Windows 上按继承来的 PWD 找 .p4config，显式指定 cwd 时必须同时清掉 PWD。
    cmd.env_remove("PWD");

    if let Some(workspace) = &options.workspace {
        cmd.arg("-c");
        cmd.arg(workspace);
    }

    cmd.arg("-G");
    cmd.arg("have");
    cmd.arg("...");

    if options.verbose {
        println!("    Running: p4 -G have ...");
    }

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn()?;
    // 参数是 `...`，用不上 stdin。
    let P4Pipes { stdout, stderr, .. } = take_p4_pipes(&mut child, None)?;

    // Parse while reading: the raw response is several GB on a large workspace, so buffering
    // it whole before parsing is what made this run out of memory.
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

    println!(
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
/// - 被忽略的文件名因此过滤不掉：`workspace.rs` 的文件级忽略拿不到结果，非 ASCII 的
///   被忽略文件会一路走到 Add 类，最后由 `p4 add` 以「拒绝忽略文件」报错退场。
///   `tests/e2e_prune.rs` 里盯着这条回退路径的用例是这里的守门人。
fn takes_paths_from_stdin(always_args: &[&str]) -> bool {
    !always_args.contains(&"ignores")
}

/// 已经提醒过「有路径交不到 p4 手上」。同一轮里文件级过滤与目录级剪枝会各问一次，
/// 同一个问题重复出现，刷屏没有信息量。
static WARNED_UNSENDABLE_PATHS: AtomicBool = AtomicBool::new(false);

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

/// 从一批路径里挑出能交给 p4 命令行的那些。剩下的保守地按「p4 判断不了」处理——
/// 宁可多报一个变更，也不能把交不出去的路径当成「未被忽略」以外的结论。
pub(crate) fn command_line_ready_paths(paths: &[String]) -> Vec<String> {
    let (ready, dropped): (Vec<String>, Vec<String>) = paths
        .iter()
        .cloned()
        .partition(|path| command_line_safe(path));

    if !dropped.is_empty() && !WARNED_UNSENDABLE_PATHS.swap(true, Ordering::Relaxed) {
        eprintln!(
            "Warning: {} of {} path(s) contain characters p4 cannot read from the command line \
             on this platform; their ignored state is left undecided, so they are never treated \
             as ignored.",
            dropped.len(),
            paths.len()
        );
    }

    ready
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
    /// 只告警不失败。
    fn is_lenient(self) -> bool {
        self == Self::Warn
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
    if !success {
        return Some(format!("p4 {command} exited with {status}: {stderr}"));
    }

    // 退出码为 0 也可能是失败，见 [FailureMode::ExitCodeOrStderr]。
    (mode == FailureMode::ExitCodeOrStderr && !stderr.is_empty())
        .then(|| format!("p4 {command} reported: {stderr}"))
}

/// Runs one slice of a batched p4 command (eg. p4 stuff a100 a101 ... a198 a199)
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

/// Reads a p4 output stream line by line, decoding each line with the resolved charset.
/// Streaming keeps large responses (fstat on a big workspace is tens of millions of lines)
/// from being buffered in full.
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

        // A BOM can only ever appear at the very start of the stream.
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

/// Maximum number of concurrent p4 processes. The server is configured with maxParallel=8, and
/// running more clients than that only adds process startup overhead and memory pressure.
/// Unbounded spawning previously put tens of thousands of p4.exe processes in flight at once.
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

    println!(
        "      Running \"p4 {}\" with {} batches.",
        always_args[0],
        batches.len()
    );

    // `buffered` polls at most MAX_PARALLEL_P4_COMMANDS futures at a time. The child processes
    // still run in parallel, because async-process dispatches pipe reads to a blocking thread
    // pool, so this does not need task::spawn - and therefore does not need the 'static
    // transmutes that unbounded spawning required to satisfy the borrow checker.
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

    // Drain every batch even after a failure, which matches the previous behavior of running
    // all batches to completion before reporting. Dropping the stream instead would cancel
    // in-flight batches, leaving a write command partially applied with no way to tell which
    // parts landed.
    while let Some(slice) = slices.next().await {
        match slice {
            Ok(lines) if first_error.is_none() => results.extend(lines),
            // Results are pointless once a failure is being reported, so stop accumulating.
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

    /// 挑剩下的路径交给调用方按「未被忽略」处理，顺序保持不变。
    #[test]
    fn command_line_ready_paths_drop_only_what_p4_cannot_read() {
        let paths = vec![
            r"C:\ws\readme.txt".to_owned(),
            r"C:\ws\使用说明.txt".to_owned(),
            r"C:\ws\build".to_owned(),
        ];

        let ready = command_line_ready_paths(&paths);

        if cfg!(windows) {
            assert_eq!(ready, [paths[0].clone(), paths[2].clone()]);
        } else {
            assert_eq!(ready, paths);
        }
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

        // An argument longer than the limit gets a batch to itself. This used to emit an
        // empty batch as well, which ran `p4 ignores` with no arguments at all.
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
            // Each argument contributes its length plus a separating space.
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
