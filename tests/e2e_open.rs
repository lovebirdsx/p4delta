//! 八类变更的端到端验证：造出离线改动，看 dry run 报什么，再用 `-a` 落盘，
//! 最后回服务器独立取证。
//!
//! 每个用例一个独立的 p4d 实例，所以它们可以并行跑、互不干扰。
//!
//! 断言分两层：分类结果看 stdout（那是给用户看的契约），
//! **真实状态一律用沙箱自己的 `p4 opened` 取证**——只看 stdout 的话，
//! 「报告得对但下发的 p4 命令是错的」这种缺陷会被漏掉。

mod support;

use predicates::prelude::*;

use support::listed_changes;

/// 改一个已同步的文件。dry run 只报告，`-a` 才真的 open for edit。
#[test]
fn an_edited_file_is_reported_and_applied_as_edit() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("readme.txt", "changed locally\n");

    sandbox
        .cli()
        .arg("-l")
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Editing 1 files"))
        .stdout(predicate::str::contains("Re-run with -a to apply changes."));

    assert!(
        sandbox.opened().is_empty(),
        "dry run must not open anything on the server"
    );

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Inconsistencies fixed."));

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "unexpected opened files: {opened:?}");
    assert!(opened[0].contains("readme.txt"), "{opened:?}");
    assert!(opened[0].contains(" - edit "), "{opened:?}");
}

/// 工作区里多出一个 depot 没有的文件。
#[test]
fn a_new_file_is_reported_and_applied_as_add() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("fresh.txt", "brand new\n");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Adding 1 files"));

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "unexpected opened files: {opened:?}");
    assert!(opened[0].contains("fresh.txt"), "{opened:?}");
    assert!(opened[0].contains(" - add "), "{opened:?}");
}

/// 本地删掉一个已同步的文件。`-k` 让 p4 保留本地文件，所以这里不会真的删盘。
#[test]
fn a_deleted_file_is_reported_and_applied_as_delete() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.remove("src/lib.txt");
    assert!(!sandbox.exists("src/lib.txt"));

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleting 1 files"));

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "unexpected opened files: {opened:?}");
    assert!(opened[0].contains("lib.txt"), "{opened:?}");
    assert!(opened[0].contains(" - delete "), "{opened:?}");
}

/// 打开了但内容没动：这属于「多余的打开」，reconcile 要把它撤掉。
#[test]
fn an_unmodified_opened_file_is_reverted() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.p4_ok(&["edit", "readme.txt"]);
    assert_eq!(sandbox.opened().len(), 1);

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Revert Edit"));

    assert!(
        sandbox.opened().is_empty(),
        "an unchanged check-out should have been reverted: {:?}",
        sandbox.opened()
    );
    assert!(sandbox.exists("readme.txt"), "revert -k must keep the file");
}

/// 已 checkout 待删除，本地却又改了内容——该改成待编辑，而不是删掉改动。
#[test]
fn a_changed_file_opened_for_delete_is_reopened_as_edit() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // `-k` 保留本地文件，只是把它标记成待删除。
    sandbox.p4_ok(&["delete", "-k", "readme.txt"]);
    sandbox.write("readme.txt", "changed after being marked for delete\n");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Reopen Edit"));

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].contains(" - edit "), "{opened:?}");
    assert_eq!(
        sandbox.read("readme.txt"),
        "changed after being marked for delete\n"
    );
}

/// 已 checkout 待编辑，本地却把文件删了——该改成待删除。
#[test]
fn a_deleted_file_opened_for_edit_is_reopened_as_delete() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.p4_ok(&["edit", "readme.txt"]);
    sandbox.remove("readme.txt");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Reopen Delete"));

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].contains(" - delete "), "{opened:?}");
}

/// 加进 depot 的文件又被本地删了：这个 add 已经没有意义，撤掉。
#[test]
fn a_missing_file_opened_for_add_is_reverted() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("fresh.txt", "brand new\n");
    sandbox.p4_ok(&["add", "fresh.txt"]);
    sandbox.remove("fresh.txt");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Revert Add"));

    assert!(
        sandbox.opened().is_empty(),
        "the stale add should have been reverted: {:?}",
        sandbox.opened()
    );
}

/// 标了待删除，本地文件其实没动：这个 delete 同样没有意义。
#[test]
fn a_restored_file_opened_for_delete_is_reverted() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.p4_ok(&["delete", "-k", "readme.txt"]);
    assert_eq!(sandbox.opened().len(), 1);

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Revert Delete"));

    assert!(
        sandbox.opened().is_empty(),
        "the stale delete should have been reverted: {:?}",
        sandbox.opened()
    );
    assert_eq!(sandbox.read("readme.txt"), "hello from the depot\n");
}

