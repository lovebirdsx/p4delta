//! 操作范围：多入口合并成一轮、文件入口、排除，以及 client root 下的 `.p4delta-scope`
//! 与位置参数/`--exclude-*` 的组合（硬上限 + 交集）。
//!
//! 只验**解析与报错**的那几条不在这里——它们在求值阶段就 bail 了，一个 p4 进程都不需要，
//! 所以搬去了 `tests/cli.rs`（不必为它们各起一个 p4d 沙箱）。留在这里的都要真实文件树或
//! depot 数据才能得出结论。
//!
//! 范围归属是**固定**的：配置只从 client root 读，相对路径以它（或命令行那侧的启动 cwd）
//! 为基准。所以这里没有「换个目录就换一份配置」的用例，只有「从哪儿调用都还是那一份」。

mod support;

use predicates::prelude::*;

use support::listed_changes;

/// 多个入口合并成一轮：一次运行就是一个视图，两边各自报告一次。
#[test]
fn several_entries_are_handled_in_a_single_pass() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("readme.txt", "changed at the root\n");
    sandbox.write("src/lib.txt", "changed under src\n");

    let output = sandbox
        .cli()
        .args(["-a", "-l"])
        .args(["readme.txt", "src"])
        .output()
        .expect("run tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("Processing 2 scope entries."), "{stdout}");
    assert_eq!(listed_changes(&stdout).len(), 2, "{stdout}");

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 2, "{opened:?}");
}

/// 文件入口只处理那一个文件，同目录的兄弟改动不被牵进来。
///
/// 这里刻意不退而求其次用它的父目录：父目录会把工作范围悄悄扩大到用户没点过的东西上，
/// 而 `--sync` 会覆盖本地改动、`--clean` 会删文件。
#[test]
fn a_single_file_entry_touches_only_that_file() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("src/lib.txt", "changed\n");
    sandbox.write("src/deep/a/b/c.txt", "also changed\n");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg("src/lib.txt")
        .assert()
        .success()
        .stdout(predicate::str::contains("Editing 1 files"))
        .stdout(predicate::str::contains("c.txt").not());

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].contains("lib.txt"), "{opened:?}");
}

/// 本地已删除的文件可以直接当入口：depot 记录还在，应当被开成删除。
/// 路径解析走 view 映射而不读本地文件系统，缺了本地文件照样查得到。
#[test]
fn a_file_entry_handles_a_locally_deleted_file() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.remove("src/lib.txt");
    assert!(!sandbox.exists("src/lib.txt"));

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg("src/lib.txt")
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleting 1 files"));

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].contains("lib.txt"), "{opened:?}");
    assert!(opened[0].contains(" - delete "), "{opened:?}");
}

/// 排除目录里的已跟踪文件必须两侧同时过滤：depot 记录里剔掉、扫盘时跳过。
/// 只做一侧的话，那个文件会因为「本地扫不到」被误判成待删除。
#[test]
fn excluded_directories_are_left_untouched() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 配置里只有排除项：include 省略即整个 client root。
    sandbox.write(".p4delta-scope", r#"{"exclude": [{"dir": "src/deep"}]}"#);
    sandbox.write("src/lib.txt", "changed\n");

    let output = sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(".")
        .output()
        .expect("run tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("outside the scope untouched"),
        "排除的 depot 记录应当被明确报告:\n{stdout}"
    );
    assert!(
        !stdout.contains("Deleting") && !stdout.contains("c.txt"),
        "排除目录里的文件不该出现在任何改动里:\n{stdout}"
    );

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].contains("lib.txt"), "{opened:?}");
    assert!(opened[0].contains(" - edit "), "{opened:?}");
}

/// 配置的范围是硬上限：传入工作区根时，根上的改动仍被配置挡在外面。
#[test]
fn the_scope_file_caps_the_given_paths() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write(".p4delta-scope", r#"{"include": [{"dir": "src"}]}"#);
    sandbox.write("readme.txt", "changed at the root\n");
    sandbox.write("src/lib.txt", "changed under src\n");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("lib.txt"))
        .stdout(predicate::str::contains("readme.txt").not());

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].contains("lib.txt"), "{opened:?}");
}

