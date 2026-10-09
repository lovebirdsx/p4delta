//! `--sync`（普通同步）：由原生 p4 判定覆盖保护、opened/resolve 与 have 状态。
//!
//! 与 `e2e_sync.rs`（`--sync --force`，强制修复）的分工：这一份盯的是「工具没有替 p4 做
//! 决定」——不改未打开的本地改动、不碰已打开的文件、不删 depot 里没有的东西；断言全部
//! 落在磁盘、`p4 opened`、`p4 have` 与 `p4 resolve -n` 上，不看工具自己打印了什么。
//!
//! 几处刻意做成**对拍**：另起一个状态相同的沙箱跑原生 `p4 sync`，比较两边的磁盘与 p4 状态。
//! 「与原生一致」是这条路唯一的正确性主张——它不承诺更快，也不承诺更聪明。

mod support;

use std::collections::BTreeMap;

use predicates::prelude::*;
use serde_json::Value;

/// have 已是目标版本、本地却改了或丢了：普通同步不修，强制修复才修。
///
/// 这是两种模式最要紧的分水岭，排在最前面：普通同步敢当「日常版本更新」用，靠的就是
/// 它不会顺手把用户没提交的活儿抹掉。
#[test]
fn a_local_change_to_an_up_to_date_file_survives_a_normal_sync() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("readme.txt", "locally changed\n");
    sandbox.remove("src/lib.txt");

    // 前提：两处都不是「落后于目标」，所以原生没有理由动它们。
    let before = sandbox.p4_ok(&["fstat", "-T", "headRev haveRev", "readme.txt"]);
    assert!(
        before.contains("headRev 1") && before.contains("haveRev 1"),
        "unexpected depot state: {before}"
    );

    sandbox
        .cli()
        .args(["--sync", "-a", "-l", "."])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "No files to sync, everything up to date.",
        ))
        // 「本地有改动所以跳过了它」不是告警该出场的事：普通同步什么都没做，
        // 而这两处本来就不在它的判据里。
        .stderr(predicate::str::contains("Warning").not());

    assert_eq!(sandbox.read("readme.txt"), "locally changed\n");
    assert!(
        !sandbox.exists("src/lib.txt"),
        "普通同步不写回本地缺失的文件"
    );
    assert!(sandbox.opened().is_empty());

    // 同一份现场交给强制修复：两处都会被修回 depot 的样子。
    sandbox
        .cli()
        .args(["--sync", "--force", "-a", "."])
        .assert()
        .success()
        .stdout(predicate::str::contains("Reverting 1 files"))
        .stdout(predicate::str::contains("Restoring 1 files"));

    assert_eq!(sandbox.read("readme.txt"), "hello from the depot\n");
    assert_eq!(sandbox.read("src/lib.txt"), "library\n");
}

/// 落后、缺失、目标处已删除：三类都按原生给的答案落地，并留下可独立取证的状态。
#[test]
fn a_normal_sync_transfers_what_native_p4_would_transfer() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 落后：提交新版，再把 have 拉回旧版。
    sandbox.commit("moving.txt", "first revision\n");
    sandbox.p4_ok(&["edit", "moving.txt"]);
    sandbox.write("moving.txt", "second revision\n");
    sandbox.p4_ok(&["submit", "-d", "second revision"]);
    sandbox.p4_ok(&["sync", "//depot/main/moving.txt#1"]);

    // 缺失：depot 有、本地与 have 都没有。
    sandbox.commit("fresh.txt", "brand new\n");
    sandbox.p4_ok(&["sync", "//depot/main/fresh.txt#none"]);

    // 目标处已删除：have 停在旧版、head 是删除版本。
    sandbox.commit("gone.txt", "will be deleted\n");
    sandbox.p4_ok(&["delete", "gone.txt"]);
    sandbox.p4_ok(&["submit", "-d", "delete it"]);
    sandbox.p4_ok(&["sync", "//depot/main/gone.txt#1"]);

    let output = sandbox
        .cli()
        .args(["--sync", "-a", "-l", "."])
        .output()
        .expect("run the tool");
    assert!(output.status.success(), "{output:?}");
    let changes = normalize(&support::listed_changes(&String::from_utf8_lossy(
        &output.stdout,
    )));

    assert_eq!(
        changes,
        [
            ("Add".to_owned(), "fresh.txt".to_owned()),
            ("Delete".to_owned(), "gone.txt".to_owned()),
            ("Update".to_owned(), "moving.txt".to_owned()),
        ]
    );

    // 独立取证：磁盘、have 记录、opened 三处。
    assert_eq!(sandbox.read("moving.txt"), "second revision\n");
    assert_eq!(sandbox.read("fresh.txt"), "brand new\n");
    assert!(!sandbox.exists("gone.txt"));
    assert!(
        sandbox.p4_lines(&["have", "gone.txt"]).is_empty(),
        "删除必须连同 have 记录一起清掉，否则普通 p4 sync 之后再也写不回来"
    );
    assert_eq!(
        sandbox.p4_lines(&["have", "fresh.txt"]).len(),
        1,
        "写进来之后该有 have 记录了"
    );
    assert!(sandbox.opened().is_empty(), "普通同步不开文件");
}

/// 已打开的文件由 p4 判定，工具不替它做决定。
///
/// 两种最能说明问题的情形：打开后改了内容、打开后删了本地文件。普通同步都要原样留着
/// （原生 sync 对已打开文件的官方口径是「不碰」）。
#[test]
fn an_opened_file_is_left_to_p4() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.p4_ok(&["edit", "readme.txt"]);
    sandbox.write("readme.txt", "opened and changed\n");
    sandbox.p4_ok(&["delete", "src/lib.txt"]);

    sandbox
        .cli()
        .args(["--sync", "-a", "-l", "."])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "No files to sync, everything up to date.",
        ));

    assert_eq!(sandbox.read("readme.txt"), "opened and changed\n");
    assert!(!sandbox.exists("src/lib.txt"));
    assert_eq!(
        sandbox.opened().len(),
        2,
        "打开状态必须原样留着: {:?}",
        sandbox.opened()
    );
}

