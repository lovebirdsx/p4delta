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
        .args(["--sync", "--force", "-a", "-l"])
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
        .args(["--sync", "--force", "-l"])
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
        .args(["--sync", "--force", "-a", "-l"])
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
        .args(["--sync", "--force", "-a", "-l"])
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
        .args(["--sync", "--force", "--to", &target.to_string(), "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "Sync (force) mode: repairing the workspace to changelist {target}."
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
        .args(["--sync", "--force", "--to", &target.to_string(), "-a", "-l"])
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

/// head 已是删除版本、have 停在目标那一版、本地文件还在，而 `--to` 指向更早的 CL——
/// 在那个 CL 上这个路径还有内容。分析层这时没有别的依据，只能比摘要，所以要的 have 版
/// 字段（类型 / 大小 / 摘要）必须真的来自补查：旧代码在这里会把 head 版本的记录原样
/// 拿去比对，或者干脆报「Cannot handle ...」（现场报错的那一版）。
///
/// 顺带钉住补查的范围：`--to` 下只有「have 恰好停在目标版本」的记录需要内容基线；
/// 落后于目标的记录直接拉，一个字节的内容都不查（`BaselineScope::AtTarget`）。
#[test]
fn a_historical_target_keeps_the_deleted_head_record_untouched() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 落后者（head rev2、have rev1，目标是 rev2）与要比内容的（head rev2 = 删除、
    // have rev1，目标是 rev1）各一个。
    sandbox.commit("behind.txt", "behind one\n");
    sandbox.commit("compare.txt", "compare one\n");
    sandbox.p4_ok(&["edit", "behind.txt"]);
    sandbox.write("behind.txt", "behind two\n");
    sandbox.p4_ok(&["submit", "-d", "behind two"]);
    let target = latest_submitted(&sandbox);

    sandbox.p4_ok(&["delete", "compare.txt"]);
    sandbox.p4_ok(&["submit", "-d", "delete compare"]);
    // 提交删除会把 have 记录一并清掉，手工拉回第一版才造得出「head 是删除版本、
    // have 停在旧版、本地文件还在」这个状态（普通同步到旧 CL 之后的样子）。
    sandbox.p4_ok(&["sync", "-f", "//depot/main/behind.txt#1"]);
    sandbox.p4_ok(&["sync", "-f", "//depot/main/compare.txt#1"]);

    // 前提，独立取证。
    let before = sandbox.p4_ok(&["fstat", "-T", "headRev haveRev headAction", "compare.txt"]);
    assert!(
        before.contains("headRev 2") && before.contains("haveRev 1"),
        "unexpected depot state: {before}"
    );
    assert!(before.contains("headAction delete"), "{before}");

    sandbox
        .cli()
        .args([
            "--sync",
            "--force",
            "--verify-all",
            "--to",
            &target.to_string(),
            "-a",
            "-l",
        ])
        .arg(".")
        .assert()
        .success()
        // 摘要候选只有一个：落后于目标的那条不进补查。
        .stdout(predicate::str::contains(
            "Requesting depot state for 1 file(s) whose content needs comparing.",
        ))
        .stdout(predicate::str::contains("Updating 1 files"))
        // 目标时刻 compare.txt 还在库里，本地那一份也没变：不许进删除组。
        .stdout(predicate::str::contains("Deleting").not());

    // 独立取证：落后的那个拉到目标版本，比内容的那个原样不动。
    assert_eq!(sandbox.read("behind.txt"), "behind two\n");
    assert!(
        sandbox
            .p4_ok(&["fstat", "-T", "headRev", "behind.txt"])
            .contains("headRev 2"),
        "behind.txt 的 head 不该被动过"
    );
    assert!(sandbox.exists("compare.txt"), "本地文件不该被删掉");
    assert_eq!(sandbox.read("compare.txt"), "compare one\n");
    let after = sandbox.p4_ok(&["fstat", "-T", "haveRev headAction", "compare.txt"]);
    assert!(
        after.contains("haveRev 1") && after.contains("headAction delete"),
        "have 与 head 都不该被动过：{after}"
    );
    assert!(sandbox.opened().is_empty(), "sync never opens files");
}

/// 两条记录同时落后于 head、have 都停在目标版本——补查要**一次批量**取回它们的内容
/// 字段（≥2 条才走 `p4 fstat -L` 的批量表，单条退回普通查询，这条路径此前没有 e2e）。
///
/// 顺带把补查的用途钉死：`--verify-all` 下本地那份同尺寸的脏内容必须被摘要认出来（Revert），
/// 本地缺失的那份要从目标版本写回（Restore）。基线若回填错了（拿 head 版本的摘要比 have
/// 版本），同尺寸脏内容会算成「没变」而漏掉，缺失那份则会拿错版本写回。
#[test]
fn a_historical_target_refreshes_every_baseline_in_one_batch() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 目标时刻：a.txt / b.txt 都在库（各 rev1）。
    sandbox.commit("a.txt", "a one\n");
    sandbox.commit("b.txt", "b one\n");
    let target = latest_submitted(&sandbox);

    // 目标之后各改一版：head 到 rev2，have 仍停在 rev1。
    sandbox.p4_ok(&["edit", "a.txt"]);
    sandbox.write("a.txt", "a two\n");
    sandbox.p4_ok(&["submit", "-d", "a second"]);
    sandbox.p4_ok(&["edit", "b.txt"]);
    sandbox.write("b.txt", "b two\n");
    sandbox.p4_ok(&["submit", "-d", "b second"]);

    // 把 have 与本地都退回目标那一版（rev1）：两条记录的 have 都停在目标上。
    sandbox.p4_ok(&["sync", "-f", "//depot/main/a.txt#1"]);
    sandbox.p4_ok(&["sync", "-f", "//depot/main/b.txt#1"]);

    // a：同尺寸的脏内容（`a one` → `a two`，六字节不变）；b：本地缺失。
    sandbox.write("a.txt", "a two\n");
    sandbox.remove("b.txt");

    // 前提，独立取证：两条都是 head rev2 / have rev1。
    let before = sandbox.p4_ok(&["fstat", "-T", "headRev haveRev", "a.txt", "b.txt"]);
    assert!(
        before.contains("headRev 2") && before.contains("haveRev 1"),
        "unexpected depot state: {before}"
    );

    sandbox
        .cli()
        .args([
            "--sync",
            "--force",
            "--verify-all",
            "--to",
            &target.to_string(),
            "-a",
            "-l",
        ])
        .arg(".")
        .assert()
        .success()
        // 两条都要内容基线——这就是走 `-L` 批量表的那一档。
        .stdout(predicate::str::contains(
            "Requesting depot state for 2 file(s) whose content needs comparing.",
        ))
        .stdout(predicate::str::contains("Reverting 1 files"))
        .stdout(predicate::str::contains("Restoring 1 files"))
        .stdout(predicate::str::contains("Updating").not())
        .stdout(predicate::str::contains("Deleting").not());

    // 独立取证：两条都回到目标版本 rev1，head 没动，have 停在 rev1。
    assert_eq!(
        sandbox.read("a.txt"),
        "a one\n",
        "同尺寸脏内容该还原到目标版本"
    );
    assert_eq!(
        sandbox.read("b.txt"),
        "b one\n",
        "缺失的那份该从目标版本写回"
    );
    for name in ["a.txt", "b.txt"] {
        let after = sandbox.p4_ok(&["fstat", "-T", "headRev haveRev", name]);
        assert!(
            after.contains("headRev 2") && after.contains("haveRev 1"),
            "{name} 的 head / have 都不该被动过：{after}"
        );
    }
    assert!(sandbox.opened().is_empty(), "sync never opens files");
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
        .args([
            "--sync",
            "--force",
            "--to",
            &before_the_subdir.to_string(),
            "-a",
            "-l",
        ])
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
        .args([
            "--sync",
            "--force",
            "--to",
            &beyond_head.to_string(),
            "-a",
            "-l",
        ])
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
        .args(["--sync", "--force", "-l"])
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
        .args(["--sync", "--force", "-a", "-l"])
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
        .args(["--sync", "--force", "-a", "-l"])
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
        .args(["--sync", "--force", "--verify-all", "-a", "-l"])
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
        .args(["--sync", "--force", "--verify-all", "-a", "-l"])
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
        .args(["--sync", "--force", "-a", "-l"])
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
        .args(["--sync", "--force", "-a", "-l"])
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

/// force sync 的成本必须跟着「真要同步的东西」走，而不是跟着工作区大小走。
///
/// 抓的是那个把它拖垮的放大源：force 走 reconcile 管线，`gather_workspace` 会对**每一个**
/// 扫到的文件查一次 `p4 ignores -i`（外加一次 `p4 add -n` 兜底）。两笔账的实测数字见
/// `docs/why-faster.md`。
///
/// 而本地那些被忽略规则盖着的文件**本来就该被分析**：`.p4ignore` 不阻止 `p4 sync -f`
/// 更新已入库的文件，拿它当「不存在」是错的。这里刻意把嵌套 `.p4ignore`（`bulk/.p4ignore`
/// 一条 `*.txt`，盖住全部 400 个 bulk 文件）与 `--to <CL>` 一起摆上，就是现场的形状——
/// 有嵌套规则时目录剪枝会退回全扫，于是旧实现对着满工作区的「被忽略」文件逐批发问。
///
/// 判据走**外部观测**：`P4_EXE` 指向转发 shim（见 `tests/support/p4shim.rs`），数日志里
/// 含 `ignores` / `add -n` 的行。这一轮没有任何删除候选，所以两者都该是 0。
#[test]
fn a_force_sync_never_queries_ignores_for_the_whole_workspace() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };
    let Some(shim) = support::P4Shim::build() else {
        eprintln!("skipping: rustc is not available to build the p4 shim");
        return;
    };

    sandbox.bulk_text_files(400);
    let target = latest_submitted(&sandbox);
    sandbox.write("bulk/.p4ignore", "*.txt\n");
    // 前提：规则真的盖住这些文件。不钉住的话，将来规则不再匹配时这条用例会「静静地」变成
    // 一句空话——它要测的是「查不查」，不是「查了发现没被忽略」。
    let probe = sandbox.p4_ok(&["ignores", "-i", "bulk/d000/f000.txt"]);
    assert!(
        probe.contains(" ignored"),
        "前提不成立：规则没盖住 bulk 下的文件：{probe}"
    );

    let log = sandbox.instance_dir().join("p4-shim.log");
    let _ = std::fs::remove_file(&log);

    let output = sandbox
        .cli()
        .env("P4_EXE", &shim.path)
        .env("P4DELTA_SHIM_LOG", &log)
        .env("P4DELTA_SHIM_REAL_P4", sandbox.p4_exe())
        .args(["--sync", "--force", "--to", &target.to_string(), "-l"])
        .arg(".")
        .output()
        .expect("run tool");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success(), "run failed:\n{stdout}");
    // 这一轮确实在干活：不是「什么都没扫所以没查」那种空洞的绿。406 = 400 个 bulk 文件
    // + 那份嵌套 `.p4ignore` + 5 个种子文件——扫的是**整个**工作区，忽略规则不参与筛选。
    assert!(
        stdout.contains("Collected 406 files"),
        "该扫到整个工作区：{stdout}"
    );

    let calls: Vec<String> = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    assert!(
        calls
            .iter()
            .any(|line| line.split_whitespace().any(|token| token == "have")),
        "这一轮没起过 `p4 have`，shim 没接上：{calls:?}"
    );

    let token_count = |wanted: &str| {
        calls
            .iter()
            .filter(|line| line.split_whitespace().any(|token| token == wanted))
            .count()
    };
    assert_eq!(
        token_count("ignores"),
        0,
        "force sync 不该为全工作区（含目录剪枝）查 ignore：{calls:?}"
    );
    assert_eq!(
        calls.iter().filter(|line| line.contains("add -n")).count(),
        0,
        "没有删除候选，就不该起 `p4 add -n`：{calls:?}"
    );
    assert!(
        stdout.contains("No files to sync, everything up to date."),
        "把忽略规则盖着的文件当成「本地缺失」的话，这里会冒出 400 条 Restore：{stdout}"
    );
}