/// 重叠的入口先归并再查询：不去重的话 fstat 会返回重复记录，同一个文件被打开两次。
#[test]
fn overlapping_entries_do_not_duplicate_commands() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("src/lib.txt", "changed\n");

    let output = sandbox
        .cli()
        .args(["-a", "-l"])
        .args([".", "src", "src/lib.txt"])
        .output()
        .expect("run tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("Processing 1 scope entry."),
        "三个入口应当归并成一个:\n{stdout}"
    );
    assert_eq!(listed_changes(&stdout).len(), 1, "{stdout}");

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
}

/// 配置文件自身不是工作区内容，不被当成待新增的文件。
#[test]
fn the_scope_file_is_not_reported_as_add() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write(".p4delta-scope", r#"{"include": [{"dir": "src"}]}"#);
    sandbox.write("src/fresh.txt", "brand new\n");

    let output = sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(".")
        .output()
        .expect("run tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    let changes = listed_changes(&stdout);
    assert_eq!(changes.len(), 1, "只有 fresh.txt 是改动:\n{changes:?}");
    assert!(changes[0].1.contains("fresh.txt"), "{changes:?}");

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].contains("fresh.txt"), "{opened:?}");
}

/// 不给任何路径时用配置的范围——这是「只用配置」的入口。
#[test]
fn the_scope_file_alone_drives_a_run() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write(".p4delta-scope", r#"{"include": [{"dir": "src"}]}"#);
    sandbox.write("readme.txt", "changed at the root\n");
    sandbox.write("src/lib.txt", "changed under src\n");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Using scope file"))
        .stdout(predicate::str::contains("lib.txt"))
        .stdout(predicate::str::contains("readme.txt").not());
}

/// 工作区里第二份 `.p4delta-scope` 不是第二份配置：归属固定在 client root，往上往下都不找。
///
/// 它甚至不必是合法 JSON——被读到的机会本来就没有。这条同时钉住了「打开子目录不换配置」：
/// 从 `src` 里调用、目标也写 `src` 时，生效的仍是根上那一份。
#[test]
fn a_nested_scope_file_is_not_a_second_config() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write(".p4delta-scope", r#"{"include": [{"dir": "src/deep"}]}"#);
    sandbox.write(
        "src/.p4delta-scope",
        "这不是 JSON，根上的配置才是生效的那一份\n",
    );
    sandbox.write("src/deep/a/b/c.txt", "changed deep\n");
    sandbox.write("src/lib.txt", "changed\n");

    // cwd 也在 src 里：换目录换不来另一份配置。
    sandbox
        .cli()
        .current_dir(sandbox.client_root().join("src"))
        .args(["-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("Using scope file"))
        .stdout(predicate::str::contains("lib.txt").not());

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].contains("c.txt"), "{opened:?}");
}

/// 配置读不动时**什么都不做**：空文件、非法 JSON、错误类型都是错误而不是「没有配置」。
///
/// 「读不动就当没有」会把范围悄悄放大成整个 client root，而调用方以为限制还在——
/// 在 `--clean -a` 下那是删文件的方向，所以这条要连着「什么都没被打开」一起断言。
#[test]
fn a_broken_config_stops_the_run_before_anything_happens() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("src/lib.txt", "changed\n");

    for broken in [
        "",
        "src\n",
        "{\"include\": []}",
        "{\"include\": [{\"dir\": \"../x\"}]}",
    ] {
        sandbox.write(".p4delta-scope", broken);

        sandbox
            .cli()
            .args(["-a", "-l"])
            .arg(".")
            .assert()
            .failure()
            .stderr(predicate::str::contains(".p4delta-scope"));

        let opened = sandbox.opened();
        assert!(opened.is_empty(), "配置坏了就不该打开任何文件: {opened:?}");
    }
}

/// 入口在 depot 与本地都找不到东西时提示一句（多半是拼错了），但不影响其他入口。
#[test]
fn an_unmatched_entry_is_reported_without_stopping_the_run() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("src/lib.txt", "changed\n");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .args(["src", "no-such-folder"])
        .assert()
        .success()
        .stderr(predicate::str::contains("no-such-folder"))
        .stdout(predicate::str::contains("lib.txt"));

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
}