/// 打开编辑、have 又落后于 head：原生会推进 have 并把文件挂上待 resolve，本工具照做。
///
/// 这一条钉的是「不做自动 resolve」：待解决的合并必须留给用户。
#[test]
fn an_opened_file_behind_the_target_reaches_the_same_resolve_state_as_native() {
    let (Some(ours), Some(theirs)) = (
        sandbox_with_an_opened_file_behind_head(),
        sandbox_with_an_opened_file_behind_head(),
    ) else {
        return;
    };

    ours.cli().args(["--sync", "-a", "."]).assert().success();

    // 对照面：原生普通 sync，同样是「把工作区拉到 head」。
    theirs.p4_ok(&["sync"]);

    assert_eq!(ours.read("readme.txt"), theirs.read("readme.txt"));
    assert_eq!(
        ours.opened(),
        theirs.opened(),
        "opened 状态该与原生逐字一致"
    );
    assert_eq!(
        resolve_state(&ours),
        resolve_state(&theirs),
        "待 resolve 的清单该与原生一致"
    );
    // 待解决没被自动做掉：`p4 resolve -n` 还报得出一条待合并。
    assert!(
        !resolve_state(&ours).is_empty(),
        "自动 resolve 会把这一条抹掉，而普通同步不替用户决定合并结果"
    );
}

/// 已打开、have 落后，而 `--to <CL>` 指的是中间某一版：钉住的是**目标处**的版本。
///
/// 这一条区分「钉目标版本」与「钉 head」：目标处是第 2 版、head 是第 3 版，照 head 钉会
/// 把文件推过头，而原生 `...@CL` 只推到目标那一版。
#[test]
fn an_opened_file_follows_the_changelist_target_not_head() {
    let (Some(ours), Some(theirs)) = (support::sandbox_or_skip(), support::sandbox_or_skip())
    else {
        return;
    };

    let mut target = 0;
    for sandbox in [&ours, &theirs] {
        sandbox.p4_ok(&["edit", "readme.txt"]);
        sandbox.write("readme.txt", "second\n");
        sandbox.p4_ok(&["submit", "-d", "second"]);
        target = latest_submitted(sandbox);
        sandbox.p4_ok(&["edit", "readme.txt"]);
        sandbox.write("readme.txt", "third\n");
        sandbox.p4_ok(&["submit", "-d", "third"]);

        sandbox.p4_ok(&["sync", "//depot/main/readme.txt#1"]);
        sandbox.p4_ok(&["edit", "readme.txt"]);
        sandbox.write("readme.txt", "local\n");
    }

    ours.cli()
        .args(["--sync", "--to", &target.to_string(), "-a", "."])
        .assert()
        .success();
    theirs.p4_ok(&["sync", &format!("//depot/main/...@{target}")]);

    assert_eq!(ours.read("readme.txt"), theirs.read("readme.txt"));
    assert_eq!(ours.opened(), theirs.opened());
    assert_eq!(resolve_state(&ours), resolve_state(&theirs));

    // 独立取证：have 停在目标那一版，而不是 head。
    let have = ours.p4_ok(&["fstat", "-T", "headRev haveRev", "//depot/main/readme.txt"]);
    assert!(have.contains("headRev 3"), "{have}");
    assert!(have.contains("haveRev 2"), "have 该停在目标版本：{have}");
}

/// 预演里已打开的文件照样要报出来，但一个字都不写。
#[test]
fn an_opened_file_is_only_reported_in_a_dry_run() {
    let Some(sandbox) = sandbox_with_an_opened_file_behind_head() else {
        return;
    };

    let have_before = sandbox.p4_ok(&["fstat", "-T", "haveRev", "//depot/main/readme.txt"]);
    let opened_before = sandbox.opened();

    let output = sandbox
        .cli()
        .args(["--sync", "-l", "."])
        .output()
        .expect("run the tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Handing 1 opened files"), "{stdout}");
    assert!(stdout.contains("Opened"), "{stdout}");

    assert_eq!(sandbox.read("readme.txt"), "second local\n");
    assert_eq!(
        sandbox.p4_ok(&["fstat", "-T", "haveRev", "//depot/main/readme.txt"]),
        have_before,
        "预演不该动 have"
    );
    assert_eq!(sandbox.opened(), opened_before, "预演不该动 opened");
}

/// 范围内的已打开文件不止一个时，只有 have 不在目标版本上的那些算待办。
///
/// `p4 opened` 给的是范围内**全部**已打开文件，比原生那句话提到的多：have 已经落在目标
/// 版本上的文件，原生连一句都不说（实测，下面先取证），工具也不该把它报成候选——否则
/// 报告里的数字是虚的，还会为它下发一条 p4 只会回 `file(s) up-to-date.` 的空命令。
#[test]
fn an_opened_file_at_the_target_revision_is_not_a_candidate() {
    let Some(sandbox) = sandbox_with_an_opened_file_behind_head() else {
        return;
    };

    // 再加一个同范围的已打开文件：have == head、本地改了。
    sandbox.p4_ok(&["edit", "src/lib.txt"]);
    sandbox.write("src/lib.txt", "local edit\n");

    // 前提取证：原生整棵范围的预演只提落后的那一个。
    let native = sandbox.p4_ok(&["sync", "-n", "//depot/main/..."]);
    assert!(native.contains("readme.txt"), "{native}");
    assert!(!native.contains("lib.txt"), "原生不该提它：{native}");

    let records = run_json(&sandbox, &["--json", "--sync", "-l", "."]);
    let files = files_in(&records);
    assert_eq!(files.len(), 1, "只有 have 落后的那个该出现：{files:?}");
    assert!(files.contains_key("//depot/main/readme.txt"), "{files:?}");
    assert_eq!(summary_in(&records)["counts"]["resolve"], 1);
}