/// 目标处已删除、本地有一份、而这份正被忽略规则盖着：**不删**。
///
/// 这是「只对删除候选查 ignore」全部的意义所在——`p4 sync -f` 自己不看 `.p4ignore`，
/// 真让它按目标删，用户特意放在那儿的东西就没了。删除是唯一不可逆的一步，所以它保留
/// 原先那道保护，只是把查询范围从「整个工作区」缩到「删除候选」。
///
/// 三个候选一起上：两个被规则盖着（其中一个是**含空格**的名字——`p4 ignores` 的路径只能
/// 挂在命令行上，空格是它最容易被切错的地方），一个没被盖住。判据要能分开这三者：只留下
/// 前两个，第三个照删。非 ASCII 的候选另有用例（它在 Windows 上走的是另一条判据，见
/// `an_unreadable_delete_candidate_without_a_verdict_stops_the_run`）。
#[test]
fn an_ignored_delete_candidate_is_left_alone() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };
    // 真正下发那一轮要让 shim 记下 p4 调用（用来钉住查询范围），编不出 shim 就跳过。
    let Some(shim) = support::P4Shim::build() else {
        eprintln!("skipping: rustc is not available to build the p4 shim");
        return;
    };

    // 造出删除组的形状：have 停在旧版、head 是删除版本、本地有一份。
    sandbox.commit("protected.tmp", "keep me\n");
    sandbox.commit("sp ace.tmp", "keep me too\n");
    sandbox.commit("went.txt", "not covered by the rules\n");
    sandbox.p4_ok_paths(&["delete"], &["protected.tmp", "sp ace.tmp", "went.txt"]);
    sandbox.p4_ok(&["submit", "-d", "delete them"]);
    sandbox.p4_ok_paths(
        &["sync"],
        &[
            "//depot/main/protected.tmp#1",
            "//depot/main/sp ace.tmp#1",
            "//depot/main/went.txt#1",
        ],
    );

    sandbox.write(".p4ignore", "*.tmp\n");
    let probe = sandbox.p4_ok(&["ignores", "-i", "protected.tmp"]);
    assert!(
        probe.contains(" ignored"),
        "前提不成立：规则没盖住删除候选：{probe}"
    );

    sandbox
        .cli()
        .args(["--sync", "--force", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleting 1 files"))
        .stdout(predicate::str::contains("Kept 2 file(s)"))
        .stdout(predicate::str::contains(
            "Re-run with -a to sync the workspace.",
        ));

    assert!(sandbox.exists("protected.tmp"), "预演不该删文件");
    assert!(sandbox.exists("sp ace.tmp"), "预演不该删文件");
    assert!(sandbox.exists("went.txt"), "预演不该删文件");

    let log = sandbox.instance_dir().join("p4-shim.log");
    let _ = std::fs::remove_file(&log);

    sandbox
        .cli()
        .env("P4_EXE", &shim.path)
        .env("P4DELTA_SHIM_LOG", &log)
        .env("P4DELTA_SHIM_REAL_P4", sandbox.p4_exe())
        .args(["--sync", "--force", "-a"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleting 1 files"))
        .stdout(predicate::str::contains("Kept 2 file(s)"));

    // 保护的另一半是**范围**：问的就是删除候选那一小撮，工作区里没进删除组的文件一个都不问
    // ——否则这一步又成了全盘过滤（见 `docs/why-faster.md`）。
    let calls: Vec<String> = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    let ignores: Vec<&String> = calls
        .iter()
        .filter(|line| line.split_whitespace().any(|token| token == "ignores"))
        .collect();
    assert_eq!(ignores.len(), 1, "三个删除候选该合成一批：{calls:?}");
    assert!(
        ignores[0].contains("protected.tmp") && ignores[0].contains("sp ace.tmp"),
        "被忽略的两个候选该在查询里：{:?}",
        ignores[0]
    );
    assert!(
        !ignores[0].contains("readme.txt"),
        "没进删除组的文件不该被问：{:?}",
        ignores[0]
    );
    // 三个候选都是 ASCII，全程都交得到命令行上，兜底那条 `p4 add -n` 一次都不该起。
    assert!(
        !calls.iter().any(|line| line.contains("add -n")),
        "没有交不出去的路径，兜底查询不该出场：{calls:?}"
    );

    assert!(sandbox.exists("protected.tmp"), "被忽略的删除候选不该被删");
    assert!(sandbox.exists("sp ace.tmp"), "含空格的路径同样要被保护");
    assert_eq!(sandbox.read("protected.tmp"), "keep me\n", "内容也不该被动");
    // 保护必须做到「整条路径都不碰」：have 记录被清掉的话，下一轮它就不再是删除候选，
    // 用户那份文件会从此脱离工具的管理。
    assert_eq!(
        sandbox.p4_lines(&["have", "protected.tmp"]).len(),
        1,
        "保护要连 have 记录一起留下"
    );

    assert!(!sandbox.exists("went.txt"), "规则没盖住它，照旧删");
    assert!(
        sandbox.p4_lines(&["have", "went.txt"]).is_empty(),
        "没被保护的路径该连 have 记录一起清掉"
    );
    assert!(sandbox.opened().is_empty(), "sync never opens files");
}

/// 删除保护的 fail-closed 一半：`p4 ignores` **起来了、退出码还是 0**，却把错误写在 stderr
/// 上、stdout 一个字都没有。
///
/// 这就是最危险的形态——空 stdout 单看就是「一个都没被忽略」，照着做等于删用户的文件。判据
/// 因此不能只看退出码：退出码 0 但 stderr 有话说也算失败（`FailureMode::ExitCodeOrStderr`）。
/// 故障由转发 shim 注入（见 `tests/support/p4_shim_main.rs`），真实 p4 在正常输入下不会这么回。
#[test]
fn an_unusable_ignores_verdict_stops_the_run() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };
    let Some(shim) = support::P4Shim::build() else {
        eprintln!("skipping: rustc is not available to build the p4 shim");
        return;
    };

    // 删除组的形状：have 停在旧版、head 是删除版本、本地有一份。
    sandbox.commit("protected.tmp", "keep me\n");
    sandbox.p4_ok_paths(&["delete"], &["protected.tmp"]);
    sandbox.p4_ok(&["submit", "-d", "delete it"]);
    sandbox.p4_ok_paths(&["sync"], &["//depot/main/protected.tmp#1"]);
    sandbox.write(".p4ignore", "*.tmp\n");

    let output = sandbox
        .cli()
        .env("P4_EXE", &shim.path)
        .env("P4DELTA_SHIM_FAULT_COMMAND", "ignores")
        .env(
            "P4DELTA_SHIM_FAULT_STDERR",
            "Error: unable to read the ignore file.\n",
        )
        .env("P4DELTA_SHIM_REAL_P4", sandbox.p4_exe())
        .args(["--sync", "--force", "-a"])
        .arg(".")
        .output()
        .expect("run the tool");

    assert!(
        !output.status.success(),
        "拿不到可靠的忽略裁决就必须停下：{output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Refusing to continue")
            || stderr.contains("unable to read the ignore file"),
        "失败得说清是忽略裁决不可用：{stderr}"
    );
    assert!(
        sandbox.exists("protected.tmp"),
        "失败的一轮一个文件都不许动"
    );
    assert_eq!(sandbox.read("protected.tmp"), "keep me\n");
}