/// 反过来：**一条**都没匹配上就是「什么都没做」，不能报成功——用户会把 exit 0 当成
/// 「处理完了」，而真相多半是路径拼错了。
#[test]
fn entries_that_all_match_nothing_are_an_error() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("src/lib.txt", "changed\n");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg("no-such-folder")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no-such-folder"));

    let opened = sandbox.opened();
    assert!(opened.is_empty(), "什么都不该打开: {opened:?}");
}

/// 被提交进 depot 的范围配置文件不能被当成「本地已删除」。
///
/// 这是 `-a` 下的毁数据场景：只在扫盘侧按文件名跳过它、depot 侧照收，它就成了待删除，
/// `p4 delete` 会把团队共享的范围配置删掉。两侧必须同时排除。
#[test]
fn a_committed_scope_file_is_never_reported_as_deleted() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 配置里只有排除项，include 省略即整个 client root——正好把这份被提交的配置文件
    // 包进了查询范围。
    sandbox.commit(".p4delta-scope", r#"{"exclude": [{"dir": "src/deep"}]}"#);
    sandbox.write("src/lib.txt", "changed\n");

    let output = sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(".")
        .output()
        .expect("run tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    let changes = listed_changes(&stdout);
    assert_eq!(changes.len(), 1, "只有 lib.txt 是改动:\n{changes:?}");
    assert!(
        !stdout.contains("Deleting"),
        "配置文件不该被报成删除:\n{stdout}"
    );

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].contains("lib.txt"), "{opened:?}");
    assert!(opened[0].contains(" - edit "), "{opened:?}");
}

/// 入口的父目录本地整个没了（目录连着文件一起被删）时照样能处理：路径解析走 view 映射
/// 而不读本地文件系统，类型判定也容许「本地不存在」。
///
/// p4 子进程的 cwd 现在是 client root，不再跟着入口走——一个不存在的入口目录连查询都起不来，
/// 这条正是那种形状下仍要拿到正确结论的用例。
#[test]
fn an_entry_under_a_vanished_directory_falls_back_to_an_existing_cwd() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.commit("src/deep/gone.txt", "gone soon\n");
    std::fs::remove_dir_all(sandbox.client_root().join("src")).expect("remove src");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg("src/deep/gone.txt")
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleting 1 files"));

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].contains("gone.txt"), "{opened:?}");
    assert!(opened[0].contains(" - delete "), "{opened:?}");
}

/// 排除项从命令行来：`--exclude-dir`，相对路径以 client root 为基准，可重复。
#[test]
fn exclude_dir_carries_a_directory_out_of_the_scope() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("src/lib.txt", "changed\n");
    sandbox.write("src/deep/a/b/c.txt", "changed deep\n");
    sandbox.write("src/other/d.txt", "changed elsewhere\n");

    let output = sandbox
        .cli()
        .args([
            "-a",
            "-l",
            "--exclude-dir",
            "src/deep",
            "--exclude-dir",
            "src/other",
        ])
        .arg(".")
        .output()
        .expect("run tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("outside the scope untouched"),
        "被排除的 depot 记录应当被报告:\n{stdout}"
    );
    for excluded in ["c.txt", "d.txt"] {
        assert!(
            !stdout.contains(excluded),
            "排除目录里的改动不该出现（{excluded}）:\n{stdout}"
        );
    }

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].contains("lib.txt"), "{opened:?}");
}

/// `--exclude-file` 只排那一个文件，同目录的兄弟照常处理。
#[test]
fn exclude_file_carries_a_single_file_out_of_the_scope() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("src/lib.txt", "changed\n");
    sandbox.write("src/deep/a/b/c.txt", "changed deep\n");

    sandbox
        .cli()
        .args(["-a", "-l", "--exclude-file", "src/deep/a/b/c.txt"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("c.txt").not())
        .stdout(predicate::str::contains("lib.txt"));

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].contains("lib.txt"), "{opened:?}");
}

