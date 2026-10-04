//! `--json` 输出契约的端到端验证。
//!
//! 契约的单一真相是 `docs/json-contract.md`。这里只钉那些**消费方会依赖、而单测盯不住**
//! 的条款：stdout 通道的纯净、summary 恒有、`--no-revert-groups` 与原生逐行对齐、
//! 入口全落空的判决、以及失败时记录流仍然完整。
//!
//! 断言落在「消费方解析出来的字段」上，而不是 stdout 的字符串形状——后者单测已经盖住。
//! 解析本身也是断言：stdout 里混进一行人类文字，这里就会炸。

mod support;

use std::collections::BTreeSet;

use serde_json::{Value, json};

use support::Sandbox;

/// 一轮 `--json` 的结果。
struct Run {
    ok: bool,
    records: Vec<Value>,
    stderr: String,
}

/// 跑一轮 `--json`。两个开关是编辑器的标准调用方式，所有用例共用：
/// `--json` 是本契约，`--no-scope-file` 表示范围完全由调用方给定。
fn run_json(sandbox: &Sandbox, args: &[&str]) -> Run {
    let output = sandbox
        .cli()
        .arg("--json")
        .arg("--no-scope-file")
        .args(args)
        .output()
        .expect("run tool");

    let stdout = String::from_utf8(output.stdout).expect("stdout must be UTF-8");
    let records = stdout
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|error| panic!("stdout 里混进了非 JSON 行: {line:?} ({error})"))
        })
        .collect();

    Run {
        ok: output.status.success(),
        records,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

impl Run {
    fn files(&self) -> Vec<&Value> {
        self.records
            .iter()
            .filter(|record| record["kind"] == "file")
            .collect()
    }

    /// 所有文件记录的 `class`，按出现顺序。
    fn classes(&self) -> Vec<&str> {
        self.files()
            .iter()
            .map(|record| record["class"].as_str().expect("class 必须是字符串"))
            .collect()
    }

    /// 某个 `class` 下唯一的那条记录。
    fn only(&self, class: &str) -> &Value {
        let matching: Vec<&Value> = self
            .files()
            .into_iter()
            .filter(|record| record["class"] == class)
            .collect();

        assert_eq!(matching.len(), 1, "期望恰好一条 {class} 记录：{matching:?}");
        matching[0]
    }

    /// 契约的头一条硬约束：**没有 summary 就等于没有结论**，所以任何一轮都得以它收尾。
    fn summary(&self) -> &Value {
        let last = self
            .records
            .last()
            .unwrap_or_else(|| panic!("stdout 一条记录都没有"));
        assert_eq!(
            last["kind"], "summary",
            "最后一条记录必须是 summary：{last}"
        );
        last
    }
}

/// 三模式共用的前置：一份新增、一份改动、一份删除，服务器上什么都没打开。
fn offline_drift(sandbox: &Sandbox) {
    sandbox.write("readme.txt", "changed locally\n");
    sandbox.write("fresh.txt", "brand new\n");
    sandbox.remove("src/lib.txt");
}

/// 记录集合的归一化：只留「动作 + 文件名」。
///
/// 两条引擎一个报 client 语法、一个报本地路径，中间隔着 client view 的映射；
/// 这里只关心「哪些文件、什么动作」，文件名足以把差异逼出来（种子里的 basename 唯一）。
fn normalized(records: impl IntoIterator<Item = (String, String)>) -> BTreeSet<(String, String)> {
    records
        .into_iter()
        .map(|(action, path)| {
            let path = path.replace('\\', "/");
            let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
            (action.to_lowercase(), name)
        })
        .collect()
}

/// δ 的记录集合。
fn our_changes(run: &Run) -> BTreeSet<(String, String)> {
    normalized(
        run.files()
            .iter()
            .map(|record| {
                (
                    record["action"].as_str().expect("action 必须有").to_owned(),
                    record["clientFile"]
                        .as_str()
                        .expect("clientFile 必须有")
                        .to_owned(),
                )
            })
            .collect::<Vec<_>>(),
    )
}

/// 原生 `p4 reconcile -n -a -e -d` 的记录集合。
fn native_changes(sandbox: &Sandbox) -> BTreeSet<(String, String)> {
    let lines = sandbox.p4_lines(&["-Mj", "-Ztag", "reconcile", "-n", "-a", "-e", "-d", "..."]);

    normalized(
        lines
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|record| {
                Some((
                    record["action"].as_str()?.to_owned(),
                    record["clientFile"].as_str()?.to_owned(),
                ))
            })
            .collect::<Vec<_>>(),
    )
}