/// 已打开的文件落在排除项里：补查会看见它，但一条规格都不许下发。
///
/// 补查走的是只读查询（`p4 opened` / `p4 fstat`），范围过滤在它之后——排除项是硬边界。
/// 这里的排除来自**配置**：一次性的 `--exclude-*` 与普通同步冲突（见 `tests/cli.rs`），
/// 而持久范围对普通同步同样生效。
#[test]
fn an_opened_file_outside_the_scope_is_never_written_to() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write(".p4delta-scope", r#"{"exclude": [{"dir": "generated"}]}"#);
    sandbox.commit("generated/tracked.txt", "first\n");
    sandbox.p4_ok(&["edit", "generated/tracked.txt"]);
    sandbox.write("generated/tracked.txt", "second\n");
    sandbox.p4_ok(&["submit", "-d", "second"]);
    sandbox.p4_ok(&["sync", "//depot/main/generated/tracked.txt#1"]);
    sandbox.p4_ok(&["edit", "generated/tracked.txt"]);
    sandbox.write("generated/tracked.txt", "local\n");

    let have_before = sandbox.p4_ok(&[
        "fstat",
        "-T",
        "haveRev",
        "//depot/main/generated/tracked.txt",
    ]);

    sandbox
        .cli()
        .args(["--sync", "-a"])
        .arg(".")
        .assert()
        .success();

    assert_eq!(sandbox.read("generated/tracked.txt"), "local\n");
    assert_eq!(
        sandbox.p4_ok(&[
            "fstat",
            "-T",
            "haveRev",
            "//depot/main/generated/tracked.txt"
        ]),
        have_before,
        "排除项里的文件连 have 都不该被推"
    );
}

/// JSON 契约：已打开的文件单独一类，`nativeAction` 缺席——原生对这类事件没有动作词。
#[test]
fn the_json_stream_reports_an_opened_file_as_resolve() {
    let Some(sandbox) = sandbox_with_an_opened_file_behind_head() else {
        return;
    };

    let records = run_json(&sandbox, &["--json", "--sync", "-a", "."]);
    let files = files_in(&records);
    assert_eq!(files.len(), 1, "{files:?}");

    let record = &files["//depot/main/readme.txt"];
    assert_eq!(record["class"], "resolve");
    assert_eq!(record["action"], "scheduling");
    assert_eq!(record["stage"], "apply");
    assert_eq!(record["force"], false);
    assert_eq!(record["applied"], true);
    assert_eq!(record["rev"], "2");
    assert!(
        record.get("nativeAction").is_none(),
        "原生没给动作词，记录里就该缺席：{record}"
    );

    let summary = summary_in(&records);
    assert_eq!(summary["ok"], true);
    assert_eq!(summary["mode"], "sync");
    assert_eq!(summary["force"], false);
    assert_eq!(summary["total"], 1);
    assert_eq!(summary["counts"]["resolve"], 1);
    assert_eq!(records_summary_count(&records), 1, "summary 恰好一条");
}

/// clobber 保护矩阵的对拍：本地可写、未打开、又与目标不同的文件，原生会拒绝覆盖
/// （`Can't clobber writable file`）。普通同步必须给出同一个结果——同样的失败，同样
/// 不让整轮伪装成成功。
#[test]
fn a_writable_unopened_file_fails_the_same_way_native_does() {
    let (Some(ours), Some(theirs)) = (support::sandbox_or_skip(), support::sandbox_or_skip())
    else {
        return;
    };

    for sandbox in [&ours, &theirs] {
        // 目标处有新版，本地却是一份可写的、没有 have 记录的同名文件。
        sandbox.commit("blocked.txt", "from the depot\n");
        sandbox.p4_ok(&["sync", "//depot/main/blocked.txt#none"]);
        // `write` 会清掉只读位并回拨 mtime——正是 noclobber 要拦的那种文件。
        sandbox.write("blocked.txt", "my own writable copy\n");
    }

    let ours_run = ours
        .cli()
        .args(["--sync", "-a", "."])
        .output()
        .expect("run the tool");
    let theirs_run = theirs.p4(&["sync"]);

    assert_eq!(
        ours_run.status.success(),
        theirs_run.status.success(),
        "整轮成败该与原生一致\nours: {ours_run:?}\ntheirs: {theirs_run:?}"
    );
    assert_eq!(ours.read("blocked.txt"), theirs.read("blocked.txt"));
    assert_eq!(
        ours.p4_lines(&["have", "blocked.txt"]),
        theirs.p4_lines(&["have", "blocked.txt"])
    );

    // 原生拒绝了它，我们也不能报成成功。
    assert!(
        !theirs_run.status.success(),
        "前提：这一份现场原生会拒绝覆盖：{theirs_run:?}"
    );
    assert!(
        String::from_utf8_lossy(&ours_run.stderr).contains("Failed to sync"),
        "逐文件失败必须让整轮失败：{ours_run:?}"
    );
}

/// `allwrite noclobber` 下原生对「本地改了、又没打开」的文件**逐文件拒绝**：`info` 提示、
/// 退出码 0、正文一条记录都没有。p4delta 曾把这些 info 全当成「需要已打开文件解释」的提示，
/// 补查解释不了就整轮失败——这条回归钉的就是它。
///
/// update 与 delete 两条腿都要在：`can't delete modified file` 指向的文件在目标处是删除
/// 状态，与 update 共用同一段文本结构，漏掉它就等于漏掉一半现场。
#[test]
fn a_refused_local_change_is_reported_as_a_refusal_not_an_incomplete_answer() {
    let Some(sandbox) = sandbox_with_allwrite_noclobber() else {
        return;
    };

    behind_then_modified(&sandbox, "readme.txt", "local update\n");
    // 删除腿：目标处已删除，本地未打开地改了。
    sandbox.commit("going.txt", "will go\n");
    sandbox.p4_ok(&["delete", "going.txt"]);
    sandbox.p4_ok(&["submit", "-d", "delete it"]);
    sandbox.p4_ok(&["sync", "//depot/main/going.txt#1"]);
    sandbox.write("going.txt", "local delete\n");

    let have_before = sandbox.p4_ok(&["fstat", "-T", "haveRev", "//depot/main/readme.txt"]);
    let opened_before = sandbox.opened();

    // 人类文案不能说「已是最新」：拒绝意味着工作区与目标不同，只是这一轮不去覆盖它。
    sandbox
        .cli()
        .args(["--sync", "-a", "."])
        .assert()
        .success()
        .stdout(predicate::str::contains("everything up to date").not())
        .stdout(predicate::str::contains("refused"))
        .stderr(predicate::str::contains("can't update modified file"))
        // delete 那条的修订号（have 停在第 1 版）是消费方定位文件用的，必须原样在。
        .stderr(predicate::str::contains(
            "//depot/main/going.txt#1 - can't delete modified file",
        ));

    assert_eq!(sandbox.read("readme.txt"), "local update\n");
    assert_eq!(sandbox.read("going.txt"), "local delete\n");
    assert_eq!(
        sandbox.p4_ok(&["fstat", "-T", "haveRev", "//depot/main/readme.txt"]),
        have_before,
        "拒绝的文件连 have 都不该被推"
    );
    assert_eq!(sandbox.opened(), opened_before);

    // 机器可读一侧：拒绝不是逐文件动作，一条记录都不发；这一轮仍有结论（ok），
    // 消费方靠 stderr 上的拒绝原文判断「不是最新」。
    let records = run_json(&sandbox, &["--json", "--sync", "-a", "."]);
    assert!(files_in(&records).is_empty(), "拒绝的文件不该进记录流");
    let summary = summary_in(&records);
    assert_eq!(summary["ok"], true, "{summary}");
    assert_eq!(summary["total"], 0, "拒绝不产生逐文件动作：{summary}");
}