/// 命令行给的 depot 路径翻译不到本地（不在 client view 里）时，不能悄悄退回配置范围：
/// 那等于把操作放大到用户没点过的东西上。全部译不出来就报错退出。
#[test]
fn a_depot_path_outside_the_client_view_is_an_error() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write(".p4delta-scope", r#"{"include": [{"dir": "src"}]}"#);
    sandbox.write("src/lib.txt", "changed\n");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg("//no-such-depot/main/...")
        .assert()
        .failure()
        .stderr(predicate::str::contains("in this client's view"))
        .stdout(predicate::str::contains("lib.txt").not());

    let opened = sandbox.opened();
    assert!(opened.is_empty(), "什么都不该打开: {opened:?}");
}

/// `--sync --force` 吃同一份排除名单：排除目录里的本地改动不传（也就不会被 depot 版本覆盖）。
///
/// 这里的正对照是「本地改动进同步清单」，那是强制修复的分类学；普通同步的清单由原生打算
/// 传哪些文件决定，同样的排除边界另见 `e2e_sync_normal.rs::an_excluded_directory_is_never_written_to`。
#[test]
fn sync_leaves_excluded_directories_alone() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write(".p4delta-scope", r#"{"exclude": [{"dir": "src/deep"}]}"#);
    sandbox.write("src/deep/a/b/c.txt", "locally changed deep\n");
    // 正对照：排除范围外的改动照常进同步清单。
    sandbox.write("src/lib.txt", "locally changed\n");

    sandbox
        .cli()
        .args(["--sync", "--force", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("lib.txt"))
        .stdout(predicate::str::contains("c.txt").not());

    assert_eq!(
        sandbox.read("src/deep/a/b/c.txt"),
        "locally changed deep\n",
        "排除目录里的本地内容不该被动过"
    );
}

/// `--clean -a` 下排除目录里的未跟踪文件不能被删——范围限定要挡的正是这类破坏。
/// 生效的配置文件自身同理：它是本地新增、depot 里没有，clean 正好会把它当垃圾清掉。
#[test]
fn clean_never_deletes_inside_excluded_directories() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write(".p4delta-scope", r#"{"exclude": [{"dir": "src/deep"}]}"#);
    sandbox.write("src/deep/scratch.txt", "untracked\n");
    sandbox.write("src/scratch.txt", "untracked\n");

    sandbox
        .cli()
        .args(["--clean", "-a", "-l"])
        .arg(".")
        .assert()
        .success();

    assert!(
        sandbox.exists("src/deep/scratch.txt"),
        "排除目录里的未跟踪文件不该被删"
    );
    assert!(
        !sandbox.exists("src/scratch.txt"),
        "排除目录外的未跟踪文件照常被清掉"
    );
    assert!(
        sandbox.exists(".p4delta-scope"),
        "配置文件不该被 clean 清掉"
    );
}

/// 不给路径、也没有配置时是错误，不是「整个 client root」。
///
/// 不给范围就把整棵 client 过一遍是这套契约里最不该有的默认值——`--clean -a` 下它是
/// 破坏性的。这条要真实连接才谈得上（根得先问出来），所以从 `tests/cli.rs` 搬了过来。
#[test]
fn a_run_without_a_path_is_an_error() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("src/lib.txt", "changed\n");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("No path given"));

    let opened = sandbox.opened();
    assert!(opened.is_empty(), "什么都不该打开: {opened:?}");
}

/// 传入的路径与配置范围完全不相交时什么都没得做，报错退出而不是静默成功。
#[test]
fn a_scope_that_does_not_overlap_the_given_paths_is_a_failure() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write(".p4delta-scope", r#"{"include": [{"dir": "src"}]}"#);
    sandbox.write("src/lib.txt", "changed\n");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg("readme.txt")
        .assert()
        .failure()
        .stderr(predicate::str::contains("do not overlap"))
        // 报错里要带上生效的那份配置与两侧的清单，否则这条消息看着自相矛盾。
        .stderr(predicate::str::contains(".p4delta-scope"))
        .stderr(predicate::str::contains("include"));

    let opened = sandbox.opened();
    assert!(opened.is_empty(), "什么都不该打开: {opened:?}");
}

