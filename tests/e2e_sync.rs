//! `--sync`：把工作区拉到目标 depot 版本，只传真正需要传的文件。
//!
//! 与 `e2e_clean.rs` 一样，断言全部落在磁盘、`p4 opened`、`p4 have` 上——sync 会覆盖
//! 本地改动、会删文件，只看它自己打印了什么不足以说明它做对了。
//!
//! 每条用例都画着与 `--clean` 的分水岭：**depot 里没有的本地文件一律不碰**。

mod support;

use predicates::prelude::*;

/// 与 `--clean` 最要紧的分水岭：depot 里没有的本地文件是用户自己的东西，sync 一个都不碰。
/// 排在最前面，是因为这个模式敢让人用的理由就是它。
#[test]
fn an_untracked_file_survives_a_sync() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("extra.txt", "not in the depot\n");
    sandbox.write("build/scratch.txt", "also not in the depot\n");

    sandbox
        .cli()
        .args(["--sync", "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "No files to sync, everything up to date.",
        ));

    // 同一组文件交给 `--clean` 会被删掉，见 `e2e_clean.rs` 的第一条用例。
    assert!(sandbox.exists("extra.txt"));
    assert!(sandbox.exists("build/scratch.txt"));
    assert!(sandbox.opened().is_empty(), "sync never opens files");
}

/// dry run：三类差异都数得出来，而磁盘、opened、have 三处都不许动。
#[test]
fn sync_dry_run_changes_nothing() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("extra.txt", "not in the depot\n");
    sandbox.write("readme.txt", "locally changed\n");
    sandbox.remove("src/lib.txt");

    // 目标处已删除。提交删除会把 have 记录一并清掉，所以要手工把 have 拉回旧版，
    // 才造得出「have 停在旧版、head 是删除版本」这个状态。
    sandbox.commit("gone.txt", "will be deleted\n");
    sandbox.p4_ok(&["delete", "gone.txt"]);
    sandbox.p4_ok(&["submit", "-d", "delete it"]);
    sandbox.p4_ok(&["sync", "//depot/main/gone.txt#1"]);

    sandbox
        .cli()
        .args(["--sync", "-l"])
        .arg(".")
        .assert()
        .success()
        // 覆盖本地改动的代价必须在授权之前就看得见——clean 只在 -a 时打这句，sync 预演也打。
        .stdout(predicate::str::contains(
            "WARNING: this overwrites local changes to files that are not opened.",
        ))
        .stdout(predicate::str::contains("Reverting 1 files"))
        .stdout(predicate::str::contains("Restoring 1 files"))
        .stdout(predicate::str::contains("Deleting 1 files"))
        .stdout(predicate::str::contains("Updating").not())
        .stdout(predicate::str::contains("Counted 3 files to sync"))
        .stdout(predicate::str::contains(
            "Re-run with -a to sync the workspace.",
        ));

    assert!(sandbox.exists("extra.txt"), "未跟踪的文件不该被碰");
    assert_eq!(sandbox.read("readme.txt"), "locally changed\n");
    assert!(!sandbox.exists("src/lib.txt"));
    assert!(sandbox.exists("gone.txt"), "预演不该删文件");
    assert!(sandbox.opened().is_empty());
    // 这条才有信息量：`#none` 会清掉 have 记录，所以「还在」是真证据——预演若误跑了清记录
    // 那一步，它会变空。（拿一个本来就在 head 的文件断言「have 还在」是空的：工具无论
    // 做什么，它的 haveRev 都是 1。）
    assert_eq!(
        sandbox.p4_lines(&["have", "gone.txt"]).len(),
        1,
        "have 记录不该被动过"
    );
}

/// 已打开的文件不归 sync 管，`p4 sync -f` 的官方口径也是如此
/// （`This flag doesn't affect open files.`）。
///
/// 与 clean 的同名用例一样，刻意做出两个最能说明问题的情形：打开后改了内容、
/// 打开后删了本地文件——sync 都得原样留着。
#[test]
fn sync_never_touches_opened_files() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.p4_ok(&["edit", "readme.txt"]);
    sandbox.write("readme.txt", "opened and changed\n");
    sandbox.p4_ok(&["edit", "src/lib.txt"]);
    sandbox.remove("src/lib.txt");

    sandbox
        .cli()
        .args(["--sync", "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Updating").not())
        .stdout(predicate::str::contains("Reverting").not())
        .stdout(predicate::str::contains("Restoring").not())
        .stdout(predicate::str::contains("Deleting").not())
        .stdout(predicate::str::contains(
            "No files to sync, everything up to date.",
        ));

    assert_eq!(sandbox.read("readme.txt"), "opened and changed\n");
    assert!(
        !sandbox.exists("src/lib.txt"),
        "sync must not write back a file that is open for edit"
    );
    assert_eq!(
        sandbox.opened().len(),
        2,
        "sync must leave the open state alone: {:?}",
        sandbox.opened()
    );
}

