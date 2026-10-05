//! 操作范围：多入口合并成一轮、文件入口、排除，以及 `.p4delta-scope` 与位置参数
//! 的组合（硬上限 + 交集）。
//!
//! 只验**解析与报错**的那几条不在这里——它们在 `evaluate_scope` 里就 bail 了，
//! 一个 p4 进程都不需要，所以搬去了 `tests/cli.rs`（不必为它们各起一个 p4d 沙箱）。
//! 留在这里的都要真实文件树或 depot 数据才能得出结论。

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

    // 配置里只有排除项：include 默认取配置文件所在目录，也就是工作区根。
    sandbox.write(".p4delta-scope", "-src/deep\n");
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

    sandbox.write(".p4delta-scope", "src\n");
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

    sandbox.write(".p4delta-scope", "src\n");
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

    sandbox.write(".p4delta-scope", "src\n");
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

/// 配置里的相对路径以配置文件所在目录为基准，而不是跑命令时的 cwd。
#[test]
fn a_nested_scope_file_uses_its_own_directory_as_the_base() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("src/.p4delta-scope", "deep\n");
    sandbox.write("src/deep/a/b/c.txt", "changed deep\n");
    sandbox.write("src/lib.txt", "changed\n");

    // cwd 是工作区根：配置里的 `deep` 必须相对 src 解析，否则会落到不存在的
    // <root>/deep，范围整个空掉。
    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg("src")
        .assert()
        .success()
        .stdout(predicate::str::contains("c.txt"));

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].contains("c.txt"), "{opened:?}");
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

    // 配置里只有排除项，include 于是落在配置文件所在目录（工作区根）——正好把这份被
    // 提交的配置文件包进了查询范围。
    sandbox.commit(".p4delta-scope", "-src/deep\n");
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

/// 入口的父目录本地整个没了（目录连着文件一起被删）时，cwd 退到最近的现存祖先：
/// 拿一个不存在的目录当 p4 子进程的 cwd，连查询都起不来。
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

/// 排除项也能从命令行来：同一个参数里用 `;` 连接。
#[test]
fn a_semicolon_joined_argument_carries_exclusions() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("src/lib.txt", "changed\n");
    sandbox.write("src/deep/a/b/c.txt", "changed deep\n");

    let output = sandbox
        .cli()
        .args(["-a", "-l"])
        .arg(".;-src/deep")
        .output()
        .expect("run tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("outside the scope untouched"),
        "被排除的 depot 记录应当被报告:\n{stdout}"
    );
    assert!(
        !stdout.contains("c.txt"),
        "排除目录里的改动不该出现:\n{stdout}"
    );

    let opened = sandbox.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(opened[0].contains("lib.txt"), "{opened:?}");
}

/// 同一个排除项写成 `--` 之后的位置参数同样成立；而不加 `--` 时它以 `-` 开头，被当成
/// 选项拒掉——这是刻意的：`-c` / `-v` / `-w` 都是真实存在的短选项，不能一律当路径收。
#[test]
fn exclusions_after_a_double_dash_work_too() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("src/lib.txt", "changed\n");
    sandbox.write("src/deep/a/b/c.txt", "changed deep\n");

    sandbox
        .cli()
        .args(["-a", "-l", ".", "-src/deep"])
        .assert()
        .failure();

    sandbox
        .cli()
        .args(["-a", "-l", "--", ".", "-src/deep"])
        .assert()
        .success()
        .stdout(predicate::str::contains("c.txt").not());

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

    sandbox.write(".p4delta-scope", "src\n");
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

    sandbox.write(".p4delta-scope", "-src/deep\n");
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

    sandbox.write(".p4delta-scope", "-src/deep\n");
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