/// 交集空掉的另一种成因是排除项把入口自己排掉了：报错里要把排除项列出来。
///
/// 不列的话这条消息看着自相矛盾——`scope: src` 配 `given: src/deep` 明明相交。
#[test]
fn a_scope_entry_that_exclusions_swallow_reports_the_exclusions() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write(
        ".p4delta-scope",
        r#"{"include": [{"dir": "src"}], "exclude": [{"dir": "src/deep"}]}"#,
    );

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg("src/deep")
        .assert()
        .failure()
        // 分隔符两个平台都认：报错里是本地路径的原样，Windows 上全是反斜杠。
        .stderr(predicate::str::is_match(r#"excluded: dir ".*src[\\/]deep"#).unwrap());
}

// ---- 从已删除的快照/请求协议里迁移过来的安全性质 ----
//
// 协议本身（`--scope-report` / `--scope-request` / `--scope-snapshot`）已经删掉，机器可读的
// 范围不再经手 δ；下面这些断言与协议无关，它们钉的是**范围求值本身**的正确性，所以换了
// 命令行这一侧的输入继续验。被排除子树的剪枝证据尤其重要：`--clean -a` 下它是删文件的边界。

/// open 模式也把入口里的 Perforce 元字符转义后再交给 p4。
///
/// 这里曾经是「拒绝带元字符的入口」——因为那一档把入口路径原样当 file spec 交出去。
/// 本地目录名里的 `#`/`@`/`%` 在 Windows 上完全合法，正确做法是转义一次，而不是把路径拒掉。
#[test]
fn open_mode_escapes_perforce_metacharacters_in_an_entry() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    const DIR: &str = "odd#1@2%3";
    const LOCAL: &str = "odd#1@2%3/tracked.txt";
    // `p4 add` 收客户端路径、不做转义解码，所以入库用原名；fstat / 规格那一侧要转义形态。
    const DEPOT: &str = "//depot/main/odd%231%402%253/tracked.txt";

    sandbox.write(LOCAL, "first\n");
    sandbox.p4_ok(&["add", "-f", LOCAL]);
    sandbox.p4_ok(&["submit", "-d", "first"]);
    // 让实例自己 sync 下来：`p4 submit` 把 have 的 syncTime 记成提交那一刻**已经被回拨过的**
    // mtime，紧接着再改文件正好落回 ±1 秒的窗口里，改动会被快筛静默跳过。sync 把 syncTime
    // 记成同步时刻，之后再回拨才拨得出去（同 `Sandbox::bulk_text_files` 的做法）。
    sandbox.p4_ok(&["sync", "-f", DEPOT]);
    sandbox.write(LOCAL, "changed\n");

    let output = sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(DIR)
        .output()
        .expect("run tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    let changes = listed_changes(&stdout);
    assert_eq!(changes.len(), 1, "{changes:?}\n{stdout}");
    assert!(changes[0].1.contains("tracked.txt"), "{changes:?}");

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].contains("tracked.txt"), "{opened:?}");
    assert!(opened[0].contains(" - edit "), "{opened:?}");

    let after = sandbox.p4_ok(&["fstat", "-T", "haveRev", DEPOT]);
    assert!(after.contains("haveRev 1"), "open 不该动 have：{after}");
}

/// `--exclude-*` 的名字按**字面**处理：分号不拆、前导 `-` 不当排除前缀、类型以参数为准。
///
/// 这三条正是「不把机器生成的路径拼回文本语法」的理由。分号拆开会把一条路径变成两条
/// （第二条可能不存在，于是整轮报错或者范围悄悄变了）；前导 `-` 被剥掉会指到另一个目录。
/// 另一条排除指向本地**不存在**的目录：不去 stat 猜类型，声明成目录就是目录。
#[test]
fn typed_exclusions_keep_literal_names_and_declared_kinds() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    const DIR: &str = "-排除; 目录 中文";
    let excluded = format!("src/{DIR}");

    sandbox.write(&format!("{excluded}/noise.txt"), "brand new\n");
    sandbox.write("src/lib.txt", "changed\n");

    sandbox
        .cli()
        .args([
            "-a",
            "-l",
            "--exclude-dir",
            &excluded,
            "--exclude-dir",
            "src/ghost",
        ])
        .arg("src")
        .assert()
        .success()
        .stdout(predicate::str::contains("lib.txt"))
        // 被排除的目录里那份未跟踪文件一旦漏出去就会以 Add 出现在清单里。
        .stdout(predicate::str::contains("noise.txt").not())
        .stdout(predicate::str::contains("Deleting").not());

    assert!(sandbox.exists(&format!("{excluded}/noise.txt")));

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].contains("lib.txt"), "{opened:?}");
}