/// Windows 上非 ASCII 的删除候选挂不到 p4 命令行上（见 `command_line_safe`），只能走
/// `p4 add -n` 兜底。而兜底对一条 p4 已跟踪的路径回的是 `//depot/...#N - opened for add` 或
/// `can't add existing file`——**里面没有忽略信息**，「没被拒绝」推不出「没被忽略」。
///
/// 拿不到肯定裁决就不许删：这一轮直接停下，一个文件都不动。这是有意的取舍——比删错便宜。
/// Unix 上没有这条兜底（路径都交得到命令行上），用例只在 Windows 有意义。
#[cfg(windows)]
#[test]
fn an_unreadable_delete_candidate_without_a_verdict_stops_the_run() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 种子里的 `src/使用说明.txt` 已经是入库的第一版：把它删掉、提交，再把 have 拉回 #1，
    // 就得到「head 说删了、本地还有一份」的形状。删除命令走 stdin（见 `p4_ok_paths`）——
    // 非 ASCII 的名字挂不到命令行上，这正是生产代码这里要面对的那个限制。
    sandbox.p4_ok_paths(&["delete"], &["src/使用说明.txt"]);
    sandbox.p4_ok(&["submit", "-d", "delete the manual"]);
    sandbox.p4_ok_paths(&["sync"], &["//depot/main/src/使用说明.txt#1"]);

    let output = sandbox
        .cli()
        .args(["--sync", "--force", "-a"])
        .arg(".")
        .output()
        .expect("run the tool");

    assert!(
        !output.status.success(),
        "兜底给不出裁决时必须停下，而不是拿「没被拒绝」当「没被忽略」：{output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Refusing to continue") && stderr.contains("使用说明.txt"),
        "失败得点名那个拿不到裁决的候选：{stderr}"
    );
    assert!(
        sandbox.exists("src/使用说明.txt"),
        "失败的一轮一个文件都不许动"
    );
    assert_eq!(sandbox.read("src/使用说明.txt"), "中文内容\n");
}

