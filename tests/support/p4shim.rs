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
use std::sync::atomic::{AtomicUsize, Ordering};

use super::paths::e2e_dir;

/// shim 的源码。它不参与任何 crate 的编译，只在这里被 rustc 现场编出来。
const SHIM_SOURCE: &str = include_str!("p4_shim_main.rs");

/// 进程内自增序号：临时名里除了 pid 还得带上它，理由见 [`P4Shim::build`]。
static TEMP_SEQ: AtomicUsize = AtomicUsize::new(0);

/// 编好的 shim。字段是喂给 `P4_EXE` 的路径。
pub struct P4Shim {
    pub path: PathBuf,
}

impl P4Shim {
    /// 编译（或复用已编好的）shim；不可用时返回 `None`——原因由 [`unavailable`] 统一打一行
    /// `skipping:`，调用方直接返回即可。
    ///
    /// 产物按**源码内容哈希**命名（`p4-shim-<hash>.exe`）：源码没变就直接复用，省掉每个
    /// 用例一次 rustc；名字带指纹也顺手取消了 stamp 文件与「exe 已发布、stamp 还没写」
    /// 这类中间态。旧指纹的产物留在原地不删（同 `template.rs`：删掉正被别人读着的东西
    /// 比多占几百 KB 糟得多），由 `cargo clean` 收拾。
    ///
    /// 编译走 staging + rename。冷跑时多个调用方会同时走到这里——nextest 是
    /// process-per-test（不同进程），`cargo test` 是同一个二进制里的多个线程，两种并发
    /// 都要挡，所以临时名带 pid + 进程内序号（见下）。早先的写法是往**同一个** `.rs` 与
    /// **同一个** exe 路径上直接写：交叠时后来者可能在别人读到一半的源码上跑 rustc
    /// （编译失败 → 调用方打一行 `skipping:` 后**安静地绿**），或者把别人正在执行的 exe
    /// 覆盖掉。范式照抄 `template.rs` 的模板发布：各写各的临时文件、rename 发布、输了就用
    /// 赢家那份。
    pub fn build() -> Option<P4Shim> {
        let Some(rustc) = rustc() else {
            return unavailable("rustc is not found".to_owned());
        };
        let out_dir = e2e_dir().join("tools");
        if let Err(error) = std::fs::create_dir_all(&out_dir) {
            return unavailable(format!("could not create {}: {error}", out_dir.display()));
        }

        let fingerprint = hash(SHIM_SOURCE).to_string();
        let out = out_dir.join(exe_name(&format!("p4-shim-{fingerprint}")));
        if out.is_file() {
            return Some(P4Shim { path: out });
        }

        // 临时名 = 指纹 + pid + 进程内序号。**分隔符只能用 `-`**：rustc 从源文件名推导
        // crate 名，点号不是合法字符——实测 `p4-shim-<hash>.tmp.<pid>.rs` 直接报
        // `invalid character '.' in crate name`，于是整个 shim 编不出来、依赖它的用例
        // 集体「0.3 秒通过」（见 `unavailable`）。
        //
        // 只有 pid 不够：nextest 是 process-per-test，但 `cargo test`（environment.md
        // 推荐的复现路径）是**一个二进制里的多个线程**——同一个 pid 下并发的用例会算出
        // 同一个临时名，两个 link.exe 抢同一个 `/OUT:`（实测 LNK1104），先完成的那次还会
        // 把源文件从兄弟线程的 rustc 手里删掉。那一档 nextest 永远看不见。
        let pid = std::process::id();
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let source = out_dir.join(format!("p4-shim-{fingerprint}-{pid}-{seq}.rs"));
        let staging = out_dir.join(exe_name(&format!("p4-shim-{fingerprint}-{pid}-{seq}")));
        if let Err(error) = std::fs::write(&source, SHIM_SOURCE) {
            return unavailable(format!("could not write {}: {error}", source.display()));
        }
        let output = Command::new(rustc)
            .arg("--edition")
            .arg("2021")
            .arg(&source)
            .arg("-o")
            .arg(&staging)
            .output();
        let _ = std::fs::remove_file(&source);
        // MSVC 的链接器默认产出一份 .pdb，`-C debuginfo=0` 拦不住它（实测无效）。
        // shim 是纯转发器，没人在它上面调试，而这份符号文件的名字跟着 staging 的 pid 走、
        // 与发布后的 `p4-shim-<hash>.exe` 对不上——不删就是每次改源码留一个 1.6 MB 的垃圾。
        let _ = std::fs::remove_file(staging.with_extension("pdb"));

        let built = match output {
            Ok(output) if output.status.success() && staging.is_file() => staging,
            Ok(output) => {
                let _ = std::fs::remove_file(&staging);
                return unavailable(format!(
                    "rustc failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim_end()
                ));
            }
            Err(error) => return unavailable(format!("could not run rustc: {error}")),
        };

        match std::fs::rename(&built, &out) {
            Ok(()) => {}
            Err(_) => {
                // 输的那个：可能已有赢家发布了同名文件（内容是同一份），也可能它正被某个
                // 测试执行、Windows 上因此拒绝被替换。两种都用已发布的那份；真没有才失败。
                let _ = std::fs::remove_file(&built);
                if !out.is_file() {
                    return unavailable(format!("could not publish to {}", out.display()));
                }
            }
        }

        Some(P4Shim { path: out })
    }
}

/// 「shim 不可用」的统一出口，`build` 的每条 `None` 路径都过这里。
///
/// 设了 `P4_E2E_REQUIRED`（CI 与本地要严格跑时都会设）就直接失败：跑 cargo 的机器必然
/// 有 rustc，shim 编不出来说明是真的坏了，不能让它退化成一行被 nextest 捕获的
/// `skipping:`——依赖 shim 的用例会照常「通过」，覆盖悄悄消失。这不是假设：源文件名里
/// 一个点号就曾让整条 bulk 用例（本仓库最重的一条）0.3 秒「通过」。
fn unavailable(reason: String) -> Option<P4Shim> {
    assert!(
        std::env::var_os("P4_E2E_REQUIRED").is_none(),
        "P4_E2E_REQUIRED is set but the p4 shim is unavailable: {reason}"
    );
    eprintln!("skipping: the p4 shim is unavailable: {reason}");
    None
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
