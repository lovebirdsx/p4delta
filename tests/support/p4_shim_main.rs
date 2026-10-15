//! e2e 用的 p4 转发 shim 源码。
//!
//! 它不属于任何 crate，而是由 `tests/support/p4shim.rs` 在运行时用 rustc 编出来（理由见那里）。
//! 每次被调用：先往 `P4DELTA_SHIM_LOG` 追加一行（记下这次启动的参数），再把参数原样转发给
//! `P4DELTA_SHIM_REAL_P4`，三条 stdio 与退出码都透传——这样 p4delta 看起来就像在直接调 p4。
//!
//! 日志一行 = 一次真实的 p4 进程启动，是与 p4delta 自报计数相互独立的观测。
//!
//! 故障注入：`P4DELTA_SHIM_FAULT_COMMAND` 给一个命令名，命中那次调用就**不转发**，改为按
//! `P4DELTA_SHIM_FAULT_STDOUT` / `P4DELTA_SHIM_FAULT_STDERR` 输出并以 0 退出。它模拟的是
//! 「p4 起来了、退出码 0，但输出没有可用裁决」这一类最危险的失败形态——正是删除保护必须
//! 稳住的那些。

use std::io::Write;
use std::process::{Command, Stdio};

fn main() {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    if let Some(log) = std::env::var_os("P4DELTA_SHIM_LOG") {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
        {
            // 整行一次追加，避免并发进程把正文与换行交错写入。
            let _ = file.write_all(format!("{}\n", args.join(" ")).as_bytes());
        }
    }

    // 按「任意一个参数等于命令名」命中，与用例读日志时的判据一致：p4 的全局选项
    // （`-G`、`-Mj -Ztag`、`-x - -b N`）会排在命令名前面。
    if let Some(fault) = std::env::var_os("P4DELTA_SHIM_FAULT_COMMAND") {
        let fault = fault.to_string_lossy().into_owned();
        if args.iter().any(|arg| *arg == fault) {
            print!("{}", std::env::var("P4DELTA_SHIM_FAULT_STDOUT").unwrap_or_default());
            eprint!("{}", std::env::var("P4DELTA_SHIM_FAULT_STDERR").unwrap_or_default());
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            std::process::exit(0);
        }
    }

    let real =
        std::env::var_os("P4DELTA_SHIM_REAL_P4").expect("the p4 shim needs P4DELTA_SHIM_REAL_P4");
    let status = Command::new(real)
        .args(std::env::args_os().skip(1))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .expect("the p4 shim must forward to the real p4");

    std::process::exit(status.code().unwrap_or(1));
}