/// 中文与空格的路径按字面送到 p4：空格不是分隔符，也不是要 trim 的空白。
#[test]
fn unicode_and_space_paths_reach_p4() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    const DIR: &str = "资产 说明";
    const FILE: &str = "资产 说明/待处理 文件.txt";

    // 入库与 `sync` 都不能省：非 ASCII 路径要走 stdin（挂在命令行上会被 ANSI 代码页吃成
    // `????`），而 submit 会把当时那份回拨过的 mtime 记成 have 的 syncTime，紧接着再回拨
    // 一次正好落进 ±1 秒的捷径窗口，改动会被静默当成「没变」。
    sandbox.write(FILE, "first\n");
    sandbox.p4_ok_paths(&["add"], &[FILE]);
    sandbox.p4_ok(&["submit", "-d", "unicode fixture"]);
    sandbox.sync();
    sandbox.write(FILE, "changed\n");

    let output = sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(DIR)
        .output()
        .expect("run tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    let changes = listed_changes(&stdout);
    assert_eq!(changes.len(), 1, "{changes:?}\n{stdout}");
    assert!(changes[0].1.contains("待处理 文件.txt"), "{changes:?}");

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].contains(" - edit "), "{opened:?}");
}

/// 让某个文件在被**读取内容**时失败，句柄/权限活到返回值被丢弃为止。
///
/// Windows 用共享模式（不给 `FILE_SHARE_READ`），Unix 用 0000 权限。两者都只挡内容读取：
/// `stat` 照常成功，所以「收进清单」与「真把它读出来算摘要」被分得开——剪枝用例要的正是
/// 后者的反证。
#[cfg(windows)]
struct ReadDenied {
    _held: std::fs::File,
}

#[cfg(not(windows))]
struct ReadDenied {
    path: std::path::PathBuf,
}

/// 见 [`ReadDenied`]。
#[cfg(windows)]
fn deny_reads(path: &std::path::Path) -> ReadDenied {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    ReadDenied {
        _held: std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_WRITE)
            .open(path)
            .expect("hold the file open against readers"),
    }
}

/// 见 [`ReadDenied`]。
#[cfg(not(windows))]
fn deny_reads(path: &std::path::Path) -> ReadDenied {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000))
        .expect("drop the read bit");
    ReadDenied {
        path: path.to_path_buf(),
    }
}

/// 还回读权限：实例目录的清理与用例自己的断言都还指望碰这个文件。
#[cfg(not(windows))]
impl Drop for ReadDenied {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;

        let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
    }
}

/// 摘要缓存的字节里有没有这个文件的路径键。
///
/// 缓存是**被测程序自己落盘的产物**，键就是扫描时用的那个路径键（小写、统一分隔符），
/// bincode 把字符串原样写进去，所以子串命中即「这个文件被读过内容算过摘要」。
fn cache_holds(bytes: &[u8], path: &std::path::Path) -> bool {
    let key = path
        .display()
        .to_string()
        .replace('/', std::path::MAIN_SEPARATOR_STR)
        .to_ascii_lowercase();
    bytes
        .windows(key.len())
        .any(|window| window == key.as_bytes())
}