/// 三模式里 `open` 是编辑器的默认档：stdout 只有记录、summary 收尾、`clientFile` 是
/// client 语法。这三件事任何一件破了，消费方当场解析失败或把结果读错。
#[test]
fn stdout_carries_only_records_and_the_summary_closes_the_stream() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };
    offline_drift(&sandbox);

    // 带上 `-l`：清单是给人看的，`--json` 下必须整体改道 stderr。
    let run = run_json(&sandbox, &["-l", "."]);

    assert!(run.ok, "exit code not success:\n{}", run.stderr);
    let summary = run.summary();
    assert_eq!(summary["mode"], "open");
    assert_eq!(summary["ok"], true);
    assert_eq!(summary["applied"], false);
    assert_eq!(summary["total"], 3);
    assert_eq!(summary["counts"], json!({"add": 1, "delete": 1, "edit": 1}));
    assert_eq!(summary["unmatched"], 0);
    assert_eq!(summary["scopeMatched"], 1);
    assert!(summary["reason"].is_null(), "{summary}");

    // `clientFile` 是 client 语法（消费方拿它翻回本地路径），`depotFile` 是 depot 路径。
    // 两条都不带 `--client-root` 也要拼得出来：δ 自己去问一次 `p4 info`。
    let edit = run.only("edit");
    assert_eq!(
        edit["clientFile"],
        format!("//{}/readme.txt", sandbox.client()),
        "{edit}"
    );
    assert_eq!(edit["depotFile"], "//depot/main/readme.txt", "{edit}");
    assert_eq!(run.only("delete")["depotFile"], "//depot/main/src/lib.txt");

    // 预演不带 `applied`：这一轮没真改任何东西。
    for record in run.files() {
        assert_eq!(record["applied"], false, "{record}");
    }

    // 人类可读的报告整体改道 stderr——包括 `-l` 的清单与统计行。
    assert!(run.stderr.contains("Editing 1 files"), "{}", run.stderr);
    assert!(
        run.stderr.contains("Inconsistencies found"),
        "{}",
        run.stderr
    );
    assert!(sandbox.opened().is_empty(), "预演不许在服务器上动任何东西");
}

/// 三组 `revert_*` 是 `p4 revert -a` 的语义，不在 `p4 reconcile -a -e -d` 里。
/// 默认档报出来，`--no-revert-groups` 摘掉——摘掉之后必须与原生**逐行**对齐，
/// 那是这个开关存在的全部理由（编辑器要在两条引擎之间做集合比较）。
#[test]
fn no_revert_groups_matches_native_reconcile() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 未打开的三类：新增、改动、缺失。
    //
    // `commit` 之后必须 `sync` 一次：submit 会把当时那份（被 `write` 回拨过的）mtime 记成
    // have 的 syncTime，紧接着再回拨一次正好落进 ±1 秒的捷径窗口，改动会被静默当成「没变」。
    // sync 把 syncTime 拉到当下，两边就拉开了一小时。
    sandbox.commit("edited.txt", "baseline\n");
    sandbox.sync();
    sandbox.write("edited.txt", "changed locally\n"); // edit
    sandbox.write("fresh.txt", "brand new\n"); // add
    sandbox.remove("src/lib.txt"); // delete

    // 已打开、磁盘上却不一致的两类：p4 那边要改开（reopen_*）。
    sandbox.p4_ok(&["delete", "-k", "logo.bin"]);
    sandbox.write("logo.bin", "changed after being marked for delete\n"); // reopen_edit
    sandbox.p4_ok(&["edit", "src/deep/a/b/c.txt"]);
    sandbox.remove("src/deep/a/b/c.txt"); // reopen_delete

    // 已打开、磁盘上与 have 一致的三类：只有 `p4 revert -a` 才管它们。
    sandbox.p4_ok(&["edit", "src/使用说明.txt"]); // 内容没改 → revert_edit
    sandbox.p4_ok(&["delete", "-k", "readme.txt"]); // 内容没改 → revert_delete
    sandbox.write("ghost.txt", "brand new\n");
    sandbox.p4_ok(&["add", "ghost.txt"]);
    sandbox.remove("ghost.txt"); // 过时的 add → revert_add

    let full = run_json(&sandbox, &["."]);
    let mut classes = full.classes();
    classes.sort_unstable();
    assert_eq!(
        classes,
        [
            "add",
            "delete",
            "edit",
            "reopen_delete",
            "reopen_edit",
            "revert_add",
            "revert_delete",
            "revert_edit",
        ],
        "默认档八类各一：{classes:?}\n{}",
        full.stderr
    );

    let ours = our_changes(&run_json(&sandbox, &["--no-revert-groups", "."]));
    let theirs = native_changes(&sandbox);

    assert_eq!(ours, theirs, "摘掉 revert 三组后必须与原生逐行对齐");
    assert!(
        ours.iter().all(|(_, name)| !name.starts_with("ghost")),
        "过时的 add 只在 revert_add 里，原生不该报它：{ours:?}"
    );
    assert!(
        ours.iter().all(|(_, name)| name != "使用说明.txt"),
        "「已打开但没改动」的文件原生一个字都不报：{ours:?}"
    );
    // 唯独「文件还在、却被 open for delete」不是消失而是改判：原生 `-e` 一律把它改开成
    // edit，**与内容改没改无关**（实测）。δ 的默认档把它当成过时的打开撤销掉，
    // 这一档得跟着原生走，否则两边对同一个工作区给出的清单会差一条。
    assert!(
        ours.contains(&("edit".to_owned(), "readme.txt".to_owned())),
        "改成 edit 的那一类必须出现在原生那一侧：{ours:?}"
    );
    assert!(
        ours.iter()
            .any(|(action, name)| action == "edit" && name == "logo.bin"),
        "改开那两类原生照报（只是措辞不同）：{ours:?}"
    );
}

