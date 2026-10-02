//! p4 可执行文件在哪。
//!
//! 顺序是 `P4_EXE` → `PATH` → P4V 安装目录。`PATH` 排在 P4V 目录之前是刻意的：
//! 绝大多数机器上 p4 本来就在 `PATH` 里，那是零意外的一条；P4V 的安装器把命令行
//! 客户端作为**可选组件**，没勾那一项、或者装了但没进 `PATH` 的机器才轮到兜底。
//!
//! 与 `tests/support/tools.rs` 的探测是两套独立的实现，见那边的注释。

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Result, anyhow};

/// 解析结果只算一次。`PATH` 在进程生命周期里不会变，重复探测只是白跑 syscall。
static P4_EXE: OnceLock<std::result::Result<PathBuf, String>> = OnceLock::new();

/// p4 的完整路径。找不到时给的是给人看的一句话，strict 还是降级由调用方决定。
pub(crate) fn p4_exe() -> Result<&'static Path> {
    match P4_EXE
        .get_or_init(|| resolve_p4_exe(env::var_os("P4_EXE"), env::var_os("PATH"), &install_dirs()))
    {
        Ok(path) => Ok(path.as_path()),
        Err(reason) => Err(anyhow!("{reason}")),
    }
}

/// 校验 `P4_EXE`。没设时是空操作——「这台机器没有 p4」不在这里报错，
/// 那条路径各调用点有自己的 strict / lenient 策略。
///
/// 设了却指向不存在的文件是**配置错误**，要在任何输出之前说出来：让它悄悄回落到
/// 系统里另一份 p4，既违背设置它的意图，也让「强制用某一份 p4」失去开关。
pub(crate) fn check_p4_exe_env() -> Result<()> {
    if explicit_p4_exe().is_none() {
        return Ok(());
    }

    // 与后面真正的调用共用同一份缓存，结论不会自相矛盾。
    p4_exe().map(|_| ())
}

/// `P4_EXE` 的值。空串按没设处理，与 p4 自己对待空环境变量的态度一致
/// （见 `charset::non_empty_env`）。
fn explicit_p4_exe() -> Option<OsString> {
    let value = env::var_os("P4_EXE")?;
    (!value.is_empty()).then_some(value)
}

/// 探测顺序的全部逻辑。环境与候选目录都从参数来，测试直接喂值、不动进程环境——
/// 改进程环境是全局操作，会和并行跑的用例互相踩（与 `charset::resolve_p4_charset`
/// 同一套做法）。
fn resolve_p4_exe(
    explicit: Option<OsString>,
    path_var: Option<OsString>,
    install_dirs: &[PathBuf],
) -> std::result::Result<PathBuf, String> {
    // 显式指定的路径是独占的：是文件就用它，不是就是配置错误，不往下翻。
    if let Some(explicit) = explicit.filter(|value| !value.is_empty()) {
        let path = absolute(PathBuf::from(&explicit));
        return path.is_file().then_some(path).ok_or_else(|| {
            format!(
                "P4_EXE is set to \"{}\", but that file does not exist.",
                Path::new(&explicit).display()
            )
        });
    }

    if let Some(found) = path_var
        .as_deref()
        .and_then(|path_var| find_in_path(path_var, exe_name()))
    {
        return Ok(absolute(found));
    }

    if let Some(found) = install_dirs
        .iter()
        .map(|dir| dir.join(exe_name()))
        .find(|candidate| candidate.is_file())
    {
        return Ok(absolute(found));
    }

    Err(not_found_message())
}

/// 在 `PATH` 的各个目录里找 `name`。
fn find_in_path(path_var: &OsStr, name: &str) -> Option<PathBuf> {
    env::split_paths(path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// 绝对化。子进程带着自己的 `current_dir`，相对路径的程序在 Windows 上按**父进程**的
/// cwd 解析、在 Unix 上按**子进程**的 cwd 解析——同一个配置两边含义不同，绝对化之后才
/// 一致（`src/path.rs` 对路径参数同样这么做）。解析不出来时退回原值：那说明 cwd 本身
/// 有问题，报那个路径比报一个含糊的 spawn 失败有用。
fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

/// 可执行文件在各平台上的名字。
fn exe_name() -> &'static str {
    if cfg!(windows) { "p4.exe" } else { "p4" }
}

/// P4V 安装目录下 p4 的候选位置，顺序即尝试顺序。
///
/// 用 `ProgramFiles` 环境变量而不是写死 `C:\Program Files`：系统盘不在 C 时那个常量
/// 就是错的，32 位的 P4V 也装在另一棵树里。
#[cfg(windows)]
fn install_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        let Some(base) = env::var_os(var) else {
            continue;
        };
        let base = PathBuf::from(base);
        // DVCS 是 P4V 自带那一份的落点（p4d 也在那里），Perforce 根目录是单独装了
        // 命令行客户端时的落点，两个都看。
        dirs.push(base.join("Perforce").join("DVCS"));
        dirs.push(base.join("Perforce"));
    }

    dirs
}

