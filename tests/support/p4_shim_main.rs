//! e2e 用的 p4 转发 shim 源码。
//!
//! 它不属于任何 crate，而是由 `tests/support/p4shim.rs` 在运行时用 rustc 编出来（理由见那里）。
//! 每次被调用：先往 `P4DELTA_SHIM_LOG` 追加一行（记下这次启动的参数），再把参数原样转发给
//! `P4DELTA_SHIM_REAL_P4`，三条 stdio 与退出码都透传——这样 p4delta 看起来就像在直接调 p4。
//!
//! 日志一行 = 一次真实的 p4 进程启动，是与 p4delta 自报计数相互独立的观测。

use std::io::Write;
use std::process::{Command, Stdio};

fn main() {
    if let Some(log) = std::env::var_os("P4DELTA_SHIM_LOG") {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
        {
            let args: Vec<String> = std::env::args_os()
                .skip(1)
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            let _ = writeln!(file, "{}", args.join(" "));
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