/// 兜底那一路**起来了、退出码 0，但 stdout 是空的**——`FailureMode::SilentStrict` 看一眼退出
/// 码就放它过去，空结果于是被读成「一个都没被忽略」。
///
/// 与上一条的区别在故障形态：上一条是真实 p4 的「无裁决输出」，这一条是空输出本身。两条都
/// 必须停在原地，因为「p4 说了什么」和「p4 什么都没说」在删除这件事上都不能当成「没被忽略」。
#[cfg(windows)]
#[test]
fn an_empty_add_preview_stops_the_run() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };
    let Some(shim) = support::P4Shim::build() else {
        eprintln!("skipping: rustc is not available to build the p4 shim");
        return;
    };

    // 同上：种子里的第一版删掉、提交、再把 have 拉回 #1。
    sandbox.p4_ok_paths(&["delete"], &["src/使用说明.txt"]);
    sandbox.p4_ok(&["submit", "-d", "delete the manual"]);
    sandbox.p4_ok_paths(&["sync"], &["//depot/main/src/使用说明.txt#1"]);

    let output = sandbox
        .cli()
        .env("P4_EXE", &shim.path)
        .env("P4DELTA_SHIM_FAULT_COMMAND", "add")
        .env("P4DELTA_SHIM_REAL_P4", sandbox.p4_exe())
        .args(["--sync", "--force", "-a"])
        .arg(".")
        .output()
        .expect("run the tool");

    assert!(
        !output.status.success(),
        "空输出推不出「没被忽略」，这一轮必须停下：{output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Refusing to continue"),
        "失败得说清是哪一步拿不到裁决：{stderr}"
    );
    assert!(
        sandbox.exists("src/使用说明.txt"),
        "失败的一轮一个文件都不许动"
    );
}