/// 入口全落空在文本模式下是 `bail!`（退出码 1）。`--json` 下退出码不变，但判决要能被
/// 程序读出来：逐条 `unmatched` + summary 的 `ok:false, reason:"no-entry-matched"`。
/// 编辑器把「删除整个目录」拆成成对的入口发过来，两个都落空是**正常的答案**。
#[test]
fn entries_that_match_nothing_are_reported_as_unmatched() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    let run = run_json(&sandbox, &["./gone.txt"]);

    assert!(!run.ok, "入口全落空仍是非零退出：{}", run.stderr);
    let summary = run.summary();
    assert_eq!(summary["ok"], false);
    assert_eq!(summary["reason"], "no-entry-matched");
    assert_eq!(summary["unmatched"], 1);
    assert_eq!(summary["scopeMatched"], 0);
    assert_eq!(summary["total"], 0);
    assert_eq!(summary["counts"], json!({}));

    // 落空的入口逐条报出来：消费方要按路径判断哪些被认定为空，而不是只看退出码。
    assert_eq!(run.records[0]["kind"], "unmatched");
    assert!(
        run.records[0]["path"]
            .as_str()
            .expect("path 必须有")
            .ends_with("gone.txt"),
        "{}",
        run.records[0]
    );
}

/// 一处失败不许把整轮记录流变成半截：失败那一组的记录照样发全，成败由 summary 说。
/// 用 `@` 命名的文件当失败源——p4 的参数语法里它是版本说明符，`p4 add` 一律拒收。
#[test]
fn a_failed_group_still_leaves_a_complete_stream_and_an_honest_summary() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("fresh.txt", "brand new\n");
    sandbox.write("report@2024.txt", "bad name\n");

    let run = run_json(&sandbox, &["-a", "."]);

    assert!(!run.ok, "有一组落不下去，退出码必须非零：{}", run.stderr);
    let summary = run.summary();
    assert_eq!(summary["ok"], false);
    assert_eq!(summary["reason"], "error");
    assert_eq!(summary["applied"], true);
    assert_eq!(summary["counts"], json!({"add": 2}));

    // 两条记录都在，且都标成 `applied:true`：这个字段说的是「这一轮真改了状态」，
    // 逐文件的成败由 summary 与 stderr 上的错误承担。
    let names: Vec<&str> = run
        .files()
        .iter()
        .map(|record| record["clientFile"].as_str().expect("clientFile 必须有"))
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(
        names.iter().any(|name| name.ends_with("fresh.txt")),
        "{names:?}"
    );
    assert!(
        names.iter().any(|name| name.ends_with("report@2024.txt")),
        "{names:?}"
    );
    for record in run.files() {
        assert_eq!(record["applied"], true, "{record}");
    }

    // 失败只吃掉那一组里的那一个文件：另一个该真的开出来。
    let opened = sandbox.opened();
    assert!(
        opened
            .iter()
            .any(|line| line.contains("fresh.txt") && line.contains(" - add ")),
        "{opened:?}"
    );
    assert!(
        !opened.iter().any(|line| line.contains("report")),
        "p4 拒收的名字不该出现在 opened 里：{opened:?}"
    );
}