/// head 目标下四种差异各自归组：落后（Update）、改了没打开（Revert）、
/// 本地缺失（Restore）、目标处已删除（Delete）。四组下发的命令是同一个形状，
/// 差别全在报告里怎么说——分组错了就是在误导用户。
#[test]
fn head_target_sorts_each_difference_into_its_own_group() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 落后：提交新版，再把 have 拉回旧版。
    sandbox.commit("moving.txt", "first revision\n");
    sandbox.p4_ok(&["edit", "moving.txt"]);
    sandbox.write("moving.txt", "second revision\n");
    sandbox.p4_ok(&["submit", "-d", "second revision"]);
    sandbox.p4_ok(&["sync", "//depot/main/moving.txt#1"]);

    // 目标处已删除：提交删除会连 have 记录一起清掉，所以要手工把 have 拉回旧版，
    // 才造得出「have 停在旧版、head 是删除版本」这个状态。
    sandbox.commit("gone.txt", "will be deleted\n");
    sandbox.p4_ok(&["delete", "gone.txt"]);
    sandbox.p4_ok(&["submit", "-d", "delete it"]);
    sandbox.p4_ok(&["sync", "//depot/main/gone.txt#1"]);

    // 改了、没打开。
    sandbox.write("readme.txt", "locally changed\n");
    // 本地缺失。
    sandbox.remove("src/deep/a/b/c.txt");

    let output = sandbox
        .cli()
        .args(["--sync", "-a", "-l"])
        .arg(".")
        .output()
        .expect("run the tool");
    assert!(output.status.success(), "{output:?}");
    let changes = normalize(&support::listed_changes(&String::from_utf8_lossy(
        &output.stdout,
    )));

    assert_eq!(
        changes,
        [
            ("Delete".to_owned(), "gone.txt".to_owned()),
            ("Restore".to_owned(), "c.txt".to_owned()),
            ("Revert".to_owned(), "readme.txt".to_owned()),
            ("Update".to_owned(), "moving.txt".to_owned()),
        ]
    );

    // 磁盘与 have 记录上的取证。
    assert_eq!(
        sandbox.read("moving.txt"),
        "second revision\n",
        "该拉到 head 的新版本"
    );
    assert_eq!(
        sandbox.read("readme.txt"),
        "hello from the depot\n",
        "未打开文件的本地改动该被丢弃"
    );
    assert_eq!(sandbox.read("src/deep/a/b/c.txt"), "deep\n");
    assert!(
        !sandbox.exists("gone.txt"),
        "目标处已删除，本地那份不该留着"
    );
    // 清 have 记录是 `#none` 的独家本事：只剩删文件的话，这里会留下一条指向已消失文件的
    // have 记录，之后普通 `p4 sync` 会认为「已是最新」而永不写回。
    assert!(
        sandbox.p4_lines(&["have", "gone.txt"]).is_empty(),
        "have 记录该被一并清掉"
    );
    assert!(sandbox.opened().is_empty(), "sync never opens files");
}

/// `--to <CL>`：拉到那个 changelist 时刻的状态，而不是 head。
#[test]
fn to_a_changelist_pulls_the_state_at_that_changelist() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 目标时刻：a.txt 已在库。
    sandbox.commit("a.txt", "first version\n");
    let target = latest_submitted(&sandbox);

    // 目标之后又来了两个提交：a.txt 改了新版，b.txt 被创建。
    sandbox.p4_ok(&["edit", "a.txt"]);
    sandbox.write("a.txt", "second version\n");
    sandbox.p4_ok(&["submit", "-d", "second version"]);
    sandbox.commit("b.txt", "created after the target\n");

    // 前提：两边都在 head 上，否则「拉回了旧版」证明不了什么。
    let before = sandbox.p4_ok(&["fstat", "-T", "headRev haveRev", "a.txt"]);
    assert!(
        before.contains("headRev 2") && before.contains("haveRev 2"),
        "unexpected depot state: {before}"
    );

    sandbox
        .cli()
        .args(["--sync", "--to", &target.to_string(), "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "Sync mode: updating the workspace to changelist {target}."
        )))
        .stdout(predicate::str::contains("Updating 1 files"))
        .stdout(predicate::str::contains("Deleting 1 files"));

    // 独立取证：不看工具自己的说法，看 p4 的 have 记录——head 没动，have 退回了目标 CL。
    assert_eq!(
        sandbox.read("a.txt"),
        "first version\n",
        "该拉到目标 CL 的那一版，而不是 head"
    );
    let after = sandbox.p4_ok(&["fstat", "-T", "headRev haveRev", "a.txt"]);
    assert!(
        after.contains("headRev 2"),
        "depot 的 head 不该被动过：{after}"
    );
    assert!(
        after.contains("haveRev 1"),
        "have 该退回目标 CL 的版本：{after}"
    );

    // 目标时刻还不存在的路径：文件删掉，have 记录一并清掉。
    assert!(!sandbox.exists("b.txt"));
    assert!(
        sandbox.p4_lines(&["have", "b.txt"]).is_empty(),
        "目标时刻不存在，就不该留着 have 记录"
    );
    assert!(sandbox.opened().is_empty());
}

