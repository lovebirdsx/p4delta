//! bulk 夹具的共享定义：规模、深路径与内容生成。
//!
//! 这些值同时服务两处，必须逐字一致，所以只留一份：
//!
//! - `seed.rs` 造 **bulk 模板** 时按它入库两版（rev1 = [`first_revision`]，
//!   rev2 = [`second_revision`]），模板里 bulk 的 head 就是 rev2；
//! - `e2e_sync.rs` 的规模用例按它自算批数、删 restore 组、核对终态内容。
//!
//! 改了这里的任何一处，模板指纹都会跟着变（见 `template.rs` 的 `stamp`：它按这个文件的
//! 内容取指纹），旧模板自动作废，不用记得手工加版本号。

use std::fs;
use std::path::Path;

/// 单个 revision 入库的文件数。
///
/// 规模由「两组各自的旧批数都超过 8」反推（8 = 生产侧的 `MAX_PARALLEL_P4_COMMANDS`，
/// 见 `src/p4/process.rs`）：spec 155 字节 + 换行，旧预算 30719 字节每批装 196 条，
/// 门槛 = 1569 条/组。1800 给到 10 批、留一批以上余量。
pub const TOTAL: usize = 3600;

/// restore 组的规模：实例里前 `RESTORED` 个 bulk 文件被删掉（have 记录还在），
/// 于是它们落进 Restore 组，其余落进 Update 组。
pub const RESTORED: usize = 1800;

/// RCS 差分让一个路径的两版共用一份归档，避免逐版 gzip 附带的文件与目录复制。
/// 只改变测试种子的服务端存储，两版历史与工作区 text 内容照常保留。
pub const FILE_TYPE: &str = "text+D";

/// 七层各 17 字节的目录 + 10 字节文件名：相对路径 ~130 字节，全路径仍远离 MAX_PATH
/// （本机实测 ~198 字节）。spec 长度同时决定每批能装多少条与夹具要造多少文件，
/// 所以路径是刻意做深的。
pub const RELATIVE_DIR: &str = concat!(
    "bulk/d0123456789abcdef/d0123456789abcdef/d0123456789abcdef/d0123456789abcdef",
    "/d0123456789abcdef/d0123456789abcdef/d0123456789abcdef",
);

/// 第 `index` 个文件的相对路径（相对 client Root）。
pub fn relative_path(index: usize) -> String {
    format!("{RELATIVE_DIR}/{}", file_name(index))
}

/// 第一版内容：模板里的 rev1，实例同步 `...#1` 之后工作区里就是这一版。
///
/// 首行带序号、**每个文件的内容都不一样**：缓冲复用若引入「上一个文件的残留字节混进
/// 摘要」，逐文件算出的摘要会各自错位，终态核对能直接点出是哪一个文件。
pub fn first_revision(index: usize) -> String {
    format!("base {index:04}\n")
}

/// 第二版内容：模板里的 rev2，也是这条用例的 head 与终态。
pub fn second_revision(index: usize) -> String {
    format!("changed {index:04}\n")
}

/// 把第一版内容整批写进 `root`（目录不存在就建）。
pub fn write_first(root: &Path) {
    write_revision(root, first_revision);
}

/// 用第二版内容就地覆盖 `root` 下的文件（调用方先 `p4 edit`，p4 会把文件改成可写）。
pub fn write_second(root: &Path) {
    write_revision(root, second_revision);
}

fn file_name(index: usize) -> String {
    format!("f{index:04}.txt")
}

/// 把 [0, TOTAL) 写进 `root`。两版内容只差这一层生成，写入路径共用。
fn write_revision(root: &Path, content: fn(usize) -> String) {
    fs::create_dir_all(root).expect("create the bulk directory");
    let indices: Vec<usize> = (0..TOTAL).collect();
    par_for_each(&indices, |index, _| {
        fs::write(root.join(file_name(index)), content(index)).expect("write a bulk file");
    });
}

/// 夹具专用的并行遍历：把 `items` 分片派给若干线程，每片按序执行 `task(index, item)`。
///
/// 只服务于 bulk 夹具的写/删/核对：几千个彼此独立的本地文件操作，串行做在开着实时防护的
/// 机器上是纯税。片内保持顺序，失败信息里的序号仍然可读；任务里 panic 会原样传播
/// （`scope` 在 join 时重抛），不会退化成「线程静默死了、用例照绿」。
///
/// **写工作区文件时不要用它替代 `Sandbox::write`**——那条路径上的清只读位与回拨 mtime 是
/// 保命的（见 `Sandbox::write_bytes` 的说明），这里敢省是因为 bulk 文件的 mtime 不在判据里：
/// 它只由每轮实例的初始 `sync ...#1` 落定，seed 阶段不会再补同步。
pub fn par_for_each<T: Sync>(items: &[T], task: impl Fn(usize, &T) + Sync) {
    if items.is_empty() {
        return;
    }
    let threads = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(4);
    let chunk = items.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        for (chunk_index, slice) in items.chunks(chunk).enumerate() {
            let task = &task;
            scope.spawn(move || {
                for (offset, item) in slice.iter().enumerate() {
                    task(chunk_index * chunk + offset, item);
                }
            });
        }
    });
}