/// clean 与 sync 各有自己的分类学：同一个 `delete` 在 clean 下是「删掉本地文件」，
/// 在 sync 下是「目标时刻它不该在」。`class` 必须配 `mode` 读，两个模式各钉一遍。
#[test]
fn clean_and_sync_report_their_own_class_tables() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("fresh.txt", "brand new\n"); // clean: 删掉 depot 里没有的
    sandbox.write("readme.txt", "changed locally\n"); // clean: 还原本地改动
    sandbox.remove("src/lib.txt"); // clean: 写回缺失的

    let clean = run_json(&sandbox, &["--clean", "."]);

    assert!(clean.ok, "{}", clean.stderr);
    let mut classes = clean.classes();
    classes.sort_unstable();
    assert_eq!(classes, ["delete", "restore", "revert"], "{}", clean.stderr);
    assert_eq!(clean.only("delete")["action"], "deleting");
    assert_eq!(clean.only("revert")["action"], "reverting");
    assert_eq!(clean.only("restore")["action"], "restoring");
    assert_eq!(clean.summary()["mode"], "clean");
    assert_eq!(clean.summary()["total"], 3);

    // sync：把一个文件钉回旧版本。clean 那一步造出来的本地状态留着不动——
    // sync 会按自己的分类学把它们各归一类（改动的还原、缺失的写回），正好一并钉住。
    sandbox.commit("newer.txt", "v1\n");
    sandbox.p4_ok(&["edit", "newer.txt"]);
    sandbox.write("newer.txt", "v2\n");
    sandbox.p4_ok(&["submit", "-d", "second revision"]);
    sandbox.p4_ok(&["sync", "-f", "newer.txt#1"]);

    let sync = run_json(&sandbox, &["--sync", "."]);

    assert!(sync.ok, "{}", sync.stderr);
    let mut classes = sync.classes();
    classes.sort_unstable();
    assert_eq!(classes, ["restore", "revert", "update"], "{}", sync.stderr);
    let update = sync.only("update");
    assert_eq!(update["action"], "updating");
    assert_eq!(update["depotFile"], "//depot/main/newer.txt");
    assert_eq!(update["rev"], "2", "目标版本要跟着记录一起报出来");
    assert_eq!(sync.only("restore")["action"], "restoring");
    assert_eq!(sync.only("revert")["action"], "reverting");
    // 本地有、depot 里没有的文件（上一步造出来的 `fresh.txt`）sync 不碰：
    // 删它是 clean 的事，sync 的语义是「只传真正需要传的文件」。
    assert!(
        sync.files().iter().all(|record| !record["clientFile"]
            .as_str()
            .expect("clientFile 必须有")
            .ends_with("fresh.txt")),
        "{}",
        sync.stderr
    );
    assert_eq!(sync.summary()["mode"], "sync");
}

/// 编辑器按路径逐条窄查时会传 `<dir>/...` 这种带省略号的入口（目录级收集）。
/// 契约声明这类入口照收，且**不做百分号转义**：消费方给什么路径就是什么路径。
#[test]
fn a_directory_spec_is_accepted_as_a_scope_entry() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("src/deep/a/b/c.txt", "changed locally\n");

    let run = run_json(&sandbox, &["src/..."]);

    assert!(run.ok, "exit code not success:\n{}", run.stderr);
    assert_eq!(
        run.only("edit")["depotFile"],
        "//depot/main/src/deep/a/b/c.txt"
    );
    assert_eq!(run.summary()["scopeMatched"], 1);
    assert_eq!(run.summary()["unmatched"], 0);
}

/// 编辑器拼参数时会把命令名与开关一起吃进来，这里兜住「忘了 `--json`」的退化：
/// 不开 `--json` 时 stdout 仍是人类可读的，消费方解析当场失败——那是**有意**的，
/// 免得两条通道混在一起。用例只钉「开与不开是两种输出」。
#[test]
fn without_the_flag_stdout_stays_human_readable() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };
    offline_drift(&sandbox);

    let output = sandbox
        .cli()
        .args(["--no-scope-file", "."])
        .output()
        .expect("run tool");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Editing 1 files"), "{stdout}");
    assert!(
        !stdout.contains("\"kind\""),
        "文本模式的 stdout 不该出现记录：{stdout}"
    );
}

