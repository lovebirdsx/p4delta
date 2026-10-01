//! `--clean` 的三类动作：方向与 open 模式相反——拿 depot 去修正工作区。
//!
//! 这里的断言同样全部落在磁盘与 `p4 opened` 上：clean 会删文件、会丢改动，
//! 只看它自己打印了什么不足以说明它做对了。

mod support;

use predicates::prelude::*;

/// depot 没有的文件：从磁盘删掉。
#[test]
fn an_untracked_file_is_deleted_from_disk() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("extra.txt", "not in the depot\n");
    sandbox.write("build/scratch.txt", "also not in the depot\n");
    assert!(sandbox.exists("extra.txt"));

    sandbox
        .cli()
        .args(["--clean", "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleting 2 files"))
        .stdout(predicate::str::contains("Workspace matches the depot."));

    assert!(!sandbox.exists("extra.txt"));
    assert!(!sandbox.exists("build/scratch.txt"));
    assert!(sandbox.opened().is_empty(), "clean never opens files");
}

/// 改过内容、但没有打开的文件：还原成 have 版本。
#[test]
fn a_modified_tracked_file_is_restored_to_have() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("readme.txt", "locally changed\n");

    sandbox
        .cli()
        .args(["--clean", "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Reverting 1 files"))
        .stdout(predicate::str::contains("Workspace matches the depot."));

    assert_eq!(sandbox.read("readme.txt"), "hello from the depot\n");
}

/// depot 有、本地没有的文件：从 depot 写回来。
#[test]
fn a_missing_tracked_file_is_restored_from_the_depot() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.remove("src/lib.txt");
    assert!(!sandbox.exists("src/lib.txt"));

    sandbox
        .cli()
        .args(["--clean", "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Restoring 1 files"))
        .stdout(predicate::str::contains("Workspace matches the depot."));

    assert_eq!(sandbox.read("src/lib.txt"), "library\n");
}

/// dry run：三类差异都数得出来，但磁盘上什么都不许动。
#[test]
fn clean_dry_run_changes_nothing() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("extra.txt", "not in the depot\n");
    sandbox.write("readme.txt", "locally changed\n");
    sandbox.remove("src/lib.txt");

    sandbox
        .cli()
        .args(["--clean", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Counted 3 files to clean"))
        .stdout(predicate::str::contains(
            "Re-run with -a to clean the workspace.",
        ));

    assert!(sandbox.exists("extra.txt"));
    assert_eq!(sandbox.read("readme.txt"), "locally changed\n");
    assert!(!sandbox.exists("src/lib.txt"));
    assert!(sandbox.opened().is_empty());
}

/// 已打开的文件不归 clean 管，`p4 clean` 的官方口径也是如此
/// （"files that are opened ... are not impacted by p4 clean"）。
///
/// 这里刻意做出两个最能说明问题的情形：打开后改了内容、打开后删了本地文件——
/// clean 都得原样留着。
#[test]
fn clean_never_touches_opened_files() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.p4_ok(&["edit", "readme.txt"]);
    sandbox.write("readme.txt", "opened and changed\n");
    sandbox.p4_ok(&["edit", "src/lib.txt"]);
    sandbox.remove("src/lib.txt");

    sandbox
        .cli()
        .args(["--clean", "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        // 两个文件都被 clean 无视，所以三类动作一个都不该出现；
        // 一个动作都没有时程序报的是这句，而不是 "Workspace matches the depot."
        .stdout(predicate::str::contains("Deleting").not())
        .stdout(predicate::str::contains("Reverting").not())
        .stdout(predicate::str::contains("Restoring").not())
        .stdout(predicate::str::contains(
            "No files to clean, everything up to date.",
        ));

    assert_eq!(sandbox.read("readme.txt"), "opened and changed\n");
    assert!(
        !sandbox.exists("src/lib.txt"),
        "clean must not write back a file that is open for edit"
    );
    assert_eq!(
        sandbox.opened().len(),
        2,
        "clean must leave the open state alone: {:?}",
        sandbox.opened()
    );
}

/// head revision 已被删除、本地文件又冒出来。
///
/// 这是 README「已知问题」里唯一标着「未在真实 depot 上实测过」的一条：
/// 要造出这个状态得往 depot 提交一次删除再让本地文件重现。沙箱的 depot 是私有的，
/// 造它不用碰任何共享仓库，所以这条现在有了实测——而且拿 `p4 clean -n` 的判定做对照，
/// 不是只看工具自己的说法。
///
/// `p4 sync` 到删除版本会把本地文件一并删掉，所以重现这一步要手工做。
#[test]
fn a_file_deleted_at_head_is_removed_from_the_workspace() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.commit("revived.txt", "will be deleted at head\n");
    sandbox.p4_ok(&["delete", "revived.txt"]);
    sandbox.p4_ok(&["submit", "-d", "delete it"]);
    sandbox.sync();

    // 前提：depot 的 head 是一次删除，而本地文件又在了。
    let fstat = sandbox.p4_ok(&["fstat", "revived.txt"]);
    assert!(
        fstat.contains("headAction delete"),
        "unexpected depot state: {fstat}"
    );
    assert!(sandbox.p4_lines(&["have", "revived.txt"]).is_empty());
    sandbox.write("revived.txt", "back from the dead\n");

    // 先取 p4 自己的判定。`-n` 是预演，不改状态，可以安全地跑在断言之前。
    // `#none` 是「这个文件在工作区里没有对应的 have 版本」。
    let theirs = sandbox.p4_ok(&["clean", "-n", "..."]);
    assert!(theirs.contains("revived.txt#none"), "{theirs}");
    assert!(theirs.contains(" - deleted as "), "{theirs}");

    sandbox
        .cli()
        .args(["--clean", "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleting 1 files"))
        .stdout(predicate::str::contains("Workspace matches the depot."));

    assert!(
        !sandbox.exists("revived.txt"),
        "p4 clean would have deleted it too"
    );
    assert!(sandbox.opened().is_empty(), "clean never opens files");
}