/// `--to <CL>` 钉住的是**目标那一版**，不是「往回退一把」：have 比目标旧时，要往前拉到
/// 目标那一版，而不是顺手拉到 head。
///
/// head / have / 目标三者互不相同只有这一种形状（have 更旧、head 更新），而它正是
/// 「钉住的确实是目标那一版」最直接的证据——拉成 head 的话两个版本号都对不上。
#[test]
fn to_a_changelist_pulls_forward_to_that_revision_not_to_head() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 三个版本，一个比一个新：目标夹在中间。
    sandbox.commit("a.txt", "first version\n");
    sandbox.p4_ok(&["edit", "a.txt"]);
    sandbox.write("a.txt", "second version\n");
    sandbox.p4_ok(&["submit", "-d", "second version"]);
    let target = latest_submitted(&sandbox);

    sandbox.p4_ok(&["edit", "a.txt"]);
    sandbox.write("a.txt", "third version\n");
    sandbox.p4_ok(&["submit", "-d", "third version"]);

    // 把 have 与本地都退回第一版：下面要往前拉，但只该拉到目标那一版。
    sandbox.p4_ok(&["sync", "//depot/main/a.txt#1"]);
    let before = sandbox.p4_ok(&["fstat", "-T", "headRev haveRev", "a.txt"]);
    assert!(
        before.contains("headRev 3") && before.contains("haveRev 1"),
        "unexpected depot state: {before}"
    );

    sandbox
        .cli()
        .args(["--sync", "--to", &target.to_string(), "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Updating 1 files"))
        // 目标之后提交的版本不该被拉下来；目标时刻就在库的文件也不该进删除组。
        .stdout(predicate::str::contains("Deleting").not());

    // 独立取证：have 停在目标那一版，head 没动，磁盘上是第二版。
    let after = sandbox.p4_ok(&["fstat", "-T", "headRev haveRev", "a.txt"]);
    assert!(
        after.contains("headRev 3"),
        "depot 的 head 不该被动过：{after}"
    );
    assert!(
        after.contains("haveRev 2"),
        "have 该停在目标 CL 的那一版：{after}"
    );
    assert_eq!(sandbox.read("a.txt"), "second version\n");
    assert!(sandbox.opened().is_empty());
}

