//! 模板：seed 一次，之后每个实例从它复制。
//!
//! 每个测试进程都会调用 [`ensure_template`]；冷缓存下可能重复 seed，但只发布一份完整模板。
//! 热缓存复用固定提交历史，每个实例仍复制自己的数据库与归档文件。
//!
//! 模板按 [`Kind`] 分种类：种子内容不同就是不同的模板，指纹、staging 名与进程内互斥
//! 都跟着种类分开，两种模板可以各自并发构建、互不阻塞。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use super::paths::{InstancePaths, e2e_dir};
use super::seed;
use super::tools::Tools;

/// 种子格式的版本号。改了 [`seed`] 中的种子内容或提交步骤就加一，下一轮使用新模板。
///
/// bulk 夹具的形状不走这个号：它整份写在 `bulk.rs` 里，[`stamp`] 直接按那个文件的内容
/// 取指纹，任何一处改动都会自己让旧模板失效。
const SEED_VERSION: u32 = 1;

/// 完成标记的文件名，写在模板目录内部。
const STAMP: &str = ".complete";

/// p4d 数据库在实例布局里的目录名，与 [`InstancePaths::server`] 是同一个。
const SERVER_DIR: &str = "server";

/// 模板的种类。种子内容不同就是不同的模板：指纹、staging 名与进程内互斥都按它分开，
/// 两种模板可以各自并发构建、互不阻塞。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 基础的五个文件。绝大多数用例用这一份。
    Base,
    /// 基础五文件之后再入库 bulk 夹具的两版（见 `bulk.rs`）——`e2e_sync.rs` 的规模用例
    /// 用这一份，实例只需一次 `sync ...#1` 就落到它要的现场。
    Bulk,
}

impl Kind {
    /// 折进目录名与指纹的短标签。
    fn label(self) -> &'static str {
        match self {
            Kind::Base => "base",
            Kind::Bulk => "bulk",
        }
    }

    /// 本种类的进程内互斥。
    ///
    /// 不同的 `tests/*.rs` 是不同进程，靠 rename 的原子性协调就够了；但**同一个**
    /// 测试二进制里的用例是并行线程、共享 pid，光靠 rename 挡不住——两个线程会算出
    /// 同一个 staging 目录，然后互相把对方正在写的目录删掉。实测到的症状是
    /// `p4d -xi` 报 `unlink: jnl.invalid-utf8`，以及客户端连不上刚被删掉目录的服务器
    /// （`WSAECONNREFUSED`）。模板只有首次运行才需要生成，加一把锁不影响并行度。
    ///
    /// 锁与 staging 名都按种类分开：Base 与 Bulk 的构建互不相干，不该互相等；分开之后
    /// 两份 staging 也就不可能撞名。
    fn lock(self) -> &'static Mutex<()> {
        static BASE: Mutex<()> = Mutex::new(());
        static BULK: Mutex<()> = Mutex::new(());
        match self {
            Kind::Base => &BASE,
            Kind::Bulk => &BULK,
        }
    }
}

/// 确保本种类的模板存在且与当前工具匹配，返回模板目录。
///
/// 模板目录名由指纹决定，**发布之后既不修改也不删除**。这一点是实测逼出来的：
/// 早先的写法是固定名 `template/`、发布前先删旧的，于是并发的第二个进程会把
/// 第一个刚发布的模板删掉、再发布自己那份，中间那段「目录不存在」的窗口里，
/// 第三个进程的 [`instantiate`] 直接 ENOENT。冷启动时并行跑两个测试二进制，
/// 3 轮里 2 轮整份用例全灭——当时用的是 `cargo test`，它逐个二进制串行执行，
/// 同一个二进制里的线程又被进程内互斥挡住，所以这个缺陷在本机怎么跑
/// 都看不见。
///
/// 名字带指纹就没有这个窗口：发布走 rename，读到的东西要么不存在、要么完整。
/// 代价是换了 p4d 或改了种子之后，旧目录会留在 `target/e2e/` 下——不删是有意的，
/// 「删掉不匹配的目录」等于把上面那个窗口重新打开。它们由 `cargo clean` 收拾。
///
/// 现在跑的是 nextest（process-per-test），冷启动时多个进程会同时走到这条路径上：
/// 它不再是「只在并发下才现形」的隐患，而是每次冷跑都会被真实走一遍。
pub fn ensure_template(tools: &Tools, kind: Kind) -> io::Result<PathBuf> {
    let _guard = kind
        .lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ensure_template_serialized(tools, kind)
}

