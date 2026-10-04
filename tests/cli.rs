//! 黑盒 CLI 测试：只通过进程边界观察，不引用 crate 内部符号。
//!
//! 这些用例都不需要 p4：它们要么在参数解析阶段就结束，要么在「路径都用不上」这条
//! 不碰服务器的分支上失败（一个可用的路径都没有时整轮退出码 1）。
//!
//! 让它们不依赖机器上有没有 p4 的是 `cli()` 里的 `P4COMMANDCHARSET`，不是命令行的
//! `--charset utf8`：`src/charset.rs` 的跳过条件要求 `P4CHARSET` 与 `P4COMMANDCHARSET`
//! **都有**环境变量，命令行给的那个值不参与判断。少了它每条用例都会白起一个 `p4 set`。

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

/// 起一个命令行。清掉 P4CLIENT，否则本机的值会让「没有工作区」的用例失真。
/// P4_EXE 同理：它是生产代码定位 p4 的第一顺位，本机残留一个值会改变下面每条用例的走向。
/// `P4COMMANDCHARSET` 的用处见模块文档。
fn cli() -> Command {
    let mut cmd = Command::cargo_bin("p4delta").expect("binary must build");
    cmd.env_remove("P4CLIENT");
    cmd.env_remove("P4_EXE");
    cmd.env("P4COMMANDCHARSET", "utf8");
    cmd
}

#[test]
fn help_lists_the_flags_that_exist() {
    cli()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage:"))
        .stdout(predicate::str::contains("--workspace"))
        .stdout(predicate::str::contains("--clean"))
        .stdout(predicate::str::contains("--sync"))
        .stdout(predicate::str::contains("--to"))
        .stdout(predicate::str::contains("--no-prune-ignored-dirs"));
}

