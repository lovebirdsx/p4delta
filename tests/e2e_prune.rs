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

/// 剪枝生效与关掉剪枝，一个沙箱里前后脚跑：剪枝只该省查询，不该改语义。
///
/// **顺序是硬约束**，不是随手排的：第一步断言「一个变更都没有」，前提是工作区里除了
/// 被剪掉的 `build/` 和被自己规则挡下的 `.p4ignore` 之外没有别的东西——所以 `fresh.txt`
/// 必须等它跑完再写。先写 fresh.txt 的话第一步的清单就不为空了。
#[test]
fn pruning_and_no_prune_agree_on_the_result() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write(".p4ignore", P4IGNORE);
    // 噪音够多才值得剪：目录级判断的意义就是不去逐个 stat 这些文件。
    for index in 0..30 {
        sandbox.write(&format!("build/noise{index}.txt"), "ignored\n");
    }

    // 一、剪枝确实生效（而不是悄悄退回了完整扫描），而且要**真的剪掉了东西**。
    // 只查 "Pruned " 这个关键字挡不住任何东西：那个计数为 0 时这行照样打印
    // （`Pruned 0 of 4 candidate directories`），把 prunable 恒置空、退回
    // 全量扫描再逐文件过滤，结论一模一样，断言却还是绿的。
    let output = sandbox
        .cli()
        .args(["-l", "-v"])
        .arg(".")
        .output()
        .expect("run tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

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

    // 二、关掉剪枝，结论必须一模一样。
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

/// 工作区里有一个 p4 在命令行上读不出的名字时，同批 ASCII 文件的忽略判断不能被它带走。
///
/// Windows 上 p4 会把命令行解析两遍——宽字符一遍、ANSI 一遍——ANSI 那遍把代码页表示不了
/// 的字符换成 `?`，而 `?` 在文件名匹配里能匹**零个**字符：同一个参数在两遍里展开成不同
/// 个数，p4 报 `Argument parsing ambiguity.` 并以 -1 退出。宽松模式把它吞成一行警告，
/// 于是**一个都过滤不掉**：`.p4ignore` 与 build/ 下的噪音全成了新增文件。
///
/// emoji 在 CP1252 与 CP936 下都表示不出来，所以这条用例在 CI 的 en-US 机器与本机
/// 中文 Windows 上都能复现旧行为。放在 `src/` 下是让被吃成通配符的名字有东西可多匹：
/// 那里有 `lib.txt` 与 `使用说明.txt`。
#[test]
fn an_unreadable_name_does_not_take_the_whole_ignore_query_down() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write(".p4ignore", P4IGNORE);
    sandbox.write("build/noise0.txt", "ignored\n");
    // 四个 emoji：代码页里各退化成一个 `?`，合起来是 `????.txt`。
    sandbox.write("src/📁📁📁📁.txt", "a name p4 cannot read\n");

    let output = sandbox
        .cli()
        .args(["-l", "-v"])
        .arg(".")
        .output()
        .expect("run tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // 剪枝照常生效：目录名是全 ASCII 的，不受影响。
    assert!(!stdout.contains("Not pruning"), "{stdout}");

    // 唯一该报的就是那个读不出名字的文件。它交不到 p4 手上，改由 `p4 add -n` 补判
    // （见 `apply_file_ignores`），而它并没有被忽略，所以仍然该当新增。
    // `.p4ignore` 被自己的规则挡下、build/ 的噪音被剪掉，两者都靠主查询。
    let changes = support::listed_changes(&stdout);
    assert_eq!(
        changes.len(),
        1,
        "expected only the unreadable name: {changes:?}"
    );
    assert_eq!(changes[0].0, "Add", "{changes:?}");
    assert!(changes[0].1.contains("📁"), "{changes:?}");
}

/// 交不到命令行上的名字如果**确实被忽略**，必须问得出来：清单里不能报成新增，
/// clean 落地时更不能把它删掉。
///
/// 前一条（[`an_unreadable_name_does_not_take_the_whole_ignore_query_down`]）管的是
/// 「一个读不出的名字不该带垮同批 ASCII 文件的判断」；这一条是它的另一半：`p4 ignores`
/// 不认 stdin，那些名字改由同样认 stdin 的 `p4 add -n` 补判（见 `apply_file_ignores`）。
///
/// clean 那一半是**防数据丢失**，不只是防误报：`CleanChanges::project`
/// （`src/reconcile/clean.rs`）直接把 `changes.add` 当成待删清单，`delete_workspace_files`
/// 真的从磁盘删；修复前被忽略的非 ASCII 文件会落进 `add`，于是一次 `--clean -a`
/// 就把它们静默删光，还打印 "Workspace matches the depot."
///
/// `*.sketch` 是挑过的：ASCII 通配，不把非 ASCII 规则文本拖进 p4 的字符集处理；
/// 它匹配那个非 ASCII 名字，却不碰 seed 里的任何文件。文件放在 `src/` 下（目录名全
/// ASCII），免得被目录级剪枝先剪掉——那样根本走不到文件级过滤，用例就白测了。
/// `keep.txt` 是对照组：补充判据只该过滤被忽略的那些，不许扩大打击面；它同时让 clean
/// 那一半有东西可删，证明 clean 真的跑了而不是什么都没做。
#[test]
fn an_ignored_name_p4_cannot_read_is_filtered_and_clean_spares_it() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write(".p4ignore", ".p4ignore\n*.sketch\n");
    sandbox.write("src/草图📁.sketch", "ignored, and the name is unreadable\n");
    sandbox.write("src/keep.txt", "brand new\n");

    // 一、清单：`.p4ignore` 被自己的规则挡下（主查询），非 ASCII 名字由补充判据挡下，
    // 只剩对照组。这一步是预演，不改状态。
    let output = sandbox
        .cli()
        .args(["-l", "-v"])
        .arg(".")
        .output()
        .expect("run tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    let changes = support::listed_changes(&stdout);
    assert_eq!(
        changes.len(),
        1,
        "expected only keep.txt: {changes:?}\n{stdout}"
    );
    assert_eq!(changes[0].0, "Add", "{changes:?}");
    assert!(changes[0].1.ends_with("keep.txt"), "{changes:?}");

    // 二、落地：对照组的 keep.txt 该被删掉，被忽略的那份必须原样留着。
    assert!(sandbox.exists("src/草图📁.sketch"));
    sandbox
        .cli()
        .args(["--clean", "-a", "-l"])
        .arg(".")
        .assert()
        .success();

    assert!(
        sandbox.exists("src/草图📁.sketch"),
        "clean 删掉了被忽略的文件"
    );
    assert_eq!(
        sandbox.read("src/草图📁.sketch"),
        "ignored, and the name is unreadable\n"
    );
    assert!(
        !sandbox.exists("src/keep.txt"),
        "clean 该把对照组那份删掉——否则这一半证明不了它真的跑过"
    );
}

/// 被剪掉的目录里可能有 depot 已经跟踪的文件：本地删掉它之后仍要报 Delete，
/// 不能因为目录被跳过就当它不存在。
///
/// 不并进 [`pruning_and_no_prune_agree_on_the_result`]：它必须在 `.p4ignore` **存在之前**
/// 提交 `build/tracked.txt`，与那一条「先剪枝、后写 fresh.txt」的顺序互斥。
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