/// 被排除的子树在**扫描/摘要之前**就被剪掉，不只是「结果里不出现它」。
///
/// 先跑一次**不带排除**的对照：被排除的那两份文件确实会进清单、也确实是摘要候选（计数与
/// 缓存里都有它们）——证据因此有判别力，而不是「它们本来就不算候选」。同一批文件上再加
/// 排除，摘要计数、缓存内容与被摁住不许读的那份文件一起证明它们从未被读过。
///
/// 被排除的那份用**模板里就入库的**文件（`src/deep/a/b/c.txt`），不另外提交一份：提交下来
/// 的 have 把 syncTime 记成提交那一刻的文件 mtime，而沙箱回拨 mtime 是相对 now 的——两次
/// 回拨之间只差毫秒，正好落回 ±1 秒的「自 sync 起未改动」窗口，那份文件会被快筛跳过，
/// 压根不是摘要候选，对照也就失去判别力。
#[test]
fn the_excluded_subtree_is_pruned_before_the_scan() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    // 被排除目录里放一份已跟踪又改过的文件与一份新文件：剪枝失效时它们一定会进清单，
    // 而且改过的那份必然要被读出来算摘要。
    sandbox.write("src/deep/a/b/c.txt", "changed but excluded\n");
    sandbox.write("src/deep/added.txt", "brand new\n");
    sandbox.write("src/lib.txt", "changed in scope\n");

    let excluded = sandbox.client_root().join("src/deep/a/b/c.txt");

    // 对照：没有排除时这一批就是「两份摘要候选 + 一份新增」。
    let control = sandbox
        .cli()
        .args(["-l"])
        .arg("src")
        .output()
        .expect("run tool");
    assert!(control.status.success(), "{control:?}");
    let control_stdout = String::from_utf8_lossy(&control.stdout);

    let mut control_changes = listed_changes(&control_stdout);
    control_changes.sort();
    assert_eq!(
        control_changes.len(),
        3,
        "{control_changes:?}\n{control_stdout}"
    );
    // 进度与计数走 stdout：人类可读的输出只在 `--json` 下改道 stderr。
    assert!(
        control_stdout.contains("Checking digests for 2 files."),
        "对照里两份都该是摘要候选：{control_stdout}"
    );

    if let Some(cache) = sandbox.cache_path() {
        let bytes = std::fs::read(&cache).expect("对照那轮算过摘要，缓存该已经写出来");
        assert!(
            cache_holds(&bytes, &sandbox.client_root().join("src/lib.txt")),
            "范围内那份在对照里就该被算过摘要"
        );
        assert!(
            cache_holds(&bytes, &excluded),
            "对照里被排除的那份也该被算过摘要——否则这条证据证明不了任何事"
        );
        // 证据那轮从冷缓存开始：否则「缓存里没有它」可能只是上一轮留下的，而且热缓存会让
        // 下面那道「摁住不许读」失效（摘要直接取自缓存，根本不碰文件）。
        std::fs::remove_file(&cache).expect("清掉对照那轮的缓存");
    }

    // 摁住被排除的那一份：内容读不到（stat 照常），真去哈希就会当场失败。
    let _denied = deny_reads(&excluded);

    let evidence = sandbox
        .cli()
        .args(["-l", "--exclude-dir", "src/deep"])
        .arg("src")
        .output()
        .expect("run tool");
    let evidence_stdout = String::from_utf8_lossy(&evidence.stdout);
    let evidence_stderr = String::from_utf8_lossy(&evidence.stderr);
    assert!(
        evidence.status.success(),
        "被排除的子树真被剪掉的话，这条路径根本不该被打开：{evidence_stderr}"
    );
    assert_eq!(
        listed_changes(&evidence_stdout).len(),
        1,
        "{evidence_stdout}"
    );
    assert!(
        evidence_stdout.contains("lib.txt") && !evidence_stdout.contains("c.txt"),
        "{evidence_stdout}"
    );
    assert!(
        evidence_stdout.contains("Checking digests for 1 files."),
        "范围里只有一份需要算摘要，被排除的那份连候选都不是：{evidence_stdout}\n{evidence_stderr}"
    );

    if let Some(cache) = sandbox.cache_path() {
        let bytes = std::fs::read(&cache).expect("这一轮算过摘要，缓存该已经写出来");
        assert!(
            cache_holds(&bytes, &sandbox.client_root().join("src/lib.txt")),
            "范围内那份必须被算过摘要"
        );
        assert!(
            !cache_holds(&bytes, &excluded),
            "被排除的那份不该进摘要缓存"
        );
    }
}

// ---- 多根布局（AltRoots）：固定归属的另一半 ----