/// `--clean` 与 `--sync` 方向相反到无法同时满足：一个拿 depot 覆盖工作区并且**删**
/// depot 里没有的文件，一个不删。静默择一就是误操作，所以是用法错误而不是告警。
#[test]
fn clean_and_sync_together_are_a_usage_error() {
    cli()
        .args(["--clean", "--sync", "-w", "some-workspace"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--sync"))
        .stderr(predicate::str::contains("--clean"));
}

/// `--to` / `--verify-all` 只在 sync 模式下有意义，单给是用法错误：
/// 静默忽略会让「--to 12345」看起来生效了，而实际同步的是 head。
#[test]
fn sync_only_flags_without_sync_are_a_usage_error() {
    cli()
        .args(["--to", "12345", "-w", "some-workspace"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--to"));

    cli()
        .args(["--verify-all", "-w", "some-workspace"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--verify-all"));
}

/// clean 不打开任何文件，`-c` 对它没有意义。但它也不该是个用法错误——
/// 直接报错会让「预演时顺手带上 -c」的习惯用法失效，所以是告警加忽略。
/// 退出码 1 来自那条用不上的路径参数，与本用例关心的告警无关。
#[test]
fn clean_ignores_the_changelist_flag_with_a_warning() {
    cli()
        .args([
            "--clean",
            "-w",
            "some-workspace",
            "-c",
            "5",
            "--charset",
            "utf8",
        ])
        .arg("this-path-does-not-exist-9f3c1e")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--changelist 5 is ignored"))
        .stdout(predicate::str::contains("Clean mode"))
        .stdout(predicate::str::contains("Using pending changelist 5").not());
}

/// sync 同样不打开文件，`-c` 对它也没有意义。但告警里要点出 `--to`——
/// 「指定目标版本」正是用户最容易顺手写成 `-c` 的东西。
#[test]
fn sync_ignores_the_changelist_flag_with_a_warning() {
    cli()
        .args([
            "--sync",
            "-w",
            "some-workspace",
            "-c",
            "5",
            "--charset",
            "utf8",
        ])
        .arg("this-path-does-not-exist-9f3c1e")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--changelist 5 is ignored"))
        .stderr(predicate::str::contains("--to"))
        .stdout(predicate::str::contains("Sync mode"))
        .stdout(predicate::str::contains("Using pending changelist 5").not());
}

/// `-w` 必须仍然是 `--workspace` 的短名：P4V 的集成配置里写的是 `-w $c -l %D`。
#[test]
fn the_w_short_flag_still_means_workspace() {
    cli()
        .args(["-w", "some-workspace", "--charset", "utf8"])
        .arg("this-path-does-not-exist-9f3c1e")
        .assert()
        .code(1)
        .stdout(predicate::str::contains(
            "Using workspace \"some-workspace\"",
        ));
}

#[test]
fn version_comes_from_cargo() {
    // 版本号在 clap 里是 env!("CARGO_PKG_VERSION")，这里断言的就是那个值，
    // 二者不一致说明又多了一处手写副本。
    cli()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn unknown_flag_is_a_usage_error() {
    cli()
        .arg("--definitely-not-a-flag")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--definitely-not-a-flag"));
}

#[test]
fn missing_workspace_is_a_run_error() {
    cli()
        .arg("--charset")
        .arg("utf8")
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "error: No workspace found, use -w or set P4CLIENT.",
        ));
}

/// 一个路径都不给也是错误。P4V 的 prompt 留空时命令行上就是这种形状。
#[test]
fn a_run_without_a_path_is_an_error() {
    cli()
        .args(["-w", "some-workspace", "--charset", "utf8"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("No path given"));
}

/// `P4_EXE` 指到不存在的文件是**配置错误**，不是「这台机器没有 p4」：后者由各调用点
/// 降级处理（比如路径不存在就跳过），前者必须在做任何事之前就报出来——否则这个开关
/// 会悄悄回落到系统里另一份 p4，等于没设。
#[test]
fn a_p4_exe_that_does_not_exist_is_a_config_error() {
    cli()
        .env("P4_EXE", "this-p4-exe-does-not-exist-4b8d2a")
        .args(["--charset", "utf8"])
        .arg("this-path-does-not-exist-9f3c1e")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("P4_EXE"))
        // 报错要发生在碰路径之前。不能断言 stderr 里没有 "does not exist"——
        // `src/locate.rs` 的 P4_EXE 报错原文里就有；用这个路径参数自己的名字才不恒真。
        .stderr(predicate::str::contains("this-path-does-not-exist-9f3c1e").not());
}

/// 一个只装了 `.p4delta-scope` 的临时目录。
///
/// 范围配置的**解析与报错**全在 `evaluate_scope`（`src/scope.rs:497`）里发生，
/// 早于 `src/lib.rs:156` 的 `reconcile_scope`——所以这一类用例一个 p4 进程都不需要，
/// 没必要为它们各起一个 p4d 沙箱。名字带 pid：nextest 是 process-per-test 且并行调度，
/// 固定名会让同时跑的进程互相踩。
struct ScopeDir {
    path: PathBuf,
}

impl ScopeDir {
    fn new(name: &str, scope: &str) -> Self {
        let path = std::env::temp_dir().join(format!("p4delta-cli-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        std::fs::write(path.join(".p4delta-scope"), scope).expect("write scope file");
        Self { path }
    }

    /// 起一个以该目录为工作目录的命令行——范围配置是从 cwd 往上找的。
    fn cli(&self) -> Command {
        let mut cmd = cli();
        cmd.current_dir(&self.path);
        cmd
    }
}

impl Drop for ScopeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 传入的路径与配置范围完全不相交时什么都没得做，报错退出而不是静默成功。
#[test]
fn a_scope_that_does_not_overlap_the_given_paths_is_a_failure() {
    let dir = ScopeDir::new("no-overlap", "src\n");

    dir.cli()
        .args(["-w", "some-workspace", "-a", "-l"])
        .arg("readme.txt")
        .assert()
        .failure()
        .stderr(predicate::str::contains("do not overlap"));
}

/// 交集空掉的另一种成因是排除项把入口自己排掉了：报错里要把排除项列出来。
///
/// 不列的话这条消息看着自相矛盾——`scope: src` 配 `given: src/deep` 明明相交。
#[test]
fn a_scope_entry_that_exclusions_swallow_reports_the_exclusions() {
    let dir = ScopeDir::new("excluded", "src\n-src/deep\n");

    dir.cli()
        .args(["-w", "some-workspace", "-a", "-l"])
        .arg("src/deep")
        .assert()
        .failure()
        // 分隔符两个平台都认：报错里是本地路径的原样，Windows 上全是反斜杠。
        .stderr(predicate::str::is_match(r"excluded: -.*src[\\/]deep").unwrap());
}

/// 只有注释的配置等同于没有配置：不能因为「配置存在」就把配置文件所在目录整个当成范围。
#[test]
fn an_empty_scope_file_is_ignored() {
    let dir = ScopeDir::new("empty-scope", "# 还没想好\n\n");

    // 不给路径、配置又是空的：等于什么都没给，报错而不是默默扫遍整个目录。
    dir.cli()
        .args(["-w", "some-workspace", "-a", "-l"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no entries"))
        .stderr(predicate::str::contains("No path given"));
}