/// 空目标护栏：限定的目录里有跟踪文件，而选定的 CL 早于这个目录的出现——那个时刻目录里
/// 一件东西都没有，分析层的结论会是「把本地每一个被跟踪的文件都删掉」。
///
/// 这个结论不该被静默执行：工具在动手之前就拒绝（`src/reconcile/mod.rs` 里那条护栏），
/// 磁盘、have、opened 三处都不许动。
#[test]
fn a_target_changelist_that_predates_the_folder_is_refused() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 目标取种子的那份已提交 CL：真实有效（是 p4 分配出来的号），只是早于下面这个子目录。
    // 不写死编号——种子里多提交一次，写死的号就会指到别处去。
    let before_the_subdir = latest_submitted(&sandbox);

    // 限定目录：子目录里有跟踪文件，护栏的两个条件才凑得齐（depot 有记录、目标为空）。
    sandbox.commit("sub/only.txt", "in the subdirectory\n");

    // 前提，独立取证：head 侧有这个文件，而目标 CL 时刻还查不到它。
    let head = sandbox.p4_ok(&["fstat", "-T", "headRev", "sub/only.txt"]);
    assert!(head.contains("headRev 1"), "unexpected depot state: {head}");
    let target_spec = format!("sub/...@{before_the_subdir}");
    let at_target = sandbox.p4_lines(&["fstat", "-Rc", "-T", "depotFile", &target_spec]);
    assert!(
        at_target.is_empty(),
        "目标 CL 时刻子目录里还没有东西，护栏的前提才成立：{at_target:?}"
    );

    let output = sandbox
        .cli()
        .args(["--sync", "--to", &before_the_subdir.to_string(), "-a", "-l"])
        .arg("sub")
        .output()
        .expect("run the tool");

    assert!(!output.status.success(), "空目标必须被拒绝：{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!(
            "Changelist {before_the_subdir} has no files in this client's view"
        )),
        "{stderr}"
    );
    // 护栏的理由：照着这个目标做会删掉本地每一个被跟踪的文件——这里恰好一个。
    assert!(
        stderr.contains("would delete all 1 local files this client tracks"),
        "{stderr}"
    );

    // 拒绝发生在动作之前：磁盘、have、opened 三处都不许动。
    assert_eq!(sandbox.read("sub/only.txt"), "in the subdirectory\n");
    assert_eq!(
        sandbox.p4_lines(&["have", "sub/only.txt"]).len(),
        1,
        "have 记录不该被清掉"
    );
    assert!(sandbox.opened().is_empty());
}

/// `--to` 给一个超过 head 的 CL：p4 把「大于任何已提交 CL 的 N」当成不设限，目标是 head
/// 状态而不是空目标——护栏不该被它误触发。
///
/// 这是护栏的另一半：它挡的是「目标时刻真的什么都没有」，不是「目标号看着离谱」。真被
/// 误触发的话，一个手打大了的号会让工具拒绝干活，而用户只看到一句「Pick a later
/// changelist」。
#[test]
fn a_target_changelist_beyond_head_falls_back_to_head() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 落后：提交新版，再把 have 与本地都退回旧版。
    sandbox.commit("moving.txt", "first revision\n");
    sandbox.p4_ok(&["edit", "moving.txt"]);
    sandbox.write("moving.txt", "second revision\n");
    sandbox.p4_ok(&["submit", "-d", "second revision"]);
    sandbox.p4_ok(&["sync", "//depot/main/moving.txt#1"]);

    // 大于任何已分配的 CL：偏移给大一点，不 +1——pending changelist 也占号。
    let beyond_head = latest_submitted(&sandbox) + 1_000_000;

    let before = sandbox.p4_ok(&["fstat", "-T", "headRev haveRev", "moving.txt"]);
    assert!(
        before.contains("headRev 2") && before.contains("haveRev 1"),
        "unexpected depot state: {before}"
    );

    sandbox
        .cli()
        .args(["--sync", "--to", &beyond_head.to_string(), "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Updating 1 files"))
        // 空目标的话本地每一个文件都在删除之列——真有那一组就说明回落没发生。
        .stdout(predicate::str::contains("Deleting").not());

    // 独立取证：have 到了 head，磁盘上是 head 那一版，head 本身没动。
    let after = sandbox.p4_ok(&["fstat", "-T", "headRev haveRev", "moving.txt"]);
    assert!(
        after.contains("headRev 2"),
        "depot 的 head 不该被动过：{after}"
    );
    assert!(
        after.contains("haveRev 2"),
        "超过 head 的目标该落到 head：{after}"
    );
    assert_eq!(sandbox.read("moving.txt"), "second revision\n");
    assert!(sandbox.opened().is_empty());
}