/// `--client-root` 与 clientspec 的 Root 对不上时，退化成 本地路径 的 `clientFile` 必须
/// 留痕：**第一条逐条点名，其余按整轮汇总**。记录流本身看不出这种退化（就是一个不以
/// `//` 开头的值），而真实触发面不只是「用户写错根」——盘符大小写、junction 形式的根、
/// client view 把文件映射到根之外，都会让一部分记录悄悄换一种拼法。
///
/// 节流是契约的一部分：一个工作区上万个文件，逐条告警会把 stderr 淹掉。
#[test]
fn a_mismatched_client_root_degrades_loudly_but_only_once() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };
    offline_drift(&sandbox);

    let run = run_json(&sandbox, &["--client-root", "/not/the/client/root", "."]);

    assert!(run.ok, "{}", run.stderr);
    for record in run.files() {
        let client_file = record["clientFile"].as_str().expect("clientFile 必须有");
        assert!(
            !client_file.starts_with("//"),
            "拼不出 client 语法就该原样退回本地路径：{record}"
        );
    }

    assert_eq!(
        run.stderr.matches("is not under the client root").count(),
        1,
        "退化的第一条逐条点名，其余不许刷屏：{}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains("3 record(s) had clientFile fall back to the local path"),
        "整轮结束时要有条数汇总：{}",
        run.stderr
    );
}

/// 某个 depot 路径名下的记录（正常情况下恰好一条，重复就是同一批被报了两次）。
fn records_for<'a>(run: &'a Run, depot_file: &str) -> Vec<&'a Value> {
    run.files()
        .into_iter()
        .filter(|record| record["depotFile"] == depot_file)
        .collect()
}

/// 记录流里的 depot 路径集合。硬条款 6 比的是「同一批文件」——两条引擎的路径拼法、
/// 记录形状都可能不同，能直接比的就是这个集合。
fn depot_files(run: &Run) -> BTreeSet<String> {
    run.files()
        .iter()
        .map(|record| {
            record["depotFile"]
                .as_str()
                .expect("depotFile 必须有")
                .to_owned()
        })
        .collect()
}

/// 提交一个 δ 算不出摘要的文件——转交原生 p4 的那一类。
///
/// 首选 `resource`（Apple 资源叉，代码与文档里一路拿它当例子）。少数平台的 p4 客户端不收
/// 这个类型（macOS 上没有资源叉的普通文件、某些 Windows 版本），退到 `ctext`：δ 的类型
/// 解析表里同样没有压缩文本，它一样会被转交。这条用例要钉的是「δ 认不出的类型要被转交」，
/// 不是 `resource` 这个名字本身。
fn commit_unsupported(sandbox: &Sandbox, relative: &str, contents: &str) {
    sandbox.write(relative, contents);
    if !sandbox
        .p4(&["add", "-t", "resource", relative])
        .status
        .success()
    {
        // 认不出类型的那次调用可能已经把文件挂上了 changelist，先摘掉再换类型重来。
        let _ = sandbox.p4(&["revert", "-k", relative]);
        sandbox.p4_ok(&["add", "-t", "ctext", relative]);
    }
    sandbox.p4_ok(&["submit", "-d", "unsupported fixture"]);
    sandbox.sync();
}