/// 同一轮里既有被拒的文件、也有原生真会传的文件：安全的那批照原生同步，被拒的原样留着。
///
/// 与原生 `p4 sync` 对拍（磁盘、have、opened 三处），因为「与原生一致」是这条路唯一的
/// 正确性主张——不该因为有一批被拒就把整轮停下，也不该顺手把被拒的文件也写了。
#[test]
fn a_refusal_does_not_stop_the_files_native_would_transfer() {
    let (Some(ours), Some(theirs)) = (
        sandbox_with_allwrite_noclobber(),
        sandbox_with_allwrite_noclobber(),
    ) else {
        return;
    };

    for sandbox in [&ours, &theirs] {
        // 被拒：落后一版、本地改了（未打开）。先做：`behind_then_modified` 里那次 submit
        // 会带上默认 changelist 里所有打开的文件。
        behind_then_modified(sandbox, "readme.txt", "local\n");
        // 安全：落后一版、本地未改。
        sandbox.commit("safe.txt", "v1\n");
        sandbox.p4_ok(&["edit", "safe.txt"]);
        sandbox.write("safe.txt", "v2\n");
        sandbox.p4_ok(&["submit", "-d", "v2"]);
        sandbox.p4_ok(&["sync", "//depot/main/safe.txt#1"]);
    }

    ours.cli().args(["--sync", "-a", "."]).assert().success();
    theirs.p4_ok(&["sync"]);

    assert_eq!(ours.read("safe.txt"), theirs.read("safe.txt"));
    assert_eq!(ours.read("readme.txt"), theirs.read("readme.txt"));
    assert_eq!(
        ours.p4_ok(&["fstat", "-T", "haveRev", "safe.txt"]),
        theirs.p4_ok(&["fstat", "-T", "haveRev", "safe.txt"])
    );
    assert_eq!(ours.opened(), theirs.opened());

    // 前提取证：安全文件确实被拉到了目标版本，被拒那份一个字节没动。
    assert_eq!(ours.read("safe.txt"), "v2\n");
    assert_eq!(ours.read("readme.txt"), "local\n");
}

/// 被拒的文件落在排除项里：那句 `info` 仍然是**拒绝**，不是「要补查的已打开文件」。
///
/// 只读预演覆盖整棵目录（排除是工具侧的硬边界，原生不认），所以排除目录里的拒绝照样会
/// 出现在预演里；把它当成待解释的提示，整轮就会在写入前失败，而这轮本来一个字都不该写。
#[test]
fn a_refusal_inside_an_excluded_entry_is_never_written_to() {
    let Some(sandbox) = sandbox_with_allwrite_noclobber() else {
        return;
    };

    sandbox.write(".p4delta-scope", r#"{"exclude": [{"dir": "generated"}]}"#);
    sandbox.commit("generated/tracked.txt", "v1\n");
    sandbox.p4_ok(&["edit", "generated/tracked.txt"]);
    sandbox.write("generated/tracked.txt", "v2\n");
    sandbox.p4_ok(&["submit", "-d", "v2"]);
    sandbox.p4_ok(&["sync", "//depot/main/generated/tracked.txt#1"]);
    sandbox.write("generated/tracked.txt", "local\n");

    let have_before = sandbox.p4_ok(&[
        "fstat",
        "-T",
        "haveRev",
        "//depot/main/generated/tracked.txt",
    ]);

    sandbox.cli().args(["--sync", "-a", "."]).assert().success();

    assert_eq!(sandbox.read("generated/tracked.txt"), "local\n");
    assert_eq!(
        sandbox.p4_ok(&[
            "fstat",
            "-T",
            "haveRev",
            "//depot/main/generated/tracked.txt"
        ]),
        have_before,
        "排除项里的文件连 have 都不该被推"
    );
}

/// 被拒的文件与「已打开、have 落后」的文件在同一轮：拒绝被滤掉，已打开那条照旧走补查。
///
/// 这条防的是把「滤掉拒绝」写成「滤掉所有 `info`」——那样已打开的文件会被静默漏掉，
/// 而原生会动它们。
#[test]
fn a_refusal_next_to_an_opened_file_leaves_the_opened_one_to_native() {
    let (Some(ours), Some(theirs)) = (
        sandbox_with_allwrite_noclobber(),
        sandbox_with_allwrite_noclobber(),
    ) else {
        return;
    };

    for sandbox in [&ours, &theirs] {
        // 被拒：落后一版、本地改了、未打开。先做：它那次 `p4 submit` 会带上默认
        // changelist 里所有打开的文件。
        behind_then_modified(sandbox, "src/lib.txt", "local lib\n");
        // 已打开、have 落后：原生只给 info 提示，工具靠补查 `p4 opened` 才认得出来。
        sandbox.p4_ok(&["edit", "readme.txt"]);
        sandbox.write("readme.txt", "first local\n");
        sandbox.p4_ok(&["submit", "-d", "first local"]);
        sandbox.p4_ok(&["sync", "//depot/main/readme.txt#1"]);
        sandbox.p4_ok(&["edit", "readme.txt"]);
        sandbox.write("readme.txt", "second local\n");
    }

    ours.cli().args(["--sync", "-a", "."]).assert().success();
    theirs.p4_ok(&["sync"]);

    assert_eq!(ours.read("readme.txt"), theirs.read("readme.txt"));
    assert_eq!(ours.opened(), theirs.opened());
    assert_eq!(resolve_state(&ours), resolve_state(&theirs));
    assert_eq!(
        ours.read("src/lib.txt"),
        "local lib\n",
        "被拒的文件不该被动"
    );
}

/// `--to <CL>`：往回退到目标那一版，而不是停在 head 上。
#[test]
fn to_a_changelist_walks_back_to_that_state_not_to_head() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.commit("a.txt", "first version\n");
    let target = latest_submitted(&sandbox);

    sandbox.p4_ok(&["edit", "a.txt"]);
    sandbox.write("a.txt", "second version\n");
    sandbox.p4_ok(&["submit", "-d", "second version"]);
    sandbox.commit("b.txt", "created after the target\n");

    // 前提：两边都在 head 上，否则「退回了旧版」证明不了什么。
    let before = sandbox.p4_ok(&["fstat", "-T", "headRev haveRev", "a.txt"]);
    assert!(
        before.contains("headRev 2") && before.contains("haveRev 2"),
        "unexpected depot state: {before}"
    );

    sandbox
        .cli()
        .args(["--sync", "--to", &target.to_string(), "-a", "-l", "."])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "Sync mode: updating the workspace to changelist {target}."
        )))
        .stdout(predicate::str::contains("Updating 1 files"))
        // 目标时刻还不存在的路径：从工作区移除。
        .stdout(predicate::str::contains("Removing 1 files"));

    let after = sandbox.p4_ok(&["fstat", "-T", "headRev haveRev", "a.txt"]);
    assert!(
        after.contains("headRev 2"),
        "depot 的 head 不该被动过：{after}"
    );
    assert!(
        after.contains("haveRev 1"),
        "have 该退回目标那一版：{after}"
    );
    assert_eq!(sandbox.read("a.txt"), "first version\n");
    assert!(!sandbox.exists("b.txt"), "目标时刻不存在，本地那份该被移除");
    assert!(sandbox.p4_lines(&["have", "b.txt"]).is_empty());
}

