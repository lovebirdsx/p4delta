//! 非 ASCII 文件名的完整链路，以及 `-l` / `-v` 的输出契约。

mod support;

use predicates::prelude::*;

/// 中文文件名从写盘、p4 入库、扫盘、摘要，一直到报告，整条链路都得对得上。
///
/// 字符集错了的典型症状是同一个文件被**同时**报成新增和删除——
/// 因为两边的路径字符串对不上，所以这里把「恰好一条 Edit、没有别的」也钉住。
#[test]
fn non_ascii_file_names_are_reported_correctly() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("src/使用说明.txt", "改过的内容\n");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Editing 1 files"))
        .stdout(predicate::str::contains("使用说明.txt"))
        .stdout(predicate::str::contains("Adding").not())
        .stdout(predicate::str::contains("Deleting").not());

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].contains("使用说明.txt"), "{opened:?}");
    assert!(opened[0].contains(" - edit "), "{opened:?}");
    assert_eq!(sandbox.read("src/使用说明.txt"), "改过的内容\n");
}

/// 输出契约：`-l` 的清单行是九格缩进的 `标签 "路径".`；不带 `-l` 时只报计数；
/// `-v` 隐含 `-l`，而且还会多打每一条 p4 命令。
#[test]
fn list_and_verbose_flags_follow_the_output_contract() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("fresh.txt", "brand new\n");

    // -l：逐文件清单。九格缩进是契约的一部分，而 `Using workspace "x".`
    // 这类行同样以 `".` 收尾，所以直接把缩进和标签一起匹配掉。
    let output = sandbox.cli().arg("-l").arg(".").output().expect("run tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let listed = stdout
        .lines()
        .find(|line| line.starts_with("         Add \""))
        .unwrap_or_else(|| panic!("no list line in:\n{stdout}"));
    assert!(listed.ends_with("fresh.txt\"."), "{listed:?}");

    // 不带 -l：只有计数，没有清单
    sandbox
        .cli()
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Adding 1 files"))
        .stdout(predicate::str::contains("Add \"").not());

    // -v：隐含 -l，并打出下发的 p4 命令
    sandbox
        .cli()
        .arg("-v")
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Add \""))
        .stdout(predicate::str::contains("Running: p4 "));
}