/// 转交给原生 p4 的文件必须在预演里现形，且带**逐文件动作**。
///
/// 这是漏掉 `-Ztag` 那条 bug 的回归用例：只给 `-Mj` 时 p4 回的是给人读的
/// `{"data":"//depot/main/x.dat#1 - opened for edit","level":0}`，没有 `depotFile` /
/// `clientFile` 字段，翻译层对每一行都 `continue`——转交的那批在记录流里凭空消失，
/// 而 summary 仍然说 `ok:true`。消费方只在 `ok:true` 时把记录流当全集，于是会把
/// 「这个文件要开出来」读成「这个文件没有漂移」。
///
/// 顺带钉死两件事：`clientFile` 要翻成 client 语法（原生 reconcile 那条路径回的是
/// **本地路径**，同族的 `opened` / `where` 才是 client 语法），以及硬条款 6——预演与
/// 应用给出的是同一批文件。
#[test]
fn a_handed_off_file_shows_its_own_action_in_the_open_preview() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    commit_unsupported(&sandbox, "w_resource.dat", "resource payload v1\n");
    sandbox.write("readme.txt", "changed locally\n");
    sandbox.write("w_resource.dat", "resource payload v2 changed\n");

    let preview = run_json(&sandbox, &["."]);
    assert!(preview.ok, "{}", preview.stderr);

    let handed_off = records_for(&preview, "//depot/main/w_resource.dat");
    assert_eq!(
        handed_off.len(),
        1,
        "转交的文件必须在预演里现身：{:?}\n{}",
        preview.records,
        preview.stderr
    );
    let record = handed_off[0];
    assert_ne!(
        record["class"], "handoff",
        "预演要报出逐文件动作，不能只说一句「这批转交了」：{record}"
    );
    assert_eq!(record["class"], "edit", "{record}");
    assert_eq!(record["applied"], false, "{record}");
    assert_eq!(
        record["clientFile"],
        format!("//{}/w_resource.dat", sandbox.client()),
        "原生 reconcile 回的是本地路径，必须翻成 client 语法：{record}"
    );
    assert_eq!(
        record["rev"], "1",
        "have 版本要从原生的 workRev 带出来：{record}"
    );
    assert!(sandbox.opened().is_empty(), "预演不许在服务器上动任何东西");

    let apply = run_json(&sandbox, &["-a", "."]);
    assert!(apply.ok, "{}", apply.stderr);
    assert_eq!(
        depot_files(&apply),
        depot_files(&preview),
        "硬条款 6：预演与应用必须是同一批文件\npreview: {:?}\napply: {:?}",
        depot_files(&preview),
        depot_files(&apply)
    );

    // 应用档里这一批只能整批报「交给谁了」——逐文件动作只有预演拿得到（那一版才带
    // `-n` 与 `-Mj`）。`action` 就是 `handoff` 字段的值，`clientFile` 照样要有。
    let handoff = records_for(&apply, "//depot/main/w_resource.dat");
    assert_eq!(handoff.len(), 1, "{:?}", apply.records);
    assert_eq!(handoff[0]["class"], "handoff", "{}", handoff[0]);
    assert_eq!(handoff[0]["handoff"], "reconcile", "{}", handoff[0]);
    assert_eq!(handoff[0]["action"], "reconcile", "{}", handoff[0]);
    assert_eq!(handoff[0]["applied"], true, "{}", handoff[0]);
    assert_eq!(
        handoff[0]["clientFile"],
        format!("//{}/w_resource.dat", sandbox.client()),
        "{}",
        handoff[0]
    );

    // 转交真的落地了：文件在服务器上被开成 edit，也就是预演预告的那件事。
    let opened = sandbox.opened();
    assert!(
        opened.iter().any(|line| line.contains("w_resource.dat")),
        "{opened:?}"
    );
}

/// 三条「只能整批报」的转交路径——clean 的预演、sync 的预演、open 的应用——的记录形状
/// 与 open 预演不同（没有逐文件动作），但同样不许静默丢，且 `clientFile` 不能缺：
/// 消费方拿它去和自己的路径比对，缺了就只能走特判。
#[test]
fn the_batch_only_handoff_paths_still_carry_both_paths() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    commit_unsupported(&sandbox, "w_resource.dat", "resource payload v1\n");
    sandbox.write("w_resource.dat", "resource payload v2 changed\n");

    for (mode, args, command) in [
        ("clean", ["--clean", "."], "clean"),
        ("sync", ["--sync", "."], "sync"),
        ("open", ["-a", "."], "reconcile"),
    ] {
        let run = run_json(&sandbox, &args);

        assert!(run.ok, "{mode}: {}", run.stderr);
        let records = records_for(&run, "//depot/main/w_resource.dat");
        assert_eq!(records.len(), 1, "{mode}: {:?}", run.records);

        let record = records[0];
        assert_eq!(record["mode"], mode, "{record}");
        assert_eq!(record["class"], "handoff", "{record}");
        assert_eq!(record["handoff"], command, "{record}");
        assert_eq!(
            record["action"], command,
            "action 就是 handoff 的值：{record}"
        );
        assert_eq!(
            record["clientFile"],
            format!("//{}/w_resource.dat", sandbox.client()),
            "{record}"
        );
        assert!(record["rev"].is_null(), "整批转交没有逐文件版本：{record}");
    }
}