/// 与原生 `p4 sync -f -n` 对照：那边「真的要传」的文件，必须一个不漏地出现在我们的清单里。
///
/// 不要求集合相等，因为两边的可观测面本来就不同：
///
/// - `-f` 会把没问题的文件也报成 `refreshing`（它压根不比对内容），那类不算「要传」；
/// - 反过来，本地改动这类漂移是 `-f` 的**盲区**（have 与 head 相同，它看不出差别），
///   只有我们能报出来。
///
/// 可对照的恰好是另外三类动词，它们与判定表里的三行一一对应：
/// `updating`（本地有一份、但不是目标版本）、`added as`（本地缺失）、`deleted as`（目标处
/// 不在库）。这个对应关系由 `label_of` 显式钉住——只比文件名的话，把 `updating` 那份归成
/// `Restore` 也照样通过。
///
/// 唯一的例外是 `deleted as` 里「本地既没有文件、也没有 have 记录」的那些：原生照样描述
/// 一句，而那里没有任何动作可做，见 `a_deleted_target_with_nothing_local_is_a_no_op`。
/// 本用例的场景里它们三者俱全，所以按全覆盖断言。
#[test]
fn our_listing_covers_everything_native_sync_would_transfer() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // updating：have 停在旧版。
    sandbox.commit("moving.txt", "first revision\n");
    sandbox.p4_ok(&["edit", "moving.txt"]);
    sandbox.write("moving.txt", "second revision\n");
    sandbox.p4_ok(&["submit", "-d", "second revision"]);
    sandbox.p4_ok(&["sync", "//depot/main/moving.txt#1"]);

    // added as：depot 有，本地与 have 都没有。
    sandbox.commit("fresh.txt", "brand new\n");
    sandbox.p4_ok(&["sync", "//depot/main/fresh.txt#none"]);

    // deleted as：head 是删除版本，本地却有。
    sandbox.commit("gone.txt", "will be deleted\n");
    sandbox.p4_ok(&["delete", "gone.txt"]);
    sandbox.p4_ok(&["submit", "-d", "delete it"]);
    sandbox.p4_ok(&["sync", "//depot/main/gone.txt#1"]);

    // 漂移：本地改了没打开的文件。原生那一侧看不见它。
    sandbox.write("src/lib.txt", "locally changed\n");

    let ours = sandbox
        .cli()
        .args(["--sync", "-l"])
        .arg(".")
        .output()
        .expect("run the tool");
    assert!(ours.status.success(), "{ours:?}");
    let ours = normalize(&support::listed_changes(&String::from_utf8_lossy(
        &ours.stdout,
    )));

    let theirs = native_transfers(&sandbox.p4_ok(&["sync", "-f", "-n", "..."]));

    // 先确认对照物本身是齐的，否则下面的「覆盖」会是个空断言。
    assert_eq!(
        theirs.len(),
        3,
        "原生输出里该有三类非 refreshing 的动作：{theirs:?}"
    );

    // 比 (动作, 文件名) 而不只是文件名：光比文件名的话，把 `updating` 那份归成 Restore
    // 也照样通过，而这三对映射正是本用例要钉的东西。
    for (verb, name) in &theirs {
        let expected = (label_of(verb).to_owned(), name.clone());
        assert!(
            ours.contains(&expected),
            "native `{verb} {name}` should land in our listing as {expected:?}: {ours:?}"
        );
    }

    // 反过来：漂移文件是 `-f` 的盲区，只有我们报得出来。
    assert!(
        ours.contains(&("Revert".to_owned(), "lib.txt".to_owned())),
        "the drifted file must be ours alone: {ours:?}"
    );
}

/// 目标处已删除，而本地**两者皆无**（文件与 have 记录都没了）时，我们无事可做——
/// 原生 `p4 sync -f -n` 却仍会为这条路径打印一行 `deleted as`。
///
/// 那行是**描述**而不是动作：它说的是「这条路径在目标处的状态是删除」。这个分歧先在真实
/// 工作区上撞见过一次（44.5k 文件那个目录：原生报 22 行 `deleted as`，我们报「无事可做」），
/// 逐条核实后确认那 22 个文件既不在磁盘上、也没有 have 记录，p4 自己也无事可做。
/// 这条用例把那次结论钉住，免得后来者把它当成漏报去「修」。
#[test]
fn a_deleted_target_with_nothing_local_is_a_no_op() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.commit("gone.txt", "will be deleted\n");
    sandbox.p4_ok(&["delete", "gone.txt"]);
    sandbox.p4_ok(&["submit", "-d", "delete it"]);
    // `p4 sync -f` 到删除版本：本地文件与 have 记录一起消失——正是实测里那 22 个的状态。
    sandbox.sync();
    assert!(!sandbox.exists("gone.txt"));
    assert!(sandbox.p4_lines(&["have", "gone.txt"]).is_empty());

    // 原生仍会描述这条路径的目标状态。
    let head_rev = head_rev_of(&sandbox, "gone.txt");
    let theirs = sandbox.p4_ok(&["sync", "-f", "-n", "..."]);
    assert!(
        theirs.contains(&format!("gone.txt#{head_rev} - deleted as")),
        "{theirs}"
    );

    // 我们无事可做，而这与「工作区已符合目标版本」并不矛盾：没有文件要删，也没有记录要清。
    sandbox
        .cli()
        .args(["--sync", "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleting").not())
        .stdout(predicate::str::contains(
            "No files to sync, everything up to date.",
        ));
}

