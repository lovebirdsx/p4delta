//! p4 / p4d 可执行文件的探测。

use std::env;
use std::path::{Path, PathBuf};

use super::paths::{e2e_dir, project_root};

/// 一对配套的 p4 客户端与服务端。
pub struct Tools {
    pub p4: PathBuf,
    pub p4d: PathBuf,
}

impl Tools {
    /// p4 所在的目录。沙箱会把它插到 PATH 最前面——生产代码的定位顺序是
    /// `P4_EXE` → `PATH` → P4V 安装目录，插在最前就能让第 2 步稳定命中这一份。
    pub fn bin_dir(&self) -> PathBuf {
        self.p4
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// 探测 p4d 与 p4。两者都找到才返回 `Some`，否则 `None`，由调用方打印
/// 一行 `skipping:` 后跳过——与 `src/test_util.rs` 的 `p4_available()` 同一套约定。
///
/// `P4D_EXE` / `P4_EXE` 是**独占**的：指定了就不再往下翻。否则设一个不存在的
/// 路径仍然会去用系统安装的那一份，既违背意图，也让「强制跳过 e2e」
/// （比如机器上的 p4d 版本不对）没有开关可用。
///
/// 与 `src/locate.rs` 是两套独立的实现，刻意不合并：这里要的是**配套的一对**
/// p4 + p4d（客户端与服务端主版本必须匹配），顺序是 vendor/ → target/e2e/tools →
/// P4V 目录 → PATH；生产只要 p4 一个，且 `PATH` 优先。合并只会为了共用函数堆参数。
/// 两边对「`P4_EXE` 指到不存在的路径」也有不同处置：这里是强制跳过，
/// 生产是配置错误（见 `locate::check_p4_exe_env`）。
pub fn discover() -> Option<Tools> {
    let p4d = match env::var_os("P4D_EXE") {
        Some(path) => existing(PathBuf::from(path))?,
        None => first_existing(p4d_candidates())?,
    };
    // p4 优先取与 p4d 同目录的那份：客户端与服务端的主版本必须匹配。
    let p4 = match env::var_os("P4_EXE") {
        Some(path) => existing(PathBuf::from(path))?,
        None => first_existing(p4_candidates(&p4d))?,
    };
    Some(Tools { p4, p4d })
}

/// 显式指定的路径要么就是它，要么没有。
fn existing(path: PathBuf) -> Option<PathBuf> {
    path.is_file().then_some(path)
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

fn p4d_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    out.push(project_root().join("vendor").join(exe("p4d")));
    out.push(e2e_dir().join("tools").join(exe("p4d")));
    #[cfg(windows)]
    {
        // P4V 自带的 p4d 装在 DVCS 子目录里，单独一份的装在 Perforce 根下。
        out.push(PathBuf::from(r"C:\Program Files\Perforce\DVCS").join(exe("p4d")));
        out.push(PathBuf::from(r"C:\Program Files\Perforce").join(exe("p4d")));
    }
    if let Some(path) = find_in_path(&exe("p4d")) {
        out.push(path);
    }
    out
}

fn p4_candidates(p4d: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = p4d.parent() {
        out.push(dir.join(exe("p4")));
    }
    out.push(project_root().join("vendor").join(exe("p4")));
    out.push(e2e_dir().join("tools").join(exe("p4")));
    #[cfg(windows)]
    {
        out.push(PathBuf::from(r"C:\Program Files\Perforce").join(exe("p4")));
    }
    if let Some(path) = find_in_path(&exe("p4")) {
        out.push(path);
    }
    out
}

fn first_existing(candidates: Vec<PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|path| path.is_file())
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}
