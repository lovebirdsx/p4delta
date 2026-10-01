//! 路径形式、changelist、缓存复用，以及 client view 排除行的处理。

mod support;

use predicates::prelude::*;

/// `//depot/...` 形式的参数要先经 `p4 where` 翻译成本地路径，
/// 而且只处理该子树——子树外的改动不该被牵进来。
#[test]
fn a_depot_path_argument_is_translated_to_the_workspace() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("src/lib.txt", "changed under src\n");
    sandbox.write("readme.txt", "changed outside src\n");

    sandbox
        .cli()
        .args(["-a", "-l"])
        .arg("//depot/main/src/...")
        .assert()
        .success()
        .stdout(predicate::str::contains("lib.txt"))
        .stdout(predicate::str::contains("readme.txt").not());

    let opened = sandbox.opened();
    assert_eq!(
        opened.len(),
        1,
        "only src/ should have been touched: {opened:?}"
    );
    assert!(opened[0].contains("lib.txt"), "{opened:?}");
}

/// `-c` 把打开的变更放进指定的 pending changelist，而不是默认那个。
#[test]
fn changes_land_in_the_requested_changelist() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    let changelist = sandbox.new_changelist("e2e changelist");
    sandbox.write("readme.txt", "changed locally\n");

    sandbox
        .cli()
        .args(["-a", "-c", &changelist.to_string()])
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "Using pending changelist {changelist}"
        )));

    let opened = sandbox.opened();
    assert_eq!(
        opened.len(),
        1,
        "only the one file should be open, so nothing leaked into the default changelist: {opened:?}"
    );
    assert!(
        opened[0].contains(&format!("change {changelist}")),
        "the change should sit in changelist {changelist}: {opened:?}"
    );
}

/// 摘要缓存跨次复用：第二次跑不再重算，直接加载上次的结果，结论不变。
///
/// 缓存文件名里带着本实例唯一的 client 名，所以实例之间天然不会串味。
#[test]
fn the_digest_cache_is_reused_across_runs() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    sandbox.write("readme.txt", "changed locally\n");

    // 第一次：没有缓存可加载，摘要得现算。
    let first = sandbox.cli().arg("-l").arg(".").output().expect("run tool");
    assert!(first.status.success(), "{first:?}");
    let first = String::from_utf8_lossy(&first.stdout);
    assert!(!first.contains("Loading cache from"), "{first}");
    assert!(
        first.contains("Hashed "),
        "第一次没有缓存可用，应当真的算了摘要:\n{first}"
    );

    // 第二次：直接加载上次的结果，结论与第一次一致。
    let output = sandbox.cli().arg("-l").arg(".").output().expect("run tool");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("Loading cache from"), "{stdout}");
    // 「加载了缓存」和「缓存真的命中」不是一回事：一个都没命中时打印的是
    // `Loaded 0 cached digests.`，同样含 "cached digests."。`Hashed` 那行只在
    // 确实算过至少一个摘要时才出现（`src/reconcile/mod.rs` 的 `total_size > 0`），
    // 拿它当命中的证据才作数。
    assert!(
        !stdout.contains("Hashed "),
        "缓存命中时不该再算摘要:\n{stdout}"
    );
    assert!(stdout.contains("Editing 1 files"), "{stdout}");

    // Linux 与 macOS 上沙箱用环境变量把缓存圈进了实例目录。
    // Windows 上 `directories` 走 SHGetKnownFolderPath，环境变量改不动它，
    // 只能靠唯一的文件名加退出时清理——那条路径不在这里断言。
    #[cfg(not(windows))]
    {
        let line = stdout
            .lines()
            .find(|line| line.contains("Loading cache from"))
            .expect("a cache load line");
        assert!(
            line.contains(&sandbox.instance_dir().display().to_string()),
            "the cache should live inside the sandbox: {line}"
        );
    }
}

/// client view 的排除行让 p4 完全看不见某些路径。扫盘时它们却长得像新增文件，
/// 不剔掉的话就会去 add 一个 p4 根本不接受的路径。
#[test]
fn unmapped_paths_are_not_reported_as_add() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    let client = sandbox.client().to_owned();
    sandbox.set_client_view(&[
        format!("//depot/main/... //{client}/..."),
        // 排除行：这个子树不映射到工作区。
        format!("-//depot/main/tmp/... //{client}/tmp/..."),
    ]);

    sandbox.write("tmp/scratch.txt", "outside the view\n");
    sandbox.write("readme.txt", "changed locally\n");

    sandbox
        .cli()
        .arg("-l")
        .arg(".")
        .assert()
        .success()
        .stdout(predicate::str::contains("scratch.txt").not())
        .stdout(predicate::str::contains("readme.txt"));
}