/// `--verify-all` 的存在理由：默认档的「没变」是推断出来的，推断会错。
///
/// 这条用例刻意把内容改掉、又把 mtime 做成「刚同步过」的样子——正好落进
/// `is_unchanged_since_sync` 的 ±1 秒窗口。默认档据此跳过摘要，于是这次改动**静默漏报**；
/// `--verify-all` 把推断换成验证，同一份工作区就能抓住它。
///
/// 漏报是这个模式相对 `p4 sync -f` 唯一的新增失败模式，所以它必须有一个跑得起来的
/// 反例，而不是只写在 README 里。
#[test]
fn verify_all_catches_what_the_timestamp_shortcut_misses() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 改掉内容，但让它看起来还是刚同步下来的那一份——默认档的捷径就是靠这个跳过摘要的。
    write_keeping_the_synced_mtime(
        &sandbox,
        "readme.txt",
        "changed behind the timestamp shortcut\n",
    );

    sandbox
        .cli()
        .args(["--sync", "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        // 这一句同时是「捷径真的生效了」的证据，而且它该排在前面：万一 mtime 没落进
        // ±1 秒窗口（见 `write_keeping_the_synced_mtime`，那里把同步时的 mtime 原样恢复），
        // 失败会出现在这里，一眼看得出是时间问题；否则它会以「Reverting 意外出现」的形式
        // 失败，看起来像工具坏了。100% 是这个用例的隐含前提——种子里每个文件都是
        // `p4 sync -f` 刚落下来的，把它写出来，前提不成立时失败信息会直接指向它。
        .stdout(predicate::str::contains("digest computations (100%)"))
        .stdout(predicate::str::contains("Reverting").not())
        .stdout(predicate::str::contains(
            "No files to sync, everything up to date.",
        ));

    assert_eq!(
        sandbox.read("readme.txt"),
        "changed behind the timestamp shortcut\n",
        "默认档刚刚放过了这份改动——这就是那个失败模式本身"
    );

    sandbox
        .cli()
        .args(["--sync", "--verify-all", "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Reverting 1 files"))
        // 这句承诺只在验证过全部文件的那一档才敢说，见 `apply_sync`。
        .stdout(predicate::str::contains(
            "The synced files match the target depot revision.",
        ));

    assert_eq!(sandbox.read("readme.txt"), "hello from the depot\n");
}

/// `--verify-all` 只放大**摘要候选**，不放大动作组：have 已经落后的文件本就该直接传，
/// 不该被当成候选再比一次摘要。误分类会让同一个文件被下发两次、`total()` 也多算一次，
/// 而 `--verify-all` 是唯一会把候选集放大的档位——所以这一条得单独钉住。
#[test]
fn verify_all_does_not_treat_files_behind_the_target_as_digest_candidates() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 落后：提交新版，再把 have 拉回旧版。
    sandbox.commit("moving.txt", "first revision\n");
    sandbox.p4_ok(&["edit", "moving.txt"]);
    sandbox.write("moving.txt", "second revision\n");
    sandbox.p4_ok(&["submit", "-d", "second revision"]);
    sandbox.p4_ok(&["sync", "//depot/main/moving.txt#1"]);
    // 再叠一次本地改写：它仍然是「要拉到目标版本」的文件，不是摘要候选。
    sandbox.write("moving.txt", "local scribble\n");

    sandbox
        .cli()
        .args(["--sync", "--verify-all", "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Updating 1 files"))
        .stdout(predicate::str::contains("Reverting").not())
        // 总数是硬证据：真被当成候选的话，同一个文件会同时计进 Update 与 Revert。
        .stdout(predicate::str::contains("Synced 1 files"));

    assert_eq!(sandbox.read("moving.txt"), "second revision\n");
    assert!(sandbox.opened().is_empty());
}

/// 「本 client 从没同步过、本地却已经有同一份文件」：会被 depot 内容整份覆盖，而且没有
/// 旧版本可退回。它与「落后了拉一把」落在同一组里，光看组标题看不出来，所以工具单独说
/// 一句——这条用例把那句话和覆盖本身都钉住。
#[test]
fn a_local_file_the_client_never_synced_is_overwritten_with_a_warning() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.commit("fresh.txt", "from the depot\n");
    // 摘掉 have 记录（顺带删掉本地文件），再手写一份同名文件——这就是那个状态：
    // depot 有、本地有、have 没有。
    sandbox.p4_ok(&["sync", "//depot/main/fresh.txt#none"]);
    sandbox.write("fresh.txt", "my own copy\n");

    sandbox
        .cli()
        .args(["--sync", "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Found 1 file(s) this client has never synced; their local copies will be overwritten.",
        ))
        .stdout(predicate::str::contains("Updating 1 files"));

    // 硬证据走磁盘与 p4，不看工具自己的 stdout。
    assert_eq!(sandbox.read("fresh.txt"), "from the depot\n");
    assert_eq!(
        sandbox.p4_lines(&["have", "fresh.txt"]).len(),
        1,
        "拉下来之后该有 have 记录了"
    );
}