#[cfg(not(windows))]
fn install_dirs() -> Vec<PathBuf> {
    Vec::new()
}

/// 找不到 p4 时给用户的话。要说清楚**找过哪些地方**，否则「我明明装了 p4」会变成
/// 一场没有线索的排查。
fn not_found_message() -> String {
    #[cfg(windows)]
    let looked_at = "P4_EXE, PATH, and the P4V install directories under Program Files";
    #[cfg(not(windows))]
    let looked_at = "P4_EXE and PATH";

    format!(
        "p4 was not found; looked at {looked_at}. Install the P4 command-line client \
         (P4V's installer can include it) or point P4_EXE at {}.",
        exe_name()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempTree;

    /// 造一份假 p4。探测只看它是不是文件，内容无所谓。
    fn fake_p4(tree: &TempTree, relative: &str) -> PathBuf {
        tree.file(relative, "")
    }

    #[test]
    fn an_explicit_p4_exe_wins_over_everything_else() {
        let tree = TempTree::new("locate-explicit");
        let explicit = fake_p4(&tree, "explicit/p4.exe");
        fake_p4(&tree, "on-path/p4.exe");
        let install = tree.dir("installed");
        fake_p4(&tree, "installed/p4.exe");

        let resolved = resolve_p4_exe(
            Some(explicit.as_os_str().to_owned()),
            Some(tree.dir("on-path").into_os_string()),
            &[install],
        );

        assert_eq!(resolved.unwrap(), explicit);
    }

    #[test]
    fn path_is_preferred_over_the_p4v_install_directories() {
        let tree = TempTree::new("locate-path");
        let from_path = fake_p4(&tree, "bin/p4.exe");
        let install = tree.dir("installed");
        fake_p4(&tree, "installed/p4.exe");

        let resolved = resolve_p4_exe(None, Some(tree.dir("bin").into_os_string()), &[install]);

        assert_eq!(resolved.unwrap(), from_path);
    }

    /// P4V 安装器把命令行客户端作为可选组件，没装那份 CLI 的机器上 `PATH` 里没有 p4，
    /// 这条兜底是那时唯一还能工作的路径。
    #[test]
    fn the_p4v_install_directory_is_the_last_resort() {
        let tree = TempTree::new("locate-install");
        let missing = tree.dir("missing");
        let install = tree.dir("installed");
        let expected = fake_p4(&tree, "installed/p4.exe");

        let resolved = resolve_p4_exe(
            None,
            Some(tree.dir("empty-path").into_os_string()),
            &[missing, install],
        );

        assert_eq!(resolved.unwrap(), expected);
    }

    /// `PATH` 本身没设时不该 panic，直接落到安装目录。
    #[test]
    fn a_missing_path_variable_falls_through_to_the_install_directories() {
        let tree = TempTree::new("locate-no-path");
        let install = tree.dir("installed");
        let expected = fake_p4(&tree, "installed/p4.exe");

        assert_eq!(resolve_p4_exe(None, None, &[install]).unwrap(), expected);
    }

    /// 设了 `P4_EXE` 却指向不存在的文件是配置错误，不能悄悄回落到 `PATH` 里那一份。
    #[test]
    fn an_explicit_path_that_does_not_exist_is_an_error() {
        let tree = TempTree::new("locate-missing-explicit");
        fake_p4(&tree, "on-path/p4.exe");
        let missing = tree.root.join("nope").join(exe_name());

        let reason = resolve_p4_exe(
            Some(missing.as_os_str().to_owned()),
            Some(tree.dir("on-path").into_os_string()),
            &[],
        )
        .expect_err("P4_EXE 指错必须失败，哪怕 PATH 里有可用的 p4");

        assert!(reason.contains("P4_EXE"), "{reason}");
        assert!(reason.contains("nope"), "{reason}");
    }

    /// 空串按没设处理。
    #[test]
    fn an_empty_explicit_path_means_unset() {
        let tree = TempTree::new("locate-empty-explicit");
        let expected = fake_p4(&tree, "bin/p4.exe");

        let resolved = resolve_p4_exe(
            Some(OsString::new()),
            Some(tree.dir("bin").into_os_string()),
            &[],
        );

        assert_eq!(resolved.unwrap(), expected);
    }

    #[test]
    fn nothing_anywhere_is_an_error_that_says_where_we_looked() {
        let tree = TempTree::new("locate-none");

        let reason = resolve_p4_exe(None, Some(tree.dir("empty-path").into_os_string()), &[])
            .expect_err("哪儿都没有 p4 时必须失败");

        assert!(reason.contains("P4_EXE"), "{reason}");
        assert!(reason.contains("PATH"), "{reason}");
        assert!(reason.contains("P4V"), "{reason}");
    }
}
