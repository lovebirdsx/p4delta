//! 一个可执行的 p4 转发 shim：把每次调用记进日志、再原样交给真实 p4。
//!
//! 用途是**外部观测**。e2e 把 `P4_EXE` 指到它，跑完之后数日志行数，就得到这一轮真正启动过的
//! p4 进程数。它与 p4delta 自己上报的 `p4Calls` 完全独立——漏记一处插桩（少算）或重复记账
//! （多算）都会让两者对不上，从而被用例抓住。日志同时记下每次调用的参数，用例因此还能断言
//! 「charset / fstat / have 这几条生产路径确实各起过进程」。
//!
//! 为什么在运行时用 rustc 现编：`P4_EXE` 必须是**真正可执行的文件**——Windows 上 `.cmd`/`.bat`
//! 起不来，Unix 上脚本又要执行位，两边的姿势不通用。而仓库不希望为测试多出一个随发布一起
//! 构建的 bin 目标（`install.ps1` 那套发布产物应当仍然只有一个 exe）。编出来的东西落在
//! `target/e2e/tools/` 下，`cargo clean` 一并清掉。

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::paths::e2e_dir;

/// shim 的源码。它不参与任何 crate 的编译，只在这里被 rustc 现场编出来。
const SHIM_SOURCE: &str = include_str!("p4_shim_main.rs");

/// 编好的 shim。字段是喂给 `P4_EXE` 的路径。
pub struct P4Shim {
    pub path: PathBuf,
}

impl P4Shim {
    /// 编译（或复用已编好的）shim；找不到 rustc 时返回 `None`，调用方打印 `skipping:` 后跳过。
    ///
    /// 缓存按源码内容哈希：源码没变就直接复用上次编出来的 exe，省掉每个用例一次 rustc。
    pub fn build() -> Option<P4Shim> {
        let rustc = rustc()?;
        let out_dir = e2e_dir().join("tools");
        std::fs::create_dir_all(&out_dir).ok()?;

        let out = out_dir.join(exe_name("p4-shim"));
        let stamp = out_dir.join("p4-shim.src-hash");
        let expected = hash(SHIM_SOURCE).to_string();
        let up_to_date = out.is_file()
            && std::fs::read_to_string(&stamp).is_ok_and(|text| text.trim() == expected);

        if !up_to_date {
            let source = out_dir.join("p4-shim.rs");
            std::fs::write(&source, SHIM_SOURCE).ok()?;
            let output = Command::new(rustc)
                .arg("--edition")
                .arg("2021")
                .arg(&source)
                .arg("-o")
                .arg(&out)
                .output()
                .ok()?;
            if !output.status.success() || !out.is_file() {
                eprintln!(
                    "Warning: could not build the p4 shim: {}",
                    String::from_utf8_lossy(&output.stderr).trim_end()
                );
                return None;
            }
            let _ = std::fs::write(&stamp, expected);
        }

        Some(P4Shim { path: out })
    }
}

/// 测试机上的 rustc。
///
/// cargo 不把 `RUSTC` 传进测试进程（实测只有 `CARGO`），但 rustc 与 cargo 同处一个工具链的
/// bin 目录，先按 `CARGO` 的兄弟文件找，再退回 `PATH`。
fn rustc() -> Option<PathBuf> {
    if let Some(cargo) = env::var_os("CARGO") {
        let sibling = Path::new(&cargo).with_file_name(exe_name("rustc"));
        if sibling.is_file() {
            return Some(sibling);
        }
    }

    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(exe_name("rustc")))
        .find(|candidate| candidate.is_file())
}

fn exe_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

/// FNV-1a：只用来给缓存文件打个源码指纹，不需要抗碰撞。
fn hash(text: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