/// 目标处不在库、本地有一份、而本 client **从没同步过**它：这批不进 p4 调用，删文件那一步
/// 照做。
///
/// 为什么绕开 p4：`#none` 对没有 have 记录的路径是个 no-op，p4 只会往 stderr 写一句
/// `file(s) up-to-date.`（退出码 0）。把它混进那次调用，stderr 就被这句良性提示占满，
/// 判据只能退到看不见真失败的 `ExitCode`——而 `.success()` 本身就是这条用例的断言之一：
/// 那句提示没有被误报成失败。
///
/// 顺带钉住那行汇总提示：它和 Update 组的「将被覆盖」是同一个风险（东西是用户自己放进去
/// 的、depot 里没有旧版本可退回），就该有一样的可见度。
#[test]
fn a_never_synced_local_file_is_deleted_without_asking_p4() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.commit("gone.txt", "will be deleted\n");
    sandbox.p4_ok(&["delete", "gone.txt"]);
    sandbox.p4_ok(&["submit", "-d", "delete it"]);
    // 提交删除会把 have 记录一并清掉（正是这里要的状态），`p4 delete` 也把本地文件移走了；
    // 手写一份回去，就得到「目标处不在库 + 本地有 + have 没有」。
    sandbox.write("gone.txt", "my own copy\n");
    assert!(
        sandbox.p4_lines(&["have", "gone.txt"]).is_empty(),
        "前提：这个路径不该有 have 记录"
    );

    sandbox
        .cli()
        .args(["--sync", "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleting 1 files"))
        .stdout(predicate::str::contains(
            "Found 1 file(s) this client has never synced; their local copies will be deleted.",
        ));

    assert!(!sandbox.exists("gone.txt"), "本地那份该被删掉");
    assert!(
        sandbox.p4_lines(&["have", "gone.txt"]).is_empty(),
        "本来就没有 have 记录，跑完也不该冒出来一条"
    );
}

/// 一条腿失败不该让另一条腿不跑——这是删除组那两条腿最要紧的保证。
///
/// 顺序是先让 p4 清 have 记录、再由自己删文件。这里把文件摁住让删除被系统拒绝，
/// 断言错误信息里**两条腿的失败都报了出来**，那才是「都跑了」的证据；哪一条吞掉另一条，
/// 都会留下一个工具自己修不回来的状态（文件没了、记录还在，下一轮的删除组不再收它）。
#[cfg(windows)]
#[test]
fn both_delete_legs_report_their_own_failure() {
    use std::os::windows::fs::OpenOptionsExt;

    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 造出删除组的一条：have 停在旧版、head 是删除版本、本地有文件。
    sandbox.commit("gone.txt", "will be deleted\n");
    sandbox.p4_ok(&["delete", "gone.txt"]);
    sandbox.p4_ok(&["submit", "-d", "delete it"]);
    sandbox.p4_ok(&["sync", "//depot/main/gone.txt#1"]);
    assert_eq!(sandbox.p4_lines(&["have", "gone.txt"]).len(), 1);

    // 摁住它：分享模式里去掉 FILE_SHARE_DELETE，删除会被系统拒绝——p4 与工具都删不掉。
    // 绑定到具名变量，句柄就活到作用域结束。
    let path = sandbox.client_root().join("gone.txt");
    let _held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x0000_0001 | 0x0000_0002) // FILE_SHARE_READ | FILE_SHARE_WRITE
        .open(&path)
        .expect("hold the file open so it cannot be deleted");

    let output = sandbox
        .cli()
        .args(["--sync", "-a", "-l"])
        .arg(".")
        .output()
        .expect("run the tool");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("Failed to sync 1 change group(s)"),
        "这一轮该被报成失败：{stderr}"
    );
    assert!(
        stderr.contains("and clearing the have records also failed"),
        "两条腿的失败都该报出来，而不是前一条吞掉后一条：{stderr}"
    );
}