/// 目标早于整个目录的出现：普通同步**允许**原生认可的历史空目标，把本地文件删掉并清 have。
///
/// 与强制修复那条护栏（`a_target_changelist_that_predates_the_folder_is_refused`）刻意不同：
/// 那边的护栏建立在「目标整体为空」这个由本工具推断出来的结论上；普通同步不做任何推断，
/// 它只是把 `@CL` 交回给原生——用户手工敲 `p4 sync ...@CL` 会发生什么，这里就发生什么。
#[test]
fn an_early_target_matches_what_native_p4_would_do() {
    let (Some(ours), Some(theirs)) = (support::sandbox_or_skip(), support::sandbox_or_skip())
    else {
        return;
    };

    let mut target = 0;
    for sandbox in [&ours, &theirs] {
        target = latest_submitted(sandbox);
        sandbox.commit("sub/only.txt", "in the subdirectory\n");
    }

    ours.cli()
        .args(["--sync", "--to", &target.to_string(), "-a", "."])
        .assert()
        .success();

    theirs.p4_ok(&["sync", &format!("...@{target}")]);

    assert_eq!(ours.exists("sub/only.txt"), theirs.exists("sub/only.txt"));
    assert_eq!(
        ours.p4_lines(&["have", "sub/only.txt"]),
        theirs.p4_lines(&["have", "sub/only.txt"])
    );
    assert!(
        !ours.exists("sub/only.txt"),
        "前提：原生认可的历史空目标确实会删掉本地文件"
    );
}

/// scope 是硬边界：只读查询可以覆盖被排除的部分，但一个字都不许写下去。
///
/// 同前一条，排除来自配置——`--exclude-*` 与普通同步冲突，配置里的排除照旧生效。
#[test]
fn an_excluded_directory_is_never_written_to() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write(".p4delta-scope", r#"{"exclude": [{"dir": "generated"}]}"#);
    // generated 里既有已跟踪的落后文件，也有 depot 里没有的新文件。
    sandbox.commit("generated/tracked.txt", "first\n");
    sandbox.p4_ok(&["edit", "generated/tracked.txt"]);
    sandbox.write("generated/tracked.txt", "second\n");
    sandbox.p4_ok(&["submit", "-d", "second"]);
    sandbox.p4_ok(&["sync", "//depot/main/generated/tracked.txt#1"]);
    sandbox.write("generated/scratch.txt", "never committed\n");

    sandbox
        .cli()
        .args(["--sync", "-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("generated").not());

    // 排除项里的文件一动不动：既没被拉到 head，也没被删。
    assert_eq!(
        sandbox.read("generated/tracked.txt"),
        "first\n",
        "排除目录里的文件不该被同步"
    );
    assert!(sandbox.exists("generated/scratch.txt"));
}

/// 隐式排除：生效的 `.p4delta-scope` 自身既不会被报成动作，也不会被写。
///
/// 场景刻意让它**落后于 head**：不排除的话原生真的会报一条 `updated`，工具就会把用户
/// 手里的范围配置覆盖成 depot 里那份——而这份配置正是它自己刚读过的。
#[test]
fn the_implicit_scope_file_is_never_touched() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    const SCOPE_NAME: &str = ".p4delta-scope";
    // 只写排除项：include 省略即整个 client root，于是这个文件自己正好落在范围之内——
    // 隐式排除要挡的就是它。
    const V1: &str = r#"{"exclude": [{"dir": "ignored"}]}"#;
    const V2: &str = r#"{"exclude": [{"dir": "ignored"}, {"dir": "build"}]}"#;

    sandbox.commit(SCOPE_NAME, V1);
    sandbox.p4_ok(&["edit", SCOPE_NAME]);
    sandbox.write(SCOPE_NAME, V2);
    sandbox.p4_ok(&["submit", "-d", "second scope"]);
    sandbox.p4_ok(&["sync", "//depot/main/.p4delta-scope#1"]);

    // 同一轮里放一件真正要做的事，证明这一轮确实干活了（否则「没被写」是空断言）。
    sandbox.commit("src/newer.txt", "first\n");
    sandbox.p4_ok(&["edit", "src/newer.txt"]);
    sandbox.write("src/newer.txt", "second\n");
    sandbox.p4_ok(&["submit", "-d", "second"]);
    sandbox.p4_ok(&["sync", "//depot/main/src/newer.txt#1"]);

    let output = sandbox
        .cli()
        .args(["--sync", "-a", "-l", "."])
        .output()
        .expect("run the tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Updating 1 files"), "{stdout}");

    // 报告里不能有它。开场那句 `Using scope file "…"` 不算——只看清单里的动作行，
    // 那样才是在断言「它没被当成要动的文件」。
    let listed = support::listed_changes(&stdout);
    assert!(
        !listed.iter().any(|(_, path)| path.contains(SCOPE_NAME)),
        "范围配置文件被报成了动作：{listed:?}"
    );
    assert!(
        listed.iter().any(|(_, path)| path.ends_with("newer.txt")),
        "这一轮确实干了活：{listed:?}"
    );

    assert_eq!(
        sandbox.read(SCOPE_NAME),
        V1,
        "范围配置文件自身是隐式排除项，不该被同步覆盖"
    );
    assert_eq!(sandbox.read("src/newer.txt"), "second\n");
}

