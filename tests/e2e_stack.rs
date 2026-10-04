//! 摘要并行段的栈边界：一批文件的摘要真要现算时，工作线程的栈不能被用爆。
//!
//! 触发条件很窄，但很现实：**冷摘要缓存 + 一批文件的 mtime 被顶出 `have.syncTime ±1s`**
//! （「时间戳变了、内容没变」的批量重算）。这种规模下 rayon 的递归切分在**每一层**都要
//! 在栈上放一份 128 KiB 的读缓冲（`src/digest.rs` 里那个 `[0; READ_BUFFER_SIZE]`，在优化
//! 构建里被内联进了递归帧，实测每层 131,592 字节），文件数一多就把默认的 2 MiB 线程栈压穿，
//! 进程直接 abort。
//!
//! **这两条用例只在 release 档有意义。** dev 档里 `compute_digest_binary` 不进内联，
//! 那 128 KiB 落在叶子帧上，递归帧只剩几百字节，崩不出来——拿 debug 跑一遍是绿的，
//! 会让人误以为守住了。默认档因此用 `.config/nextest.toml` 的 `default-filter` 排掉整个
//! `e2e_stack` 二进制，CI 另有一个 ubuntu 的 release job 用 `--ignore-default-filter` 补跑。
//!
//! 规模也是刻意的：1000 条那条把线程栈压到 1 MiB，让修复前的崩溃**必然**发生而不是
//! 碰运气（2 MiB 默认栈下崩不崩要看偷取时机，实测同一 N 时崩时过）；两万条那条才用
//! 默认栈，量的是真实触发面。

mod support;

use predicates::prelude::*;

/// 每个用例都要先确认摘要真的现算了：这一行是「走到了那条递归」的证据。
fn assert_digests_ran(stdout: &str, count: usize) {
    assert!(
        stdout.contains(&format!("Checking digests for {count} files.")),
        "摘要没跑满 {count} 个文件:\n{stdout}"
    );
}

/// 1000 个文件 + 1 MiB 线程栈。修复前必崩。
///
/// 把栈压到 1 MiB 不是把测试调松，而是把边界挪到必然触发的位置：默认 2 MiB 下
/// 一千条文件位于「有时崩有时不崩」的区间，那种用例当回归测试没有意义。
#[test]
fn digests_of_a_thousand_files_fit_a_small_thread_stack() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    const FILES: usize = 1000;
    sandbox.bulk_text_files(FILES);

    let assert = sandbox
        .cli()
        .env("RUST_MIN_STACK", "1048576")
        .args(["-l"])
        .arg("bulk")
        .assert()
        .success()
        .stderr(predicate::str::contains("stack overflow").not());

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_digests_ran(&stdout, FILES);
    // 内容一个字节没变，摘要就该与 have 里的完全相同；只要有一条变更行，
    // 要么是摘要算错了，要么是这批文件根本没算（两条都该红）。
    assert_eq!(
        support::listed_changes(&stdout),
        Vec::new(),
        "内容没变，不该有任何变更：{stdout}"
    );
}

/// 两万个文件 + **默认**线程栈：真实触发面（下游集成就是在这个量级上崩的）。
#[test]
fn twenty_thousand_files_fit_the_default_stack() {
    let Some(sandbox) = support::sandbox_or_skip() else {
        return;
    };

    const FILES: usize = 20000;
    sandbox.bulk_text_files(FILES);

    let assert = sandbox
        .cli()
        .args(["-l"])
        .arg("bulk")
        .assert()
        .success()
        .stderr(predicate::str::contains("stack overflow").not());

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_digests_ran(&stdout, FILES);
    assert_eq!(
        support::listed_changes(&stdout),
        Vec::new(),
        "内容没变，不该有任何变更：{stdout}"
    );
}