/// 删除候选的另一条来路：目标时刻该路径还没进 depot（只有 `--to <CL>` 才可能）。保护得一样管。
///
/// 与上一条用例共用同一段代码（`analyze_at_target` 那句 `target.get(..)` 返回 `None`），但
/// 判据是两处不同的入口，而这条入口的现场形状更险：目标 CL 之后才创建的本地文件，工具眼里
/// 「该路径在目标时刻不存在」，照做就是删掉一份用户刚放进去、还没入库的东西。
#[test]
fn an_ignored_candidate_absent_from_the_target_is_left_alone() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 目标时刻只有 kept.tmp；gone.tmp 是目标之后才提交的。
    sandbox.commit("kept.tmp", "keep me\n");
    let target = latest_submitted(&sandbox);
    sandbox.commit("gone.tmp", "created after the target\n");

    sandbox.write(".p4ignore", "*.tmp\n");
    let probe = sandbox.p4_ok(&["ignores", "-i", "gone.tmp"]);
    assert!(
        probe.contains(" ignored"),
        "前提不成立：规则没盖住这个文件：{probe}"
    );

    sandbox
        .cli()
        .args(["--sync", "--force", "--to", &target.to_string(), "-a"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Kept 1 file(s)"))
        .stdout(predicate::str::contains("Deleting").not());

    assert!(sandbox.exists("gone.tmp"), "被忽略的候选不该被删");
    assert_eq!(sandbox.read("gone.tmp"), "created after the target\n");
    // have 记录跟着一起留下：清掉的话下一轮它就不再是候选，这份文件从此脱离工具的管理。
    assert_eq!(sandbox.p4_lines(&["have", "gone.tmp"]).len(), 1);
    assert!(sandbox.exists("kept.tmp"), "目标处活着的文件不该被动");
}

/// 被忽略规则盖着、但**已入库且在目标版本上**的本地文件，按它的真实状态参与分析。
///
/// 旧实现对这类文件一律当作「本地缺失」（`get_filtered` 返回 `None`）而归进 Restore，
/// 于是一次 `--sync` 会把用户那份文件重写掉——这是这个模式相对 `p4 sync -f` 唯一多出来的
/// 静默覆盖。`p4 sync -f` 自己不看 `.p4ignore`，它看到的只是一份改动；这里要与它对齐：
/// 同样的工作区该报 Revert。
///
/// 结果内容两边一致，所以判据只能落在**分组**上——那正是这次行为变化本身。
#[test]
fn a_live_ignored_file_is_analyzed_by_its_real_state_not_restored() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.commit("live.tmp", "first\n");
    // 提交后必须再同步一次：`submit` 把当时那份（被 `write` 回拨过的）mtime 记成 have 的
    // syncTime，紧接着再回拨一次正好落进 ±1 秒的捷径窗口。同步一下，两个值就都落在现在。
    sandbox.sync();
    sandbox.write(".p4ignore", "*.tmp\n");
    let probe = sandbox.p4_ok(&["ignores", "-i", "live.tmp"]);
    assert!(
        probe.contains(" ignored"),
        "前提不成立：规则没盖住这个文件：{probe}"
    );

    // 本地改动（`write` 顺手把 mtime 拨出一小时，默认档据此真的去算摘要）。
    sandbox.write("live.tmp", "locally changed\n");

    // 判据落在**分组**上：这个工作区在旧实现里报的是 Restoring（被忽略 → 当成缺失 → 无条件
    // 重写），现在按真实状态报 Revert。内容最后两边一致，所以只有分组能区分这两条语义。
    sandbox
        .cli()
        .args(["--sync", "--force", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Counted 1 files to sync"))
        .stdout(predicate::str::contains("Reverting 1 files"))
        .stdout(predicate::str::contains("Restoring").not());
    assert_eq!(
        sandbox.read("live.tmp"),
        "locally changed\n",
        "预演不该动文件"
    );

    sandbox
        .cli()
        .args(["--sync", "--force", "-a"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Reverting 1 files"))
        .stdout(predicate::str::contains("Restoring").not());

    assert_eq!(sandbox.read("live.tmp"), "first\n", "目标版本盖回来");
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
        .args(["--sync", "--force", "-a", "-l"])
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

/// sync 同样不打开文件，`-c` 对它也没有意义。但告警里要点出 `--to`——
/// 「指定目标版本」正是用户最容易顺手写成 `-c` 的东西。
///
/// 这条要真实连接才谈得上（范围配置挂在 client root 上），所以在沙箱里而不是 `tests/cli.rs`。
#[test]
fn a_changelist_is_ignored_with_a_warning() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox
        .cli()
        .args(["--sync", "-c", "5", "-l"])
        .arg(".")
        .assert()
        .success()
        .stderr(predicate::str::contains("--changelist 5 is ignored"))
        .stderr(predicate::str::contains("--to"))
        .stdout(predicate::str::contains("Sync mode"))
        .stdout(predicate::str::contains("Using pending changelist 5").not());
}

/// 旧命令行预算下的贪心批数，照抄 `compute_batches` 的算式（参数字符数 + 1 累计，
/// 上限 32767 - 2048）。用例拿它给新预算的批数封顶：stdin 载荷变大了，批数不该
/// 反而变多。
fn legacy_command_line_batches(specs: &[String]) -> usize {
    const BUDGET: usize = 32767 - 2048;

    let mut batches = 0;
    let mut size = 0usize;
    for spec in specs {
        if size > 0 && size + spec.len() > BUDGET {
            batches += 1;
            size = 0;
        }
        size += spec.len() + 1;
    }
    if size > 0 {
        batches += 1;
    }
    batches
}

/// 两组深路径在旧预算下都超过 8 批，逐次对账 shim 启动数与 summary，
/// 并逐文件核对内容及 have。A/B 每次重新建沙箱；基线另设 P4DELTA_E2E_LEGACY_BATCHES。
#[test]
fn a_bulk_deep_path_sync_keeps_counts_files_and_have_consistent() {
    use serde_json::Value;

    const TOTAL: usize = 9000;
    const RESTORED: usize = 3000;
    // 五层各 17 字节的目录 + 10 字节文件名：相对路径 ~百字节，全路径远离 MAX_PATH。
    const RELATIVE_DIR: &str =
        "bulk/d0123456789abcdef/d0123456789abcdef/d0123456789abcdef/d0123456789abcdef";

    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };
    let Some(shim) = support::P4Shim::build() else {
        eprintln!("skipping: rustc is not available to build the p4 shim");
        return;
    };

    // 一次 add、一次 submit 入库并同步到工作区（have 就位），内容带序号、每个文件不同。
    let contents: Vec<String> = (0..TOTAL)
        .map(|index| format!("base {index:04}\n"))
        .collect();
    for (index, content) in contents.iter().enumerate() {
        sandbox.write(&format!("{RELATIVE_DIR}/f{index:04}.txt"), content);
    }
    sandbox.p4_ok(&["add", "bulk/..."]);
    sandbox.p4_ok(&["submit", "-d", "bulk deep paths"]);
    sandbox.sync();

    // update 组（head 前进、工作区落后）：在 depot 里提交第二版，再把 have 拉回第一版。
    // 本地内容回到第一版；仍在磁盘上的文件构成 update 候选。
    // 直接改本地内容是不行的：内容与 have 不同只会进 revert（还原），那是另一组。
    let second: Vec<String> = (0..TOTAL)
        .map(|index| format!("changed {index:04}\n"))
        .collect();
    sandbox.p4_ok(&["edit", "bulk/..."]);
    for (index, content) in second.iter().enumerate() {
        sandbox.write(&format!("{RELATIVE_DIR}/f{index:04}.txt"), content);
    }
    sandbox.p4_ok(&["submit", "-d", "bulk deep paths v2"]);
    sandbox.p4_ok(&["sync", "//depot/main/bulk/...#1"]);

    // restore 组：前 RESTORED 个本地文件删掉（have 记录还在）。
    for index in 0..RESTORED {
        sandbox.remove(&format!("{RELATIVE_DIR}/f{index:04}.txt"));
    }

    // 下发规格（测试自算上限用）：force 无 --to 时目标就是 head（rev 2）。
    let depot_prefix = "//depot/main/";
    let update_specs: Vec<String> = (RESTORED..TOTAL)
        .map(|index| format!("{depot_prefix}{RELATIVE_DIR}/f{index:04}.txt#2"))
        .collect();
    let restore_specs: Vec<String> = (0..RESTORED)
        .map(|index| format!("{depot_prefix}{RELATIVE_DIR}/f{index:04}.txt#2"))
        .collect();
    let legacy_batches =
        legacy_command_line_batches(&update_specs) + legacy_command_line_batches(&restore_specs);
    let expected_batches = if std::env::var_os("P4DELTA_E2E_LEGACY_BATCHES").is_some() {
        legacy_batches
    } else {
        legacy_command_line_batches(&update_specs).min(8)
            + legacy_command_line_batches(&restore_specs).min(8)
    };
    assert!(
        legacy_batches > 16,
        "夹具必须超过两组各 8 批，才能检验调用数下降"
    );

    let log = sandbox.instance_dir().join("p4-shim.log");
    let _ = std::fs::remove_file(&log);

    let output = sandbox
        .cli()
        .env("P4_EXE", &shim.path)
        .env("P4DELTA_SHIM_LOG", &log)
        .env("P4DELTA_SHIM_REAL_P4", sandbox.p4_exe())
        .args(["--sync", "--force", "-a", "--json"])
        .arg(".")
        .output()
        .expect("run tool");
    assert!(
        output.status.success(),
        "run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout must be UTF-8");
    let records: Vec<Value> = stdout
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|error| panic!("stdout 里混进了非 JSON 行: {line:?} ({error})"))
        })
        .collect();
    let summary = records.last().expect("记录流里至少有一条 summary");
    assert_eq!(summary["kind"], "summary", "{summary}");

    let counts = summary["counts"]
        .as_object()
        .expect("summary 必须带 counts");
    assert_eq!(
        counts.get("update"),
        Some(&(TOTAL - RESTORED).into()),
        "{summary}"
    );
    assert_eq!(
        counts.get("restore"),
        Some(&(RESTORED as u64).into()),
        "{summary}"
    );
    assert_eq!(
        counts.get("revert"),
        None,
        "内容与 have 一致的文件不该进 revert：{summary}"
    );
    assert_eq!(counts.get("delete"), None, "{summary}");
    assert_eq!(
        summary["total"],
        (TOTAL - RESTORED + RESTORED) as u64,
        "全部候选都落在 update/restore 两组：{summary}"
    );

    // shim 的逐次启动记录：sync 行数与 summary 对拍，各批 -b 条数之和必须等于
    // update + restore 的规格总数。
    let calls: Vec<String> = std::fs::read_to_string(&log)
        .expect("shim log must exist")
        .lines()
        .map(str::to_owned)
        .collect();
    let sync_calls: Vec<&String> = calls
        .iter()
        .filter(|line| line.split_whitespace().any(|token| token == "sync"))
        .collect();
    let by_command = summary["p4CallsByCommand"].as_object().expect("对象");
    assert_eq!(
        sync_calls.len() as u64,
        by_command["sync"].as_u64().unwrap_or(0),
        "shim 启动数与自报计数对不上\ncalls: {sync_calls:?}\n{summary}"
    );
    assert_eq!(
        sync_calls.len(),
        expected_batches,
        "sync 实际启动数不符合分批策略：{sync_calls:?}"
    );

    let specs_per_batch: Vec<usize> = sync_calls
        .iter()
        .map(|line| {
            line.split_whitespace()
                .skip_while(|token| *token != "-b")
                .nth(1)
                .and_then(|count| count.parse().ok())
                .expect("每一批的 -b 后面都是本批条数")
        })
        .collect();
    assert_eq!(
        specs_per_batch.iter().sum::<usize>(),
        TOTAL,
        "各批条数之和必须等于 update + restore 的规格总数：{specs_per_batch:?}"
    );
    assert!(
        specs_per_batch.iter().all(|count| *count <= 8192),
        "单批条数超过 stdin 的条数上限：{specs_per_batch:?}"
    );

    // 终态：update 组前进到第二版，restore 组被写回的也是目标（head）版本；
    // have 全部停在 head（rev 2）。
    for (index, content) in second.iter().enumerate() {
        assert_eq!(
            sandbox.read(&format!("{RELATIVE_DIR}/f{index:04}.txt")),
            *content,
            "文件 {index} 的内容不对"
        );
    }
    let have = sandbox.p4_lines(&["have", "//depot/main/bulk/..."]);
    assert_eq!(
        have.len(),
        TOTAL,
        "have 记录数与入库文件数一致：{:?}",
        have.first()
    );
    for line in &have {
        assert!(line.contains("#2"), "have 应全部停在 head：{line}");
    }

    // A/B 汇总：两次跑（基线与工作区构建）各留一行可比对的数字。
    eprintln!(
        "p4delta A/B: sync={} fstat={} have={} total={} batches={specs_per_batch:?}",
        by_command["sync"],
        by_command["fstat"].as_u64().unwrap_or(0),
        by_command["have"].as_u64().unwrap_or(0),
        summary["p4Calls"],
    );
}