/// 入口定位不了时在写入之前报错，而不是丢掉它、拿更小的范围去问 p4。
#[test]
fn an_unresolvable_scope_entry_fails_before_anything_is_written() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.commit("moving.txt", "first revision\n");
    sandbox.p4_ok(&["edit", "moving.txt"]);
    sandbox.write("moving.txt", "second revision\n");
    sandbox.p4_ok(&["submit", "-d", "second revision"]);
    sandbox.p4_ok(&["sync", "//depot/main/moving.txt#1"]);

    // depot 路径不在这个 client 的 view 里：严格求值必须在写之前拒绝。
    let output = sandbox
        .cli()
        .args(["--sync", "-a", "//not-this-depot/main/..."])
        .output()
        .expect("run the tool");

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not in this client's view"),
        "该点名是范围求值失败：{stderr}"
    );
    // 拒绝发生在动作之前：落后那一版还在原处。
    assert_ne!(sandbox.read("moving.txt"), "second revision\n");
    let after = sandbox.p4_ok(&["fstat", "-T", "haveRev", "moving.txt"]);
    assert!(after.contains("haveRev 1"), "have 不该被动过：{after}");
}

/// Perforce 的转义字符（`#`、`@`、`%`）在本地文件名里是合法的，整条链路都要能对上。
#[test]
fn a_path_with_perforce_metacharacters_round_trips() {
    let Some(sandbox) = sandbox_with_weird_name() else {
        return;
    };

    sandbox
        .cli()
        .args(["--sync", "-a", "-l", "."])
        .assert()
        .success()
        .stdout(predicate::str::contains("Updating 1 files"))
        .stdout(predicate::str::contains("weird#1@2%3.txt"));

    assert_eq!(sandbox.read("weird#1@2%3.txt"), "second revision\n");
    let after = sandbox.p4_ok(&["fstat", "-T", "haveRev", "weird%231%402%253.txt"]);
    assert!(after.contains("haveRev 2"), "have 该到目标版本：{after}");
}

/// 入口路径**自己**带 Perforce 元字符——上一条测的是文件名，这条测的是 scope 交给 p4 的
/// 规格：本地目录名里的 `#`/`@`/`%` 要先转义成 depot 语法的样子，p4 才认得出这是同一个路径。
#[test]
fn a_scope_entry_with_perforce_metacharacters_round_trips() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    const DIR: &str = "odd#1@2%3";
    const LOCAL: &str = "odd#1@2%3/tracked.txt";
    const DEPOT: &str = "//depot/main/odd%231%402%253/tracked.txt";

    // `p4 add` 收的是客户端路径、不做转义解码，所以这里用原名；`edit` / `sync` 收的是
    // depot 规格，那里才用转义形式（同 `sandbox_with_weird_name` 的实测结论）。
    sandbox.write(LOCAL, "first\n");
    sandbox.p4_ok(&["add", "-f", LOCAL]);
    sandbox.p4_ok(&["submit", "-d", "first"]);
    sandbox.write(LOCAL, "second\n");
    sandbox.p4_ok(&["edit", DEPOT]);
    sandbox.p4_ok(&["submit", "-d", "second"]);
    sandbox.p4_ok(&["sync", &format!("{DEPOT}#1")]);

    sandbox
        .cli()
        .args(["--sync", "-a", DIR])
        .assert()
        .success()
        .stdout(predicate::str::contains("Updating 1 files"));

    assert_eq!(sandbox.read(LOCAL), "second\n");
    let after = sandbox.p4_ok(&["fstat", "-T", "haveRev", DEPOT]);
    assert!(after.contains("haveRev 2"), "have 该到目标版本：{after}");
}

/// 非 ASCII 文件名同样要能对上（沙箱的 P4CHARSET / P4COMMANDCHARSET 都是 utf8）。
///
/// 全程不把这个名字挂上 p4 的命令行：Windows 会先按系统 ANSI 代码页转一遍字节，
/// 中文名在那一步就变成 `????.txt`。`p4_ok_paths` 从 stdin 送，正是生产代码的做法。
#[test]
fn a_non_ascii_path_is_synced() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 让 `src/使用说明.txt` 落后一个版本：改它、提交、再把 have 拉回第一版。
    sandbox.write("src/使用说明.txt", "第二版\n");
    sandbox.p4_ok(&["edit", "src/..."]);
    sandbox.p4_ok(&["submit", "-d", "second revision"]);
    sandbox.p4_ok_paths(&["sync"], &["//depot/main/src/使用说明.txt#1"]);

    sandbox
        .cli()
        .args(["--sync", "-a", "-l", "."])
        .assert()
        .success()
        .stdout(predicate::str::contains("Updating 1 files"));

    assert_eq!(sandbox.read("src/使用说明.txt"), "第二版\n");
}

