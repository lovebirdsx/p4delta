//! 黑盒 CLI 测试：只通过进程边界观察，不引用 crate 内部符号。
//!
//! 这些用例都不需要 p4：它们要么在参数解析阶段就结束，要么走的是
//! 「路径不存在就跳过」这条不碰服务器的分支。传 `--charset utf8` 是为了
//! 让 `init_p4_encoding` 不去调 `p4 set`，用例因此不依赖机器上有没有 p4。

use assert_cmd::Command;
use predicates::prelude::*;

/// 起一个命令行。清掉 P4CLIENT，否则本机的值会让「没有工作区」的用例失真。
/// P4_EXE 同理：它是生产代码定位 p4 的第一顺位，本机残留一个值会改变下面每条用例的走向。
fn cli() -> Command {
    let mut cmd = Command::cargo_bin("p4delta").expect("binary must build");
    cmd.env_remove("P4CLIENT");
    cmd.env_remove("P4_EXE");
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
        .stdout(predicate::str::contains("--no-prune-ignored-dirs"));
}

/// clean 不打开任何文件，`-c` 对它没有意义。但它也不该是个用法错误——
/// 直接报错会让「预演时顺手带上 -c」的习惯用法失效，所以是告警加忽略。
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
        .success()
        .stderr(predicate::str::contains("--changelist 5 is ignored"))
        .stdout(predicate::str::contains("Clean mode"))
        .stdout(predicate::str::contains("Using pending changelist 5").not());
}

/// `-w` 必须仍然是 `--workspace` 的短名：P4V 的集成配置里写的是 `-w $c -l %D`。
#[test]
fn the_w_short_flag_still_means_workspace() {
    cli()
        .args(["-w", "some-workspace", "--charset", "utf8"])
        .arg("this-path-does-not-exist-9f3c1e")
        .assert()
        .success()
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

#[test]
fn a_path_that_does_not_exist_is_skipped() {
    cli()
        .args(["-w", "some-workspace", "--charset", "utf8"])
        .arg("this-path-does-not-exist-9f3c1e")
        .assert()
        .success()
        .stdout(predicate::str::contains("doesn't exist."));
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
        .stdout(predicate::str::contains("doesn't exist.").not());
}