/// dry run 是只读的：服务器不 open 任何文件，磁盘上的内容也不许动。
#[test]
fn dry_run_leaves_the_workspace_untouched() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("readme.txt", "changed locally\n");
    sandbox.write("fresh.txt", "brand new\n");
    sandbox.remove("src/lib.txt");

    sandbox
        .cli()
        .arg("-l")
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Counted 3 changes"))
        .stdout(predicate::str::contains(
            "Inconsistencies found. Re-run with -a to apply changes.",
        ));

    assert!(
        sandbox.opened().is_empty(),
        "dry run must not open anything"
    );
    assert_eq!(sandbox.read("readme.txt"), "changed locally\n");
    assert_eq!(sandbox.read("fresh.txt"), "brand new\n");
    assert!(!sandbox.exists("src/lib.txt"));
}

/// 一个 p4 收不下的文件名不该让整轮停摆：其余各类照做，最后统一报错并非零退出。
///
/// `@` 在 p4 的参数语法里是版本说明符（`file@rev`、`@2024/01/01`），名字里带它的路径
/// p4 一律拒收——这是用户改不动、p4delta 也无解的一类文件。以前它只换到一行 stderr 警告，
/// 程序照样打印 "Inconsistencies fixed." 并退出 0；收紧成 strict 之后也必须只失败这一组，
/// 不能让另外几百个文件陪着一起不做。
#[test]
fn a_rejected_file_name_fails_the_run_without_stopping_the_other_groups() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // Add 类：p4 收不下这个名字。
    sandbox.write("report@2024.txt", "bad name\n");
    // Edit 类：与它无关，必须照常完成。
    sandbox.write("readme.txt", "changed locally\n");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(".")
        .assert()
        .failure()
        .stdout(predicate::str::contains("Adding 1 files"))
        .stdout(predicate::str::contains("Editing 1 files"))
        .stderr(predicate::str::contains(
            "Failed to apply 1 change group(s)",
        ))
        .stderr(predicate::str::contains("Add (1 files)"));

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "Edit 类必须照做：{opened:?}");
    assert!(opened[0].contains("readme.txt"), "{opened:?}");
    assert!(opened[0].contains(" - edit "), "{opened:?}");
}

/// 与 p4 自己的判定对照：同一组离线改动，两边找出的变更集合必须一致。
///
/// 只用「没有打开过任何文件」的场景，这样八类标签与 p4 的四个动作名一一对应，
/// 归一化只剩路径形式上的差异。
#[test]
fn the_reported_change_set_matches_p4_reconcile() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("readme.txt", "changed locally\n");
    sandbox.write("fresh.txt", "brand new\n");
    sandbox.remove("src/lib.txt");

    let ours = sandbox.cli().arg("-l").arg(".").output().expect("run tool");
    assert!(ours.status.success(), "{ours:?}");
    let ours = normalize(&listed_changes(&String::from_utf8_lossy(&ours.stdout)));

    let theirs = normalize(&reconcile_n(&sandbox.p4_ok(&["reconcile", "-n", "..."])));

    // basename 唯一是这套归一化的前提：重名的话两个不同的文件会互相掩盖，
    // 对照就悄悄失效了。种子将来加了同名文件，这里会先炸出来，
    // 而不是让断言在不知不觉中变成摆设。
    let mut names: Vec<&str> = ours.iter().map(|(_, name)| name.as_str()).collect();
    let total = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(
        names.len(),
        total,
        "basenames must be unique for this comparison: {names:?}"
    );

    assert_eq!(ours, theirs, "our change set diverges from p4 reconcile -n");
    assert_eq!(ours.len(), 3, "expected three changes: {ours:?}");
}

/// 把 (动作, 路径) 归一成 (小写动作, 文件名) 并排序。
///
/// 两边一个报本地路径、一个报 depot 路径，中间隔着 client view 的映射；
/// 这里只关心「哪些文件、什么动作」，文件名足以把差异逼出来。
fn normalize(changes: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = changes
        .iter()
        .map(|(action, path)| {
            let path = path.replace('\\', "/");
            let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
            (action.to_lowercase(), name)
        })
        .collect();
    out.sort();
    out
}

/// 解析 `p4 reconcile -n` 的输出，行形如 `//depot/main/fresh.txt#1 - opened for add`。
///
/// 预演模式下 p4 仍然按「会怎么打开文件」措辞，动作名前面挂着 `opened for `。
fn reconcile_n(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter_map(|line| {
            let (left, action) = line.rsplit_once(" - ")?;
            let (depot_file, _revision) = left.rsplit_once('#')?;
            let action = action.trim();
            let action = action.strip_prefix("opened for ").unwrap_or(action);
            Some((action.to_owned(), depot_file.to_owned()))
        })
        .collect()
}