/// dry run 一寸不动：磁盘、have、opened、待 resolve，以及摘要缓存。
///
/// 缓存那一项是这条路「不走整条摘要管线」的直接证据：先让 open 模式建出缓存，再跑普通
/// 同步的预演，`Loading cache from` 那句必须一次都不出现。
#[test]
fn a_dry_run_changes_nothing_and_leaves_the_cache_alone() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.commit("moving.txt", "first revision\n");
    sandbox.p4_ok(&["edit", "moving.txt"]);
    sandbox.write("moving.txt", "second revision\n");
    sandbox.p4_ok(&["submit", "-d", "second revision"]);
    sandbox.p4_ok(&["sync", "//depot/main/moving.txt#1"]);

    sandbox.commit("gone.txt", "will be deleted\n");
    sandbox.p4_ok(&["delete", "gone.txt"]);
    sandbox.p4_ok(&["submit", "-d", "delete it"]);
    sandbox.p4_ok(&["sync", "//depot/main/gone.txt#1"]);

    sandbox.write("readme.txt", "locally changed\n");
    sandbox.p4_ok(&["edit", "src/deep/a/b/c.txt"]);

    // 先跑一次 reconcile 把缓存建出来——这条管线才会写它。
    let warmup = sandbox.cli().args(["-l", "."]).output().expect("run");
    assert!(warmup.status.success(), "{warmup:?}");
    let cache = sandbox.cache_path().expect("a cache path");
    let cache_before = std::fs::read(&cache).ok();
    assert!(cache_before.is_some(), "前提：缓存已经被建出来了");

    let before = sandbox.p4_ok(&["fstat", "-T", "haveRev", "..."]);
    let opened_before = sandbox.opened();

    let dry = sandbox
        .cli()
        .args(["--sync", "-l", "."])
        .output()
        .expect("run the tool");
    assert!(dry.status.success(), "{dry:?}");
    let stdout = String::from_utf8_lossy(&dry.stdout);

    // 报出了要做的三件事，但一件都没做。
    assert!(stdout.contains("Updating 1 files"), "{stdout}");
    assert!(stdout.contains("Removing 1 files"), "{stdout}");
    assert!(
        stdout.contains("Re-run with -a to sync the workspace."),
        "{stdout}"
    );

    assert_eq!(sandbox.read("moving.txt"), "first revision\n");
    assert_eq!(sandbox.read("readme.txt"), "locally changed\n");
    assert!(sandbox.exists("gone.txt"), "预演不该删文件");
    assert_eq!(sandbox.p4_ok(&["fstat", "-T", "haveRev", "..."]), before);
    assert_eq!(sandbox.opened(), opened_before, "opened 状态不该动");

    // 普通同步不碰摘要管线：那两句是它的入口。
    assert!(
        !stdout.contains("Loading cache from"),
        "普通同步不该读摘要缓存：{stdout}"
    );
    assert!(
        !stdout.contains("Querying file sync timestamps"),
        "普通同步不该查 have 时间戳：{stdout}"
    );
    assert!(
        !stdout.contains("Analyzing files against the target depot revision"),
        "普通同步不做本地的差异分析：{stdout}"
    );
    assert_eq!(
        std::fs::read(&cache).ok(),
        cache_before,
        "普通同步不该写摘要缓存"
    );
}

/// JSON 契约：一次输出只发一套文件记录，计数去重，`stage` / `nativeAction` / `force` 都在位。
#[test]
fn the_json_stream_reports_one_stage_and_a_trustworthy_summary() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.commit("moving.txt", "first revision\n");
    sandbox.p4_ok(&["edit", "moving.txt"]);
    sandbox.write("moving.txt", "second revision\n");
    sandbox.p4_ok(&["submit", "-d", "second revision"]);
    sandbox.p4_ok(&["sync", "//depot/main/moving.txt#1"]);
    sandbox.commit("fresh.txt", "brand new\n");
    sandbox.p4_ok(&["sync", "//depot/main/fresh.txt#none"]);

    let records = run_json(&sandbox, &["--json", "--sync", "-a", "."]);
    let files = files_in(&records);

    // 一次输出只有一套记录，且都标着 apply 阶段——预演那份不会重复发一遍。
    assert_eq!(files.len(), 2, "{files:?}");
    assert!(
        files.values().all(|record| record["stage"] == "apply"),
        "{files:?}"
    );
    assert!(files.values().all(|record| record["applied"] == true));
    assert!(
        files.values().all(|record| record["force"] == false),
        "普通同步的 force 必须是 false：{files:?}"
    );
    assert_eq!(files["//depot/main/moving.txt"]["nativeAction"], "updated");
    assert_eq!(files["//depot/main/fresh.txt"]["nativeAction"], "added");

    let summary = summary_in(&records);
    assert_eq!(summary["ok"], true);
    assert_eq!(summary["mode"], "sync");
    assert_eq!(summary["force"], false);
    assert_eq!(summary["total"], 2);
    assert_eq!(summary["counts"]["update"], 1);
    assert_eq!(summary["counts"]["add"], 1);
    // 普通同步拿不到「入口匹配了几个」这个结论，发 null——不知道不是零匹配。
    assert!(
        summary["scopeMatched"].is_null(),
        "scopeMatched 该是 null：{summary}"
    );
    assert_eq!(records_summary_count(&records), 1, "summary 恰好一条");
}

