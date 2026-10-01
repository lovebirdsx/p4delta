//! p4 子进程的调用与批次切分。

use std::collections::HashMap;
use std::io;
use std::process::Stdio;
use std::time::Instant;

use anyhow::{Error, Result, anyhow, bail};
use async_process::Command;
use futures::io::AsyncBufReadExt;
use futures::{AsyncReadExt, StreamExt};

use crate::READ_BUFFER_SIZE;
use crate::charset::{decode_p4_bytes, p4_encoding, strip_bom, trim_line_ending};
use crate::cli::Options;
use crate::model::HaveRecord;
use crate::p4::marshal::MarshalStreamParser;

// The Win32 limit is 32767 characters (https://learn.microsoft.com/en-us/troubleshoot/windows-client/shell-experience/command-line-string-limitation)
// Leave 2k characters for whatever else is in the command line.
pub(crate) const ARGUMENT_LENGTH_MAX: usize = 32767 - 2048;

/// Run `p4 -G have` and parse the binary marshal output to get sync timestamps
pub(crate) async fn run_p4_have(
    options: &Options,
    work_dir: &str,
) -> Result<HashMap<String, HaveRecord>> {
    println!("   Querying file sync timestamps.");
    let start_time = Instant::now();

    let mut cmd = Command::new("p4");
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
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to capture p4 stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Failed to capture p4 stderr"))?;

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

    let read_stderr = async move {
        let mut buffer = Vec::new();
        let mut stderr = stderr;
        let _ = stderr.read_to_end(&mut buffer).await;
        buffer
    };

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

/// Runs one slice of a batched p4 command (eg. p4 stuff a100 a101 ... a198 a199)
pub(crate) async fn run_p4_command_slice(
    options: &Options,
    work_dir: &str,
    always_args: &[&'static str],
    batched_args_slice: &[String],
    use_changelist: bool,
    strict: bool,
) -> Result<Vec<String>> {
    let mut cmd = Command::new("p4");
    cmd.current_dir(work_dir);
    // p4 在 Windows 上按继承来的 PWD 找 .p4config，显式指定 cwd 时必须同时清掉 PWD。
    cmd.env_remove("PWD");

    if let Some(workspace) = &options.workspace {
        cmd.arg("-c");
        cmd.arg(workspace);
    }

    cmd.args(always_args);

    if use_changelist && options.changelist != 0 {
        cmd.arg("-c");
        cmd.arg(options.changelist.to_string());
    }

    cmd.args(batched_args_slice);

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        // p4 起不来（通常是没装、不在 PATH 里）：strict 调用方必须失败，但文件级
        // 过滤这类 lenient 调用方沿用「p4 报错只告警，不改变已有结果」的契约，
        // 返回空结果而不是中断整轮。真实运行时 p4 缺失早在 fstat/have 阶段就退出，
        // 走不到这里，因此这个降级掩盖不了真实问题。
        Err(error) if !strict && error.kind() == io::ErrorKind::NotFound => {
            eprintln!(
                "Warning: p4 {} could not be started: {}",
                always_args[0], error
            );
            return Ok(Vec::new());
        }
        Err(error) => return Err(error.into()),
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to capture p4 stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Failed to capture p4 stderr"))?;

    // Both pipes must be drained concurrently. A child that fills its stderr pipe blocks
    // forever if we only read stdout.
    let read_stdout = read_p4_lines(stdout);
    let read_stderr = async move {
        let mut buffer = Vec::new();
        let mut stderr = stderr;
        let _ = stderr.read_to_end(&mut buffer).await;
        buffer
    };

    let (lines, stderr_bytes) = futures::join!(read_stdout, read_stderr);
    let lines = lines?;

    let status = child.status().await?;
    // 非零状态不一定说明输出不可用：`p4 ignores` 有没有匹配都返回 0，其他命令的
    // 普通 warning 也可能带非零状态。但目录级剪枝不能拿半截输出做判断，strict 时直接失败。
    if !status.success() {
        let message = format!(
            "p4 {} exited with {}: {}",
            always_args[0],
            status,
            String::from_utf8_lossy(&stderr_bytes).trim()
        );

        if strict {
            bail!("{message}");
        }

        eprintln!("Warning: {message}");
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

/// Maximum number of concurrent p4 processes. The server is configured with maxParallel=8, and
/// running more clients than that only adds process startup overhead and memory pressure.
/// Unbounded spawning previously put tens of thousands of p4.exe processes in flight at once.
pub(crate) const MAX_PARALLEL_P4_COMMANDS: usize = 8;

/// Splits arguments into batches that fit within the Windows command line limit.
/// An argument longer than the limit gets a batch of its own rather than an empty one.
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

/// Runs a p4 command with thousands of arguments in multiple batches to bypass windows input limit
/// `strict` 为真时任何一批非零退出都算失败，调用方必须放弃基于输出的判断。
pub(crate) async fn run_p4_command_batched(
    options: &Options,
    work_dir: &str,
    always_args: &[&'static str],
    batched_args: &[String],
    use_changelist: bool,
    strict: bool,
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
                strict,
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

    #[test]
    fn empty_arguments_produce_no_batches() {
        assert!(compute_batches(&[]).is_empty());
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
            false,
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
            true,
        ));
        assert!(
            strict.is_err(),
            "a failed p4 query must not be usable for pruning"
        );
    }
}