/// 装一个「同一个 client 两个根」的布局，两侧各放一份**互相矛盾**的 `.p4delta-scope`。
///
/// 固定 Root 下的合法、AltRoot 下的坏(非 JSON)：谁被读了一看便知——读错那一份会以
/// `Invalid` 失败，而不是这里要的 AltRoot 报错。返回 AltRoot 目录。
fn alt_root_layout(sandbox: &support::Sandbox) -> std::path::PathBuf {
    let alt = sandbox.alt_root("alt");
    std::fs::create_dir_all(&alt).expect("create the alt root");
    sandbox.set_client_alt_roots(&[&alt]);

    sandbox.write(".p4delta-scope", r#"{"include": [{"dir": "src"}]}"#);
    std::fs::write(alt.join(".p4delta-scope"), "这不是 JSON\n").expect("write the alt scope");
    alt
}

/// cwd 停在 AltRoot 上时**失败关闭**，且 AltRoot 那份配置绝不会被读到。
///
/// `p4 info` 的 `clientRoot` 随 cwd 变：从 AltRoot 里发起时它报的就是 AltRoot。曾经
/// `(None, Some(reported))` 直接采信它，于是「同一个 client 的另一个根」被当成本轮的根、
/// 范围配置也换成那棵树上的另一份。现在唯一的权威是 client spec 里**固定的** `Root`，
/// `p4 info` 只用来核对「这次调用不是从 AltRoot 里发起的」。
#[test]
fn a_directory_on_an_alt_root_is_refused() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    let alt = alt_root_layout(&sandbox);
    sandbox.write("src/lib.txt", "changed\n");

    // 不带 --client-root：p4 info 报 AltRoot，直接拒绝。
    let run = sandbox
        .cli_in(&alt)
        .args(["-a", "-l", "src"])
        .output()
        .expect("run tool");
    assert!(
        !run.status.success(),
        "cwd 落在 AltRoot 上必须是失败关闭：{:?}",
        String::from_utf8_lossy(&run.stdout)
    );
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("AltRoot"), "报错要点名 AltRoot：{stderr}");
    assert!(
        stderr.contains(&alt.display().to_string()),
        "报错要带上那个目录：{stderr}"
    );
    assert!(
        !stderr.contains("Invalid") && !stderr.contains(".p4delta-scope"),
        "AltRoot 上的配置不该被读：{stderr}"
    );

    // --client-root 指到 AltRoot 上：同样是拒绝，而不是「换一份范围继续跑」。
    let run = sandbox
        .cli_in(&alt)
        .args(["-a", "-l", "--client-root"])
        .arg(&alt)
        .arg("src")
        .output()
        .expect("run tool");
    assert!(
        !run.status.success(),
        "--client-root 指到 AltRoot 上必须是失败"
    );
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("AltRoot"), "报错要点名 AltRoot：{stderr}");
    assert!(
        !stderr.contains("Invalid") && !stderr.contains(".p4delta-scope"),
        "AltRoot 上的配置不该被读：{stderr}"
    );

    assert!(
        sandbox.opened().is_empty(),
        "失败关闭意味着一个文件都不该被打开"
    );
}

/// 固定 Root 才是本轮的根：从它下面发起时，挂着 AltRoots 也读它那一份配置。
///
/// 两条入口一起钉：不传 `--client-root`（走 spec 的固定 Root）与传一个与固定 Root 相符的
/// `--client-root`，都必须落在同一份范围上——后者还刻意从 AltRoot 的目录里发起，
/// 位置参数用绝对路径指回固定 Root 那棵树。
#[test]
fn the_fixed_root_drives_the_run_from_any_directory() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    let alt = alt_root_layout(&sandbox);
    sandbox.write("src/lib.txt", "changed\n");
    sandbox.write("readme.txt", "changed at the root\n");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("readme.txt").not())
        .stdout(predicate::str::contains("lib.txt"));

    // 从 AltRoot 里发起，但明确给出固定 Root（绝对路径的入口指回那棵树）：
    // 范围仍是固定 Root 那一份，而不是 AltRoot 上的坏配置。
    let root = sandbox.client_root().to_path_buf();
    let run = sandbox
        .cli_in(&alt)
        .args(["-a", "-l", "--client-root"])
        .arg(&root)
        .arg(root.join("src"))
        .output()
        .expect("run tool");
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        run.status.success(),
        "固定 Root 明确给出时应当照常运行：{stderr}"
    );
    assert!(
        !stderr.contains("Invalid") && !stderr.contains(".p4delta-scope"),
        "AltRoot 上的配置不该被读：{stderr}"
    );

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "只有 src/lib.txt 该被打开：{opened:?}");
    assert!(opened[0].contains("lib.txt"), "{opened:?}");
}