/// 把文件改成新内容，但把 mtime 留成 p4 同步时落下的那一份——正好落进
/// `is_unchanged_since_sync` 的 ±1 秒窗口，默认档据此跳过摘要计算。
///
/// mtime 不取 `SystemTime::now()`：have 的 syncTime 是**同步那一刻**记下的，而 now() 与它
/// 之间隔着这条用例跑到这里花掉的全部时间；机器一慢两者就差出一秒以上，窗口不成立，捷径
/// 失效，用例以「100%」那条断言红掉——那是随负载浮动的竞态。改为写之前先存下同步时的
/// mtime、写完再恢复：与 syncTime 的关系由 p4 当初怎么落盘决定，不需要 sleep 也不需要重试。
///
/// [`support::Sandbox::write`] 写完会把 mtime 回拨一小时（免得用例踩中那个窗口），
/// 这里要的正是窗口里面，所以在它之后再单独改回来。
fn write_keeping_the_synced_mtime(sandbox: &support::Sandbox, relative: &str, contents: &str) {
    let path = sandbox.client_root().join(relative);
    let synced = std::fs::metadata(&path)
        .expect("the file must exist to have a sync-time mtime")
        .modified()
        .expect("the file must carry an mtime");

    sandbox.write(relative, contents);

    // 写这一步已经把只读位松开（`noclobber` 让 sync 下来的文件不可写），所以这里能直接
    // 打开来改时间。
    let file = std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("open the file to restore its mtime");
    file.set_modified(synced)
        .expect("restore the sync-time mtime");
}

/// 最近一次已提交的 changelist 号。
fn latest_submitted(sandbox: &support::Sandbox) -> u32 {
    let line = sandbox
        .p4_lines(&["changes", "-m1", "-s", "submitted"])
        .into_iter()
        .next()
        .expect("the seed leaves at least one submitted changelist");

    line.split_whitespace()
        .skip_while(|token| *token != "Change")
        .nth(1)
        .and_then(|number| number.parse().ok())
        .unwrap_or_else(|| panic!("no changelist number in {line:?}"))
}

/// 某个文件在 depot 里的 head 修订号。
///
/// 断言原生那几行输出时用它拼版本号，别把 `#2` 写死：种子里多提交一次，就会让一条与
/// p4delta 无关的断言红掉。
fn head_rev_of(sandbox: &support::Sandbox, path: &str) -> u32 {
    let output = sandbox.p4_ok(&["fstat", "-T", "headRev", path]);
    output
        .split_whitespace()
        .skip_while(|token| *token != "headRev")
        .nth(1)
        .and_then(|rev| rev.parse().ok())
        .unwrap_or_else(|| panic!("no headRev in {output:?}"))
}

/// 原生 `p4 sync -f -n` 的动词 → 我们的分组标签。
///
/// 这三对是判定表三行的对照面：`updating` 是「have 与目标不一致」、`added as` 是「本地
/// 缺失」、`deleted as` 是「目标时刻不在库」。`refreshing` 不在表里——那是 `-f` 的噪音，
/// 见 `our_listing_covers_everything_native_sync_would_transfer` 的文档。
fn label_of(verb: &str) -> &'static str {
    match verb {
        "updating" => "Update",
        "added as" => "Restore",
        "deleted as" => "Delete",
        other => panic!("unexpected native verb {other:?}"),
    }
}

/// 原生 `p4 sync -f -n` 输出里「真的要传」的那几行，归一成 (动词, 文件名)。
///
/// 行形如 `//depot/main/moving.txt#2 - updating E:\ws\moving.txt`：`-f` 会把没问题的
/// 文件也报成 `refreshing`，所以只认另外三类动词。
fn native_transfers(output: &str) -> Vec<(String, String)> {
    const VERBS: [&str; 3] = ["updating", "added as", "deleted as"];

    output
        .lines()
        .filter_map(|line| {
            let (_, action) = line.split_once(" - ")?;
            let verb = VERBS.iter().find(|verb| action.starts_with(**verb))?;
            Some(((*verb).to_owned(), basename(action[verb.len()..].trim())))
        })
        .collect()
}

/// 清单归一成 (动作, 文件名) 并排序。
///
/// 两边报的路径一个来自我们、一个来自 p4 的 `-n` 输出，形式上的差异不是断言的对象；
/// 文件名足以把「哪些文件、什么动作」的差异逼出来。种子里文件名唯一是这套归一化的前提
/// （见 `e2e_open.rs` 的同名函数）。
fn normalize(changes: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = changes
        .iter()
        .map(|(label, path)| (label.clone(), basename(path)))
        .collect();
    out.sort();
    out
}

fn basename(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or(path).to_owned()
}
