//! 八类变更的端到端验证：造出离线改动，看 dry run 报什么，再用 `-a` 落盘，
//! 最后回服务器独立取证。
//!
//! 每个用例一个独立的 p4d 实例，所以它们可以并行跑、互不干扰。
//!
//! 断言分两层：分类结果看 stdout（那是给用户看的契约），
//! **真实状态一律用沙箱自己的 `p4 opened` 取证**——只看 stdout 的话，
//! 「报告得对但下发的 p4 命令是错的」这种缺陷会被漏掉。
//!
//! 用例按「前置状态」而不是按「断言主题」分组：同一组里几个场景用互不相干的路径，
//! 一条用例内顺序跑完，省下的是每条一次 p4d 冷启动加一次模板复制——e2e 的开销几乎
//! 全在那里，不在断言上。前置互相冲突的（比如同一文件上先 revert 再 reopen）才拆开。

mod support;

use predicates::prelude::*;

use support::listed_changes;

/// 预演是只读的，三件事可以共用同一份前置：分类计数与逐组标题、磁盘与 `opened`
/// 一个都没动、以及与 `p4 reconcile -n` 的集合对照。
///
/// 三处差异落在三个互不相干的路径上，所以三件事之间不需要任何清理。
#[test]
fn a_dry_run_counts_every_group_touches_nothing_and_matches_p4_reconcile() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("readme.txt", "changed locally\n"); // Edit
    sandbox.write("fresh.txt", "brand new\n"); // Add
    sandbox.remove("src/lib.txt"); // Delete

    let output = sandbox.cli().arg("-l").arg(".").output().expect("run tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // 总数与逐组标题。
    assert!(stdout.contains("Counted 3 changes"), "{stdout}");
    assert!(stdout.contains("Editing 1 files"), "{stdout}");
    assert!(stdout.contains("Adding 1 files"), "{stdout}");
    assert!(stdout.contains("Deleting 1 files"), "{stdout}");
    assert!(
        stdout.contains("Inconsistencies found. Re-run with -a to apply changes."),
        "{stdout}"
    );

    // 预演不改服务器状态，也不改磁盘。
    assert!(
        sandbox.opened().is_empty(),
        "dry run must not open anything on the server"
    );
    assert_eq!(sandbox.read("readme.txt"), "changed locally\n");
    assert_eq!(sandbox.read("fresh.txt"), "brand new\n");
    assert!(!sandbox.exists("src/lib.txt"));

    // 与 p4 自己的判定对照：同一组离线改动，两边找出的变更集合必须一致。
    // 只用「没有打开过任何文件」的场景，这样八类标签与 p4 的四个动作名一一对应，
    // 归一化只剩路径形式上的差异。
    let ours = normalize(&listed_changes(&stdout));
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

/// 三类变更在同一个沙箱里一次落地：计数各自钉住自己那一类，
/// 落盘后逐行核对 `p4 opened` 的路径与动作对得上。
///
/// 三条断言原先各占一个沙箱，理由是 `opened.len() == 1` 能说明「只有这一个被动过」；
/// 合并后总数变成 3，那份隔离性改由「逐行找得到且动作正确」来保证——代价是三条一起红时
/// 定位稍慢，换来的是两次 p4d 冷启动。
#[test]
fn every_open_group_is_applied_in_one_pass() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("readme.txt", "changed locally\n");
    sandbox.write("fresh.txt", "brand new\n");
    sandbox.remove("src/lib.txt");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Editing 1 files"))
        .stdout(predicate::str::contains("Adding 1 files"))
        .stdout(predicate::str::contains("Deleting 1 files"))
        .stdout(predicate::str::contains("Inconsistencies fixed."));

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 3, "unexpected opened files: {opened:?}");
    let line_for = |name: &str| {
        opened
            .iter()
            .find(|line| line.contains(name))
            .unwrap_or_else(|| panic!("nothing opened for {name}: {opened:?}"))
    };
    assert!(line_for("readme.txt").contains(" - edit "), "{opened:?}");
    assert!(line_for("fresh.txt").contains(" - add "), "{opened:?}");
    assert!(line_for("lib.txt").contains(" - delete "), "{opened:?}");
}

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

/// 三种「过时的打开状态」在一个沙箱里依次 revert 掉。
///
/// 顺序是有意的，不是随手排的：每一步的收尾状态恰好是下一步的干净前置，
/// 所以中间不需要任何手工清理——revert 之后 `readme.txt` 回到 have、
/// 也没有任何东西还开着，下一步就能直接在它上面造新的过时状态。
/// 第三步用的是 `fresh.txt`，与前两步不共享文件。
#[test]
fn stale_open_states_are_reverted_in_one_pass() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 一、打开编辑但内容没改：过时的 checkout。
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

    // 二、标记删除但本地文件还在：过时的 delete。
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

    // 三、add 之后本地文件又没了：过时的 add。
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

/// 一个 p4 收不下的文件名不该让整轮停摆：其余各类照做，最后统一报错并非零退出。
///
/// `@` 在 p4 的参数语法里是版本说明符（`file@rev`、`@2024/01/01`），名字里带它的路径
/// p4 一律拒收——这是用户改不动、p4delta 也无解的一类文件。以前它只换到一行 stderr 警告，
/// 程序照样打印 "Inconsistencies fixed." 并退出 0；收紧成 strict 之后也必须只失败这一组，
/// 不能让另外几百个文件陪着一起不做。
///
/// 不并进 [`every_open_group_is_applied_in_one_pass`]：它要的是**失败**路径，
/// 而那条断言 `opened.len() == 3`，掺进来会让计数与隔离性都失真。
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
