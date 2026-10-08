//! 黑盒 CLI 测试：只通过进程边界观察，不引用 crate 内部符号。
//!
//! 这些用例都不需要 p4：它们要么在参数解析阶段就结束，要么在不碰服务器的分支上失败。
//!
//! 让它们不依赖机器上有没有 p4 的是 `cli()` 里的 `P4COMMANDCHARSET`，不是命令行的
//! `--charset utf8`：`src/charset.rs` 的跳过条件要求 `P4CHARSET` 与 `P4COMMANDCHARSET`
//! **都有**环境变量，命令行给的那个值不参与判断。少了它每条用例都会白起一个 `p4 set`。
//!
//! **范围求值不在这里**：范围配置挂在 client root 上，「这一轮是哪个 root」要先问 p4，
//! 所以配置解析与否、交集空不空都要真实连接才谈得上。那些用例在 `tests/e2e_scope.rs`
//! 的沙箱里（另有一批纯函数的在 `src/scope.rs` 的单元测试与共享契约向量里）。

use assert_cmd::Command;
use predicates::prelude::*;

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
        .stdout(predicate::str::contains("--no-prune-ignored-dirs"))
        .stdout(predicate::str::contains("--client-root"))
        .stdout(predicate::str::contains("--no-scope-file"))
        .stdout(predicate::str::contains("--exclude-dir"))
        .stdout(predicate::str::contains("--exclude-file"));
}

/// 被撤掉的那套「机器可读范围」开关不该悄悄回来：`--scope-report` 之类现在是未知选项。
#[test]
fn the_removed_scope_flags_are_unknown() {
    for flag in [
        "--scope-report",
        "--scope-from",
        "--scope-request",
        "--scope-snapshot",
    ] {
        cli()
            .args(["-w", "some-workspace", flag])
            .assert()
            .code(2)
            .stderr(predicate::str::contains(flag));
    }
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

/// `P4_EXE` 指到不存在的文件是**配置错误**，不是「这台机器没有 p4」：后者由各调用点
/// 降级处理（比如路径不存在就跳过），前者必须在做任何事之前就报出来——否则这个开关
/// 会悄悄回落到系统里另一份 p4，等于没设。
///
/// （不给路径是另一种错误，但那条要真实连接才谈得上——见
/// `tests/e2e_scope.rs::a_run_without_a_path_is_an_error`。）
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

/// 普通同步（`--sync` 不带 `--force`）的范围来自 client view、每个动作由原生判定，
/// 本地排除项在那条路上没有可生效的地方——静默忽略比报错糟得多：调用方以为那条子树
/// 不会被碰，而 p4 **会**去动它。这条检查在最前面，不需要连接就能得出结论。
#[test]
fn a_normal_sync_rejects_the_local_exclusions() {
    for flag in ["--exclude-dir", "--exclude-file"] {
        cli()
            .args(["--sync", "-w", "some-workspace", flag, "some/path"])
            .assert()
            .code(1)
            .stderr(predicate::str::contains(flag))
            .stderr(predicate::str::contains("--clean"));
    }

    // 给 `--force`（或换个模式）就有地方生效了：不在这里挡，而是往下走去问 p4。
    // 这里只断言「不是因为这条检查失败」——没有连接时它照样会退，但那是另一条路。
    let output = cli()
        .args([
            "--sync",
            "--force",
            "-w",
            "some-workspace",
            "--exclude-dir",
            "some/path",
        ])
        .arg("some-path")
        .output()
        .expect("run tool");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("not accepted by a normal sync"),
        "带 --force 时不该被这条检查拦下: {stderr}"
    );
}
