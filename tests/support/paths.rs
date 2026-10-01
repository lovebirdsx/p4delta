//! 沙箱的目录布局与路径校验。
//!
//! 所有运行时数据都落在 `target/e2e/` 下：它已被 gitignore，`cargo clean` 能一并清掉，
//! 而且失败现场留在工程里，不用去系统临时目录里刨。

use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// 工程根，编译期由 cargo 注入。
pub fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// cargo 的 target 目录：尊重 `CARGO_TARGET_DIR`，否则取 `<root>/target`。
pub fn target_dir() -> PathBuf {
    match env::var_os("CARGO_TARGET_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => project_root().join("target"),
    }
}

/// e2e 运行时数据的根。
pub fn e2e_dir() -> PathBuf {
    target_dir().join("e2e")
}

/// 一个 p4d 实例的全部路径。
///
/// `journal`、`pid` 与票据文件刻意放在 `server/` 之外：模板复制的是 `server/` 整个目录，
/// 而残留的 journal 会被 p4d 在下次启动时重放，把刚要来的干净状态改回去。
#[derive(Debug, Clone)]
pub struct InstancePaths {
    pub name: String,
    pub dir: PathBuf,
    pub server: PathBuf,
    pub journal: PathBuf,
    pub log: PathBuf,
    pub pid_file: PathBuf,
    pub ws_root: PathBuf,
    pub cache: PathBuf,
    pub home: PathBuf,
    pub tickets: PathBuf,
    pub enviro: PathBuf,
    pub trust: PathBuf,
}

impl InstancePaths {
    /// `target/e2e/instances/<name>` 下的一个实例。
    pub fn new(name: &str) -> Self {
        assert_safe_instance_name(name);
        let paths = Self::at(e2e_dir().join("instances").join(name));
        // 失败清理会递归删目录，路径必须先证明它确实落在沙箱根之内。
        assert!(
            is_inside(&paths.dir, &e2e_dir()),
            "instance dir escaped the sandbox root: {}",
            paths.dir.display()
        );
        paths
    }

    /// 任意目录下的同一种布局（模板 staging 目录用）。
    pub fn at(dir: PathBuf) -> Self {
        let name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        InstancePaths {
            name,
            server: dir.join("server"),
            journal: dir.join("journal"),
            log: dir.join("log.txt"),
            pid_file: dir.join("server.pid"),
            ws_root: dir.join("ws"),
            cache: dir.join("cache"),
            home: dir.join("home"),
            tickets: dir.join("p4tickets.txt"),
            enviro: dir.join("p4enviro"),
            trust: dir.join("p4trust.txt"),
            dir,
        }
    }

    /// client 的工作区目录。p4d 的 client Root 会指到这里。
    pub fn client_root(&self, client: &str) -> PathBuf {
        self.ws_root.join(client)
    }

    /// 建出实例需要用到的目录。
    pub fn create(&self) -> io::Result<()> {
        for dir in [
            &self.dir,
            &self.server,
            &self.ws_root,
            &self.cache,
            &self.home,
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }

    /// 递归删掉整个实例目录。幂等，失败不 panic（清理路径上的错误不该盖住
    /// 用例本身的失败），但会打一行说明。
    ///
    /// 退避重试是必需的，不是保险起见：p4d 刚被强杀时，Windows 上文件句柄的
    /// 释放比 `Child::wait` 返回晚一拍，紧接着删目录会删到一半停下——
    /// 实测到的形态是文件都没了、只剩 `server/`、`ws/` 这些空目录。
    /// 并行跑整个测试集时才会撞上，单个文件跑不出来。
    pub fn remove(&self) {
        let mut delay = Duration::from_millis(20);
        for attempt in 0..6 {
            match std::fs::remove_dir_all(&self.dir) {
                Ok(()) => return,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return,
                Err(error) if attempt == 5 => {
                    eprintln!("could not remove {}: {error}", self.dir.display());
                }
                Err(_) => {
                    std::thread::sleep(delay);
                    delay *= 2;
                }
            }
        }
    }
}

/// 实例名会拼进路径，且失败清理会递归删目录，所以只放行白名单字符。
fn assert_safe_instance_name(name: &str) {
    assert!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "unsafe instance name: {name:?}"
    );
}

/// `path` 是否就是 `base` 或它的子孙。按路径分量比较，不是字符串前缀。
pub fn is_inside(path: &Path, base: &Path) -> bool {
    path.starts_with(base)
}

/// 每次调用返回一个进程内唯一的实例名。
pub fn unique_instance_name() -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    format!(
        "sbx-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    )
}