/// 逐文件的失败即便 p4 退出码是 0 也要让整轮失败，且 stdout 仍是纯 JSON。
#[test]
fn a_per_file_failure_fails_the_round_with_clean_json() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 让 p4 写不进去：同名位置放一个目录。
    sandbox.commit("blocked.txt", "from the depot\n");
    sandbox.p4_ok(&["sync", "//depot/main/blocked.txt#none"]);
    std::fs::create_dir_all(sandbox.client_root().join("blocked.txt")).expect("block the path");

    let output = sandbox
        .cli()
        .args(["--json", "--sync", "-a", "."])
        .output()
        .expect("run the tool");

    assert!(!output.status.success(), "{output:?}");

    let records = parse_json_lines(&output.stdout);
    let summary = summary_in(&records);
    assert_eq!(summary["ok"], false, "{summary}");
    assert_eq!(
        records_summary_count(&records),
        1,
        "任何失败路径都恰好一条 summary"
    );
    assert!(
        !records.is_empty(),
        "stdout 必须整份是 JSON Lines：{:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// 造一个「已打开、have 落后于 head」的现场。
///
/// 这是原生**只给 `info` 提示、不给正文记录**的那一类（`readme.txt#2 - is opened and not
/// being changed`），普通同步得靠补查 `p4 opened` 才认得出来，所以几条用例共用它。
fn sandbox_with_an_opened_file_behind_head() -> Option<support::Sandbox> {
    let sandbox = support::sandbox_or_skip()?;

    sandbox.p4_ok(&["edit", "readme.txt"]);
    sandbox.write("readme.txt", "first local\n");
    sandbox.p4_ok(&["submit", "-d", "first local"]);
    sandbox.p4_ok(&["sync", "//depot/main/readme.txt#1"]);
    sandbox.p4_ok(&["edit", "readme.txt"]);
    sandbox.write("readme.txt", "second local\n");

    Some(sandbox)
}

/// 起一个 `allwrite noclobber` 的沙箱——用户现场那一份客户端配置。
///
/// 同一份「本地改了、又没打开」的现场在两种配置下是**两种形态**：`noallwrite` 靠只读位
/// 判定，原生直接报 `Can't clobber writable file`（severity 3、exit 1）整轮中止；`allwrite`
/// 只能靠内容/时间戳判定，拒绝变成逐文件的 `info`（exit 0），正是这条路要处理的形态。
fn sandbox_with_allwrite_noclobber() -> Option<support::Sandbox> {
    let sandbox = support::sandbox_or_skip()?;
    sandbox.set_client_options("allwrite noclobber nocompress unlocked nomodtime normdir");
    // 让工作区形状与 allwrite 客户端一致：文件可写、have 在 head。
    sandbox.sync();
    Some(sandbox)
}

/// 让 `file` 落后一版（have 停在第 1 版），再本地改掉它——原生 noclobber 要拦的正是这种。
fn behind_then_modified(sandbox: &support::Sandbox, file: &str, local: &str) {
    sandbox.p4_ok(&["edit", file]);
    sandbox.write(file, "second revision\n");
    sandbox.p4_ok(&["submit", "-d", "second revision"]);
    sandbox.p4_ok(&["sync", &format!("//depot/main/{file}#1")]);
    sandbox.write(file, local);
}

/// 探针风格的场景：造一个含 Perforce 转义字符的文件并让它落后于 head。
///
/// `p4 add` 的实参是**客户端路径**，不做转义解码（实测：给它转义形式会去开一个名字里
/// 带 `%` 的文件，然后 submit 失败），所以加文件用原名；`edit` / `sync` / `fstat` 收的是
/// depot 规格，那里才用 `%23` 这类转义。
fn sandbox_with_weird_name() -> Option<support::Sandbox> {
    let sandbox = support::sandbox_or_skip()?;

    const LOCAL: &str = "weird#1@2%3.txt";
    const DEPOT: &str = "//depot/main/weird%231%402%253.txt";

    sandbox.write(LOCAL, "first revision\n");
    sandbox.p4_ok(&["add", "-f", LOCAL]);
    sandbox.p4_ok(&["submit", "-d", "special name"]);

    sandbox.write(LOCAL, "second revision\n");
    sandbox.p4_ok(&["edit", DEPOT]);
    sandbox.p4_ok(&["submit", "-d", "second revision"]);
    sandbox.p4_ok(&["sync", &format!("{DEPOT}#1")]);

    Some(sandbox)
}

// ---- 取证小工具 ----

/// 待 resolve 的清单，逐行归一。
///
/// 行首是本地路径（两个沙箱的实例目录不同），分开保留会让对拍永远不等；只留文件名
/// 与「 - 」之后的原生理由（含 depot 路径与版本），那才是要比的东西。
fn resolve_state(sandbox: &support::Sandbox) -> Vec<String> {
    let output = sandbox.p4(&["resolve", "-n"]);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(|line| match line.split_once(" - ") {
            Some((path, reason)) => format!("{} - {reason}", basename(path)),
            None => line.to_owned(),
        })
        .collect()
}

/// 跑一次 `--json`，把 stdout 解析成 JSON Lines。
fn run_json(sandbox: &support::Sandbox, args: &[&str]) -> Vec<Value> {
    let output = sandbox.cli().args(args).output().expect("run the tool");
    assert!(output.status.success(), "{output:?}");
    parse_json_lines(&output.stdout)
}

fn parse_json_lines(stdout: &[u8]) -> Vec<Value> {
    let text = String::from_utf8_lossy(stdout);
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("stdout must be JSON Lines only: {error}\n{line}"))
        })
        .collect()
}

/// 记录流里的文件记录，按 `depotFile` 建索引——同一文件的重复记录会在这里暴露出来。
fn files_in(records: &[Value]) -> BTreeMap<String, Value> {
    let mut files = BTreeMap::new();
    for record in records {
        if record["kind"] != "file" {
            continue;
        }
        let depot_file = record["depotFile"]
            .as_str()
            .expect("a file record always carries depotFile")
            .to_owned();
        assert!(
            files.insert(depot_file.clone(), record.clone()).is_none(),
            "同一个文件出现了两条记录：{depot_file}"
        );
    }
    files
}

fn summary_in(records: &[Value]) -> &Value {
    records
        .iter()
        .find(|record| record["kind"] == "summary")
        .expect("every run ends with a summary")
}

fn records_summary_count(records: &[Value]) -> usize {
    records
        .iter()
        .filter(|record| record["kind"] == "summary")
        .count()
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

/// 清单归一成 (动作, 文件名) 并排序，理由同 `e2e_sync.rs` 的同名函数。
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
