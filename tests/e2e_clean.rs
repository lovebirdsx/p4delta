//! `--clean` 的三类动作：方向与 open 模式相反——拿 depot 去修正工作区。
//!
//! 这里的断言同样全部落在磁盘与 `p4 opened` 上：clean 会删文件、会丢改动，
//! 只看它自己打印了什么不足以说明它做对了。

mod support;

use predicates::prelude::*;

/// 三类动作连同预演，在一个沙箱里跑完。
///
/// 四处差异刻意落在四个互不相干的路径上，所以预演之后直接落地**不需要任何清理**：
/// 预演本来就不改状态，落地只是把同一批差异做掉。合成一条省下一次 p4d 冷启动与
/// 一次模板复制——每条 e2e 用例的固定开销都在那里，而不在断言上。
#[test]
fn the_three_clean_actions_are_counted_then_applied() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 未跟踪两份（删除）、已改一份（还原）、缺失一份（写回）。
    sandbox.write("extra.txt", "not in the depot\n");
    sandbox.write("build/scratch.txt", "also not in the depot\n");
    sandbox.write("readme.txt", "locally changed\n");
    sandbox.remove("src/lib.txt");

    // 预演：三类都数得出来，但磁盘上什么都不许动。
    sandbox
        .cli()
        .args(["--clean", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Counted 4 files to clean"))
        .stdout(predicate::str::contains(
            "Re-run with -a to clean the workspace.",
        ));

    assert!(sandbox.exists("extra.txt"));
    assert!(sandbox.exists("build/scratch.txt"));
    assert_eq!(sandbox.read("readme.txt"), "locally changed\n");
    assert!(!sandbox.exists("src/lib.txt"));
    assert!(sandbox.opened().is_empty());

    // 落地：三类动作各走一遍，计数各自钉住自己那一类。
    sandbox
        .cli()
        .args(["--clean", "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleting 2 files"))
        .stdout(predicate::str::contains("Reverting 1 files"))
        .stdout(predicate::str::contains("Restoring 1 files"))
        .stdout(predicate::str::contains("Workspace matches the depot."));

    assert!(!sandbox.exists("extra.txt"));
    assert!(!sandbox.exists("build/scratch.txt"));
    assert_eq!(sandbox.read("readme.txt"), "hello from the depot\n");
    assert_eq!(sandbox.read("src/lib.txt"), "library\n");
    assert!(sandbox.opened().is_empty(), "clean never opens files");
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

/// clean 不消费「已打开」的两组摘要候选：不读它们的文件、不算摘要、也不预热缓存。
/// `p4 clean` 本来就不碰已打开的文件，为它们读盘算出来的摘要只会被丢掉——而这恰恰是
/// 用户刚在编辑器里改过、体量最大的那些。
///
/// 两个候选都刻意做成「会进入原先摘要路径」的样子：mtime 被 `sandbox.write()` 回拨过
/// 一小时（避开时间戳捷径），沙箱的摘要缓存又是冷的。三条断言各自独立：
///
/// - 输出里没有摘要阶段的字样（`Checking digests` / `Hashed`）；
/// - 磁盘与 `p4 opened` 一个都没变（跳过不等于把它们当成不存在）；
/// - 缓存里什么都没留下——下一轮运行时无缓存可加载（由程序自己报告，不必猜缓存路径）。
#[test]
fn clean_never_hashes_files_that_are_open() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // revert_edit 候选：打开编辑、内容改了。clean 的 Revert 只管**未打开**的文件，
    // 所以这一份算出来「与 have 不同」也没有任何动作要用到它。
    sandbox.p4_ok(&["edit", "readme.txt"]);
    sandbox.write("readme.txt", "opened and changed\n");

    // revert_delete_or_reopen_edit 候选：打开删除、本地文件又冒出来。
    sandbox.p4_ok(&["delete", "src/lib.txt"]);
    sandbox.write("src/lib.txt", "back again\n");
    let mut opened_before = sandbox.opened();
    opened_before.sort();
    assert_eq!(opened_before.len(), 2);

    let output = sandbox
        .cli()
        .args(["--clean", "-a", "-l"])
        .arg(".")
        .output()
        .expect("run tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // 前置条件也要验：三个没打开的跟踪文件全部被时间戳捷径跳过。跳过数等于候选数，
    // 说明这一轮**本来就没有**任何文件需要摘要——于是下面那句「一次摘要都没算」不是
    // 被时间戳顺手掩盖出来的假象，而是真的没人进过摘要阶段。
    assert!(
        stdout.contains("Timestamp optimization: Skipped 3 of 3 digest computations (100%)"),
        "unmodified files should be skipped by the timestamp shortcut:\n{stdout}"
    );
    assert!(
        !stdout.contains("Checking digests"),
        "clean 不该为已打开的文件算摘要:\n{stdout}"
    );
    assert!(
        !stdout.contains("Hashed "),
        "clean 不该读已打开文件的内容:\n{stdout}"
    );
    assert!(
        stdout.contains("No files to clean, everything up to date."),
        "{stdout}"
    );

    // 磁盘与打开状态一个都不能变——「不处理」不包括把它们删掉或写回。
    assert_eq!(sandbox.read("readme.txt"), "opened and changed\n");
    assert_eq!(sandbox.read("src/lib.txt"), "back again\n");
    let mut opened_after = sandbox.opened();
    opened_after.sort();
    assert_eq!(
        opened_after, opened_before,
        "clean must preserve open actions"
    );

    // 算过摘要的话缓存里会留下一份，下一轮一开跑就能加载。这一轮什么都没算，
    // 所以第二轮的启动输出里不该有那句话。
    let second = sandbox.cli().arg("-l").arg(".").output().expect("run tool");
    assert!(second.status.success(), "{second:?}");
    let second_stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        !second_stdout.contains("Loading cache from"),
        "clean 不该把已打开文件的摘要写进缓存:\n{second_stdout}"
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
