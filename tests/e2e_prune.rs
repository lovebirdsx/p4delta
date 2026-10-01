//! 忽略目录剪枝：`P4IGNORE` 指向标准 `.p4ignore` 时，扫盘会跳过被忽略的目录，
//! 用目录级的 `p4 ignores` 查询代替逐文件判断。
//!
//! 沙箱环境里已经设好 `P4IGNORE=.p4ignore`——剪枝的门控要求生效值**恰好**是这个名字，
//! 差一个字都会静默退回完整扫描。

mod support;

use predicates::prelude::*;

/// 测试用的 `.p4ignore`。第一条让它忽略自己：p4 并不自动忽略这个文件，
/// 不写的话它会作为一个普通的新增文件混进清单，把断言搅浑。
/// 第二条才是这个文件要测的东西。
const P4IGNORE: &str = ".p4ignore\nbuild/\n";

/// 被忽略的目录整个跳过，里面的文件不会冒出来当新增文件。
#[test]
fn ignored_directories_are_pruned() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write(".p4ignore", P4IGNORE);
    // 噪音够多才值得剪：目录级判断的意义就是不去逐个 stat 这些文件。
    for index in 0..30 {
        sandbox.write(&format!("build/noise{index}.txt"), "ignored\n");
    }

    let output = sandbox
        .cli()
        .args(["-l", "-v"])
        .arg(".")
        .output()
        .expect("run tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // 剪枝确实生效（而不是悄悄退回了完整扫描），而且要**真的剪掉了东西**。
    // 只查 "Pruned " 这个关键字挡不住任何东西：那个计数为 0 时这行照样打印
    // （`Pruned 0 of 4 candidate directories`），把 prunable 恒置空、退回
    // 全量扫描再逐文件过滤，结论一模一样，断言却还是绿的。
    assert!(!stdout.contains("Not pruning"), "{stdout}");
    assert_eq!(
        pruned_dirs(&stdout),
        Some(1),
        "build/ 应当正是唯一被剪掉的目录:\n{stdout}"
    );
    // 一个变更都没有：`build/` 整个被跳过，`.p4ignore` 被自己的规则挡下。
    assert!(
        support::listed_changes(&stdout).is_empty(),
        "expected no changes at all:\n{stdout}"
    );
}

/// 关掉剪枝后结论必须一模一样——剪枝只该省查询，不该改语义。
#[test]
fn the_no_prune_flag_scans_everything() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write(".p4ignore", P4IGNORE);
    for index in 0..30 {
        sandbox.write(&format!("build/noise{index}.txt"), "ignored\n");
    }
    sandbox.write("fresh.txt", "brand new\n");

    let output = sandbox
        .cli()
        .args(["--no-prune-ignored-dirs", "-l", "-v"])
        .arg(".")
        .output()
        .expect("run tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("Not pruning ignored directories: --no-prune-ignored-dirs."),
        "{stdout}"
    );
    // 完整扫描下这些噪音仍然由文件级的 `p4 ignores -i` 挡掉。
    // 只在**清单行**里找：`-v` 的诊断行本来就会逐个提到那些被忽略的文件名，
    // 拿整段 stdout 做 contains 会把它们误当成变更。
    let changes = support::listed_changes(&stdout);
    assert_eq!(changes.len(), 1, "expected only fresh.txt: {changes:?}");
    assert_eq!(changes[0].0, "Add", "{changes:?}");
    assert!(changes[0].1.ends_with("fresh.txt"), "{changes:?}");
}

/// 被剪掉的目录里可能有 depot 已经跟踪的文件：本地删掉它之后仍要报 Delete，
/// 不能因为目录被跳过就当它不存在。
#[test]
fn tracked_files_in_pruned_directories_are_rescanned() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 先入库一个 build/ 下的文件——这时候还没有 .p4ignore，加得进去。
    sandbox.commit("build/tracked.txt", "tracked despite the ignore rule\n");
    // 现在把 build/ 忽略掉，并删掉本地那份。
    sandbox.write(".p4ignore", P4IGNORE);
    sandbox.remove("build/tracked.txt");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Re-scanning 1 pruned directories"))
        .stdout(predicate::str::contains("Deleting 1 files"))
        .stdout(predicate::str::contains("tracked.txt"));

    let opened = sandbox.opened();
    assert_eq!(
        opened.len(),
        1,
        "only the tracked file should be touched: {opened:?}"
    );
    assert!(opened[0].contains(" - delete "), "{opened:?}");
}

/// 从 `-v` 的输出里取「剪掉了几个目录」。
///
/// 该行形如 `Pruned 1 of 4 candidate directories in 2 batches (0.11 seconds)`。
/// 必须把数量解析出来——为 0 时这行照样打印，只看关键字等于没看。
fn pruned_dirs(stdout: &str) -> Option<usize> {
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Pruned "))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|count| count.parse().ok())
}