fn ensure_template_serialized(tools: &Tools, kind: Kind) -> io::Result<PathBuf> {
    let stamp = stamp(tools, kind);
    let template = e2e_dir().join(format!("template-{}", fingerprint(&stamp)));

    if read_stamp(&template).as_deref() == Some(stamp.as_str()) {
        return Ok(template);
    }

    fs::create_dir_all(e2e_dir())?;
    // staging 名带上种类：锁也按种类分开了，两种模板会同时构建，名字撞上就会
    // 互删对方的目录（见 `Kind::lock`）。
    let staging = e2e_dir().join(format!("template.tmp.{}.{}", kind.label(), process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;

    if let Err(error) = seed::seed_into(&staging, tools, kind) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    // 标记写在 staging 内部：rename 是原子的，所以最终目录一出现就必然是完整的，
    // 不存在「目录在了但标记还没写」这种会被别人误判成损坏的中间态。
    fs::write(staging.join(STAMP), &stamp)?;

    match fs::rename(&staging, &template) {
        Ok(()) => Ok(template),
        Err(_) => {
            // 目录名一样就意味着指纹一样，谁赢都一样，用赢家的那份。
            let _ = fs::remove_dir_all(&staging);
            match read_stamp(&template) {
                Some(winner) if winner == stamp => Ok(template),
                _ => Err(io::Error::other(format!(
                    "could not publish the template to {}",
                    template.display()
                ))),
            }
        }
    }
}

/// 把模板指纹折成一个短标签，用作目录名。
///
/// 自己写 FNV-1a 而不用 `DefaultHasher`：后者的输出不保证跨 Rust 版本稳定，
/// 目录名会跟着工具链变，回头看很难解释这些东西是怎么来的。
fn fingerprint(stamp: &str) -> String {
    format!("{:016x}", hash(stamp))
}

/// FNV-1a，同上：稳定、跨版本可比。指纹与 bulk 内容摘要都用它。
fn hash(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 把模板数据库复制成实例的 `server/` 目录。
///
/// 复制的是模板里**名叫 `server` 的那个子目录**，不是模板目录本身：
/// 模板根下还躺着工作区、完成标记这些不属于数据库的东西，多复制一层
/// 会让 p4d 在空目录上另建一个全新的库。
pub fn instantiate(template: &Path, paths: &InstancePaths) -> io::Result<()> {
    copy_dir(&template.join(SERVER_DIR), &paths.server)
}

fn read_stamp(template: &Path) -> Option<String> {
    fs::read_to_string(template.join(STAMP)).ok()
}

/// 模板指纹：种类 + 种子版本 + p4d 的身份（+ bulk 夹具的内容指纹）。
/// 任何一项变了都要重建模板。
///
/// 用文件大小与 mtime 而不是跑 `p4d -V`：后者在工程根下会因为读到
/// 仓库自己的 `LICENSE` 而报错，不必为了一个指纹去踩那个坑。
fn stamp(tools: &Tools, kind: Kind) -> String {
    let (size, mtime) = match fs::metadata(&tools.p4d) {
        Ok(meta) => (
            meta.len(),
            meta.modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or_default(),
        ),
        Err(_) => (0, 0),
    };
    let mut stamp = format!(
        "kind={}\nseed={SEED_VERSION}\np4d={}\nsize={size}\nmtime={mtime}\n",
        kind.label(),
        tools.p4d.display()
    );
    if kind == Kind::Bulk {
        // bulk 夹具的规模、深路径与内容生成全在 `bulk.rs` 里，直接按那个文件的内容取指纹：
        // 改了规模或内容，旧模板自动作废，不用记得手工加版本号。
        stamp.push_str(&format!("bulk={:016x}\n", hash(include_str!("bulk.rs"))));
    }
    stamp
}

fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        // 完成标记只属于模板目录，不该混进实例的数据库里。
        if entry.file_name() == STAMP {
            continue;
        }
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
