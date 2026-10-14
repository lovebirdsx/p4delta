//! 工作区收集器的并行实现：整棵子树切成有限个**作业**，每个作业走一段 DFS；只在 Windows 上
//! 作为产品路径启用（见 [`collect`]）。
//!
//! 三条硬约束：
//! - **不重开目录**。父作业把**自己那次打开的枚举结果**连同延迟错误一起交给子作业，所以不存在
//!   「丢句柄按路径重开」带来的错误洗白与 TOCTOU；为此遍历自己实现，语义逐条对齐 walkdir 2.5.0。
//! - **顺序**。子作业整段插在它被切出的位置，合并结果与串行逐项同序（末次 `sort_by(path_lower)`
//!   是稳定排序，等键次序也一致）。
//! - **首错**。错误按 DFS 位置裁决，不是「哪个 worker 先失败」。
//!
//! 切点数与每个作业的句柄数都有上界；排队中的作业不占句柄，只记着它那个目录的条目。

#![cfg_attr(not(windows), allow(dead_code))] // 非 Windows 不启用并行路径，模块整体只被测试引用

use std::collections::HashSet;
use std::fs;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::SystemTime;

use anyhow::{Result, anyhow};
use rayon::ThreadPool;

use crate::model::WorkspaceFile;
use crate::path::{local_path_key, normalize_local_path_owned};
use crate::prune::has_pruned_ancestor;
use crate::scope::ExcludeSet;

/// 一轮扫描的线程预算，**含**跑根作业的调用线程：池线程 = min(可用核数, 预算) − 1（实测 2 线程
/// 已拿到主要收益、4 略好、8/16 无增益）。
const SCAN_THREAD_BUDGET: usize = 4;

/// **单个作业**走过多少个目录之后才允许往外切作业。必须是每作业的：全局门限在根作业手里就已越过，
/// 于是每个子作业都会继续往下切，最后几乎一个目录一个作业。它同时是启用门限（不够大就一次都不切）。
const PARALLEL_MIN_DIRS: usize = 8192;

/// 切点总数上限：切点有界就是「在途内存」有界（作业要等整轮结束才回收文件表）。
const MAX_PARALLEL_CUTS: usize = 4096;

/// 单个作业同时打开的目录句柄上限，与 `WalkDir` 的 `max_open` 同值：超过就把最早打开的目录的
/// 剩余条目读进内存再关掉句柄，条目顺序与其中的错误一个都不丢。
const MAX_OPEN_DIRS: usize = 10;

/// 扫描代价的可调上界。[`Default`] 是产品口径，测试用更小的值直接压到并行分支上。
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScanLimits {
    pub(crate) min_dirs: usize,
    pub(crate) max_cuts: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        ScanLimits {
            min_dirs: PARALLEL_MIN_DIRS,
            max_cuts: MAX_PARALLEL_CUTS,
        }
    }
}

/// 纯函数，便于按平台核数把落法钉住。
fn pool_workers(available: usize) -> usize {
    available.min(SCAN_THREAD_BUDGET).saturating_sub(1)
}

/// 扫描池进程内单例：第一次真要切作业时才构建；可用并行度为 1 时返回 `None`。
fn scan_pool() -> Option<Arc<ThreadPool>> {
    static POOL: OnceLock<Option<Arc<ThreadPool>>> = OnceLock::new();

    POOL.get_or_init(|| {
        let workers = pool_workers(std::thread::available_parallelism().map_or(1, |n| n.get()));
        if workers == 0 {
            return None;
        }
        rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .thread_name(|index| format!("p4delta-scan-{index}"))
            .build()
            .ok()
            .map(Arc::new)
    })
    .clone()
}

// ---- 文件系统接口：产品是 `RealFs`，测试注入实现用来钉住只有注入才能验的性质 ----

/// 条目类型。`std::fs::FileType` 在外部造不出来，测试要能构造，所以自己定一个。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EntryKind {
    /// 真目录（`symlink_metadata` 口径：符号链接不算）。
    Dir,
    File,
    Symlink,
    /// 其它（fifo、设备……）：收集器一律忽略，与 walkdir 上层的判断一致。
    Other,
}

/// 收集器用到的元数据：大小与**还没报错的**修改时间。
///
/// `modified` 存成 `Result` 而不是当场 `?`：`Metadata::modified()` 只在**真正要收这个文件**时才
/// 该失败——串行实现里它排在排除判断之后、只对文件/链接调用。真取数推迟到 [`push_file`]，
/// 错误文案也就与串行一致（裸 `io::Error`，不带路径）。
#[derive(Debug)]
pub(crate) struct EntryMeta {
    pub(crate) kind: EntryKind,
    pub(crate) size: u64,
    pub(crate) modified: io::Result<SystemTime>,
}

impl EntryMeta {
    fn from_fs(meta: fs::Metadata) -> EntryMeta {
        EntryMeta {
            kind: entry_kind(&meta.file_type()),
            size: meta.len(),
            modified: meta.modified(),
        }
    }
}

pub(crate) struct Entry {
    /// 与父目录的 `read_dir` 参数拼接而来——与 walkdir 拼出来的路径逐字相同，结果表里的路径因此
    /// 不受影响。
    pub(crate) path: PathBuf,
    pub(crate) kind: EntryKind,
    /// Windows 上枚举结果自带元数据（不走额外系统调用）就在这里带上，与 walkdir 的取数时机一致；
    /// 其他平台是 `None`，文件分支再 `symlink_metadata`。`None` 是「不预先取」，不是「取不到」。
    pub(crate) meta: Option<EntryMeta>,
}

/// 收集器用到的文件系统操作。错误文案必须与 walkdir 的 `Error` 一致——`tests/` 与对拍用例按它比，
/// 换掉遍历器不该改变用户看到的报错。
pub(crate) trait ScanFs: Send + Sync + 'static {
    type Entries: Iterator<Item = Result<Entry>> + Send;

    fn symlink_metadata(&self, path: &Path) -> Result<EntryMeta>;
    fn metadata(&self, path: &Path) -> Result<EntryMeta>;
    fn read_dir(&self, path: &Path) -> Result<Self::Entries>;
}

/// 带路径的错误，文案与 `walkdir::Error` 的 `IO error for operation on ...` 逐字相同。
fn path_error(path: &Path, error: io::Error) -> anyhow::Error {
    anyhow!("IO error for operation on {}: {}", path.display(), error)
}

/// 不带路径的错误：迭代中途读目录失败时 walkdir 也不带路径。
fn io_error(error: io::Error) -> anyhow::Error {
    error.into()
}

fn entry_kind(file_type: &fs::FileType) -> EntryKind {
    if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_dir() {
        EntryKind::Dir
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

/// 产品路径上的文件系统（测试注入的那份见用例里的 `FakeFs`）。
pub(crate) struct RealFs;

pub(crate) struct RealEntries(fs::ReadDir);

impl Iterator for RealEntries {
    type Item = Result<Entry>;

    fn next(&mut self) -> Option<Result<Entry>> {
        let entry = match self.0.next()? {
            Ok(entry) => entry,
            Err(error) => return Some(Err(io_error(error))),
        };
        let path = entry.path();
        let kind = match entry.file_type() {
            Ok(file_type) => entry_kind(&file_type),
            Err(error) => return Some(Err(path_error(&path, error))),
        };
        // Windows 上枚举结果自带元数据（不走额外系统调用），这里就带上，与 walkdir 建条目时的
        // 取数时机一致——只取数、不当场报错，`modified()` 的失败推迟到真要收这个文件时。
        #[cfg(windows)]
        let meta = match entry.metadata() {
            Ok(metadata) => Some(EntryMeta::from_fs(metadata)),
            Err(error) => return Some(Err(path_error(&path, error))),
        };
        #[cfg(not(windows))]
        let meta = None;

        Some(Ok(Entry { path, kind, meta }))
    }
}

impl ScanFs for RealFs {
    type Entries = RealEntries;

    fn symlink_metadata(&self, path: &Path) -> Result<EntryMeta> {
        fs::symlink_metadata(path)
            .map(EntryMeta::from_fs)
            .map_err(|error| path_error(path, error))
    }

    fn metadata(&self, path: &Path) -> Result<EntryMeta> {
        fs::metadata(path)
            .map(EntryMeta::from_fs)
            .map_err(|error| path_error(path, error))
    }

    fn read_dir(&self, path: &Path) -> Result<RealEntries> {
        fs::read_dir(path)
            .map(RealEntries)
            .map_err(|error| path_error(path, error))
    }
}

// ---- 作业与调度 ----

type Slot = Arc<Mutex<Option<JobOutcome>>>;

/// 一个作业（一段 DFS）的收集结果。计数与错误都只管**它自己**走到的那些，子作业的由子作业给。
#[derive(Default)]
struct JobOutcome {
    /// 保持该作业内的 DFS 次序。
    files: Vec<WorkspaceFile>,
    num_dirs: usize,
    total_size: u64,
    own_error: Option<anyhow::Error>,
    /// 按切出次序排列的子作业插入点。
    splices: Vec<Splice>,
}

struct Splice {
    /// 子作业的文件全部排在父作业 `files` 的这个下标之前。
    at: usize,
    child: Slot,
}

/// 互斥量中毒不该让整轮扫描变成 panic：锁被 poison 时直接拿回内层数据。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone)]
pub(crate) enum Pool {
    /// 产品路径：第一次真要切作业时才去建池（建池要起 3 个 OS 线程，小树不该付这笔钱）。
    Lazy,
    /// 测试注入的池：固定线程数，这样「真的并行起来了吗」不依赖跑测试的机器有几个核。
    #[cfg(test)]
    Fixed(Arc<ThreadPool>),
}

impl Pool {
    fn get(&self) -> Option<Arc<ThreadPool>> {
        match self {
            Pool::Lazy => scan_pool(),
            #[cfg(test)]
            Pool::Fixed(pool) => Some(Arc::clone(pool)),
        }
    }
}

/// 一次扫描的共享状态，被所有作业通过 `Arc` 共用。
struct Scan<F: ScanFs> {
    fs: Arc<F>,
    pool: Pool,
    skip_keys: HashSet<String>,
    excludes: ExcludeSet,
    limits: ScanLimits,
    cuts: AtomicUsize,
    /// 出现过错误之后不再切新作业：更晚的工作停止扩张。
    failed: AtomicBool,
    /// 还没跑完的作业数，`wait` 用它等待整轮结束。
    pending: Mutex<usize>,
    done: Condvar,
}

impl<F: ScanFs> Scan<F> {
    fn new(
        fs: Arc<F>,
        pool: Pool,
        skip_keys: &HashSet<String>,
        excludes: &ExcludeSet,
        limits: ScanLimits,
    ) -> Self {
        Scan {
            fs,
            pool,
            skip_keys: skip_keys.clone(),
            excludes: excludes.clone(),
            limits,
            cuts: AtomicUsize::new(0),
            failed: AtomicBool::new(false),
            pending: Mutex::new(0),
            done: Condvar::new(),
        }
    }

    /// 排除与剪枝的合并判断：命中就整棵子树不碰。
    fn is_skipped(&self, key: &str) -> bool {
        self.excludes.excludes_key(key)
            || self.skip_keys.contains(key)
            || has_pruned_ancestor(key, &self.skip_keys)
    }

    /// 判断这个目录能不能切出去，能就返回（线程池，结果槽）。建池放在**最后**：没走够目录数、
    /// 切点用光、已经出过错，都不该白建一次池。
    fn delegate(&self, seen_dirs: usize) -> Option<(Arc<ThreadPool>, Slot)> {
        if seen_dirs < self.limits.min_dirs {
            return None;
        }
        if self.failed.load(Ordering::Relaxed) {
            return None;
        }
        if self.cuts.load(Ordering::Relaxed) >= self.limits.max_cuts {
            return None;
        }
        let pool = self.pool.get()?;
        if self.cuts.fetch_add(1, Ordering::Relaxed) >= self.limits.max_cuts {
            self.cuts.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        Some((pool, Arc::new(Mutex::new(None))))
    }

    fn finish_one(&self) {
        let mut pending = lock(&self.pending);
        *pending -= 1;
        if *pending == 0 {
            self.done.notify_all();
        }
    }

    fn wait(&self) {
        let mut pending = lock(&self.pending);
        while *pending > 0 {
            pending = self
                .done
                .wait(pending)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

enum Stream<E> {
    /// 还开着的句柄。`pending` 是打开这个目录时的错误：walkdir 把它推迟到遍历到这个目录时才吐
    /// 出来（打开失败的那一项本身照常 yield），这里同样推迟，错误在 DFS 里的位置才不变。
    Open {
        entries: Option<E>,
        pending: Option<anyhow::Error>,
    },
    /// 超过句柄上限后的形态：剩余条目（含其中的错误）已经读进内存，句柄已关。
    Buffered(std::vec::IntoIter<Result<Entry>>),
}

impl<E: Iterator<Item = Result<Entry>>> Stream<E> {
    fn next(&mut self) -> Option<Result<Entry>> {
        match self {
            Stream::Open { entries, pending } => {
                if let Some(error) = pending.take() {
                    return Some(Err(error));
                }
                entries.as_mut()?.next()
            }
            Stream::Buffered(entries) => entries.next(),
        }
    }

    fn is_open(&self) -> bool {
        matches!(self, Stream::Open { .. })
    }

    /// 关掉句柄：把剩下的条目读进内存（walkdir 的 `DirList::close` 同一件事）。
    fn close(&mut self) {
        if !self.is_open() {
            return;
        }
        let mut open = std::mem::replace(self, Stream::Buffered(Vec::new().into_iter()));
        let mut drained = Vec::new();
        while let Some(item) = open.next() {
            drained.push(item);
        }
        *self = Stream::Buffered(drained.into_iter());
    }
}

/// 打开一个目录，得到它的帧。打开失败不在这里报错，而是成为帧的延迟错误（见 [`Stream::Open`]）。
fn open_dir<F: ScanFs>(path: &Path, scan: &Arc<Scan<F>>) -> Stream<F::Entries> {
    match scan.fs.read_dir(path) {
        Ok(entries) => Stream::Open {
            entries: Some(entries),
            pending: None,
        },
        Err(error) => Stream::Open {
            entries: None,
            pending: Some(error),
        },
    }
}

/// 推新目录帧之前先腾出句柄名额：**先关后开**，否则峰值会是 `MAX_OPEN_DIRS + 1`。
fn make_room<E: Iterator<Item = Result<Entry>>>(stack: &mut [Stream<E>]) {
    if stack.iter().filter(|frame| frame.is_open()).count() >= MAX_OPEN_DIRS
        && let Some(oldest) = stack.iter_mut().find(|frame| frame.is_open())
    {
        oldest.close();
    }
}

/// 产品入口：**Windows** 走本模块的并行实现，其它平台走 `workspace::collect_directory` 的
/// `WalkDir` 串行路径——只有 Windows 做过实机验证，注入用例证明得了归并次序与首错裁决这类
/// **算法**性质，证明不了 Unix 上的 syscall 形态与内核行为。
///
/// Windows 上不切作业时（树不够大、单核、池建不起来）走的也是这条实现，只是全程在调用线程上，
/// 不是另外还有一条串行路径。
pub(crate) fn collect(
    dir: &str,
    skip_keys: &HashSet<String>,
    excludes: &ExcludeSet,
) -> Result<(Vec<WorkspaceFile>, usize, u64)> {
    #[cfg(windows)]
    {
        collect_with(
            dir,
            skip_keys,
            excludes,
            ScanLimits::default(),
            Arc::new(RealFs),
            Pool::Lazy,
        )
        .0
    }
    #[cfg(not(windows))]
    {
        crate::workspace::collect_directory(dir, skip_keys, excludes)
    }
}

/// 同 [`collect`]，但可以指定上界、注入文件系统与线程池。返回的第二个值是切出去的子树数，
/// 测试拿它钉「小树不切」这类性质，产品路径不看。
pub(crate) fn collect_with<F: ScanFs>(
    dir: &str,
    skip_keys: &HashSet<String>,
    excludes: &ExcludeSet,
    limits: ScanLimits,
    fs: Arc<F>,
    pool: Pool,
) -> (Result<(Vec<WorkspaceFile>, usize, u64)>, usize) {
    let scan = Arc::new(Scan::new(fs, pool, skip_keys, excludes, limits));
    let root_slot: Slot = Arc::new(Mutex::new(None));

    // 根作业跑在调用线程上（调用方本来就在阻塞线程里），不额外占池线程，也保证小树完全不
    // 接触线程池。子作业才进池。
    *lock(&scan.pending) = 1;
    run_include_root(Path::new(dir), &scan, &root_slot);
    scan.finish_one();
    scan.wait();

    let cuts = scan.cuts.load(Ordering::Relaxed);
    let outcome = lock(&root_slot).take().expect("根作业一定会写下结果");
    let (files, num_dirs, total_size, error) = merge(outcome, cuts);
    let result = match error {
        Some(error) => Err(error),
        None => Ok((files, num_dirs, total_size)),
    };

    (result, cuts)
}

/// 收尾：写下结果并唤醒等待方。`failed` 让更晚的工作停止扩张。
fn finish<F: ScanFs>(scan: &Arc<Scan<F>>, outcome: JobOutcome, slot: &Slot) {
    if outcome.own_error.is_some() {
        scan.failed.store(true, Ordering::Relaxed);
    }
    *lock(slot) = Some(outcome);
}

/// 遍历**范围里的显式目录入口**，与 walkdir 的 depth 0 规则逐条对应：
/// - 真目录：排除/剪枝命中就整段不碰（**连 `read_dir` 都不调用**）。串行实现里 walkdir 在 yield
///   这个目录条目前就已经打开过它，`skip_current_dir` 只是把那次打开的结果连同句柄一起丢掉——
///   两者对外都不报错，差别只在少一次系统调用；否则计数并下钻。
/// - 符号链接根：`follow_root_links` 让 walkdir 仍会跟进去，但条目本身按链接收（当文件，大小与
///   时间描述链接自己）；`fs::metadata` 只用到目标的类型，失败（断链）是整轮的错误。
/// - 文件根：当文件收，其它类型忽略。
fn run_include_root<F: ScanFs>(root: &Path, scan: &Arc<Scan<F>>, slot: &Slot) {
    let mut outcome = JobOutcome::default();
    let mut stack: Vec<Stream<F::Entries>> = Vec::new();
    let mut seen_dirs = 0usize;

    let frame = match scan.fs.symlink_metadata(root) {
        Err(error) => {
            outcome.own_error = Some(error);
            None
        }
        Ok(meta) => match meta.kind {
            EntryKind::Dir => {
                seen_dirs += 1;
                let key = local_path_key(&root.display().to_string());
                if scan.is_skipped(&key) {
                    None
                } else {
                    outcome.num_dirs += 1;
                    Some(open_dir(root, scan))
                }
            }
            EntryKind::Symlink => {
                // 先按 walkdir 的口径 stat 目标：失败是这一轮的错误，成功才既收链接又下钻。
                // 目标元数据只用得上类型——`modified` 取不到不影响跟不跟进去。
                match scan.fs.metadata(root) {
                    Err(error) => {
                        outcome.own_error = Some(error);
                        None
                    }
                    Ok(target) => {
                        collect_path(root, meta, scan, &mut outcome);
                        (target.kind == EntryKind::Dir).then(|| open_dir(root, scan))
                    }
                }
            }
            EntryKind::File => {
                collect_path(root, meta, scan, &mut outcome);
                None
            }
            EntryKind::Other => None,
        },
    };

    if let Some(frame) = frame {
        make_room(&mut stack);
        stack.push(frame);
    }
    if outcome.own_error.is_none() {
        walk(&mut stack, &mut seen_dirs, scan, &mut outcome);
    }
    finish(scan, outcome, slot);
}

/// 跑一个子作业：根帧（已经打开的目录，或打开失败的延迟错误）由父作业交过来。
fn run_child<F: ScanFs>(root: Stream<F::Entries>, scan: &Arc<Scan<F>>, slot: &Slot) {
    let mut outcome = JobOutcome::default();
    let mut stack = vec![root];
    let mut seen_dirs = 0usize;

    walk(&mut stack, &mut seen_dirs, scan, &mut outcome);
    finish(scan, outcome, slot);
}

/// 一段 DFS：帧栈的顶就是当前目录，走完就弹出去接着走上层。
fn walk<F: ScanFs>(
    stack: &mut Vec<Stream<F::Entries>>,
    seen_dirs: &mut usize,
    scan: &Arc<Scan<F>>,
    outcome: &mut JobOutcome,
) {
    while !stack.is_empty() {
        let next = stack.last_mut().expect("栈非空").next();
        let entry = match next {
            None => {
                stack.pop();
                continue;
            }
            Some(Ok(entry)) => entry,
            Some(Err(error)) => {
                outcome.own_error = Some(error);
                break;
            }
        };

        match entry.kind {
            EntryKind::Dir => {
                *seen_dirs += 1;
                let key = local_path_key(&entry.path.display().to_string());
                if scan.is_skipped(&key) {
                    continue;
                }
                outcome.num_dirs += 1;

                if let Some((pool, child_slot)) = scan.delegate(*seen_dirs) {
                    // 现在就把目录打开：子作业不重开，打开失败的延迟错误也由子作业在同一个 DFS
                    // 位置吐出来。紧接着关掉句柄——排队等线程的作业不占 fd，否则跑得快的父作业
                    // 能把整棵树的目录开着排队。`make_room` 得先做：这个 `read_dir` 也是新开的
                    // 句柄，不然峰值会到 `MAX_OPEN_DIRS + 1`。
                    make_room(stack);
                    let mut frame = open_dir(&entry.path, scan);
                    frame.close();
                    outcome.splices.push(Splice {
                        at: outcome.files.len(),
                        child: Arc::clone(&child_slot),
                    });

                    let scan = Arc::clone(scan);
                    *lock(&scan.pending) += 1;
                    pool.spawn(move || {
                        // 子作业 panic 既不能 abort 整进程，也不能让等待方永远醒不过来：
                        // catch 住，把一个作业失败写进结果槽，由归并按 DFS 位置裁决。
                        if let Err(payload) =
                            catch_unwind(AssertUnwindSafe(|| run_child(frame, &scan, &child_slot)))
                        {
                            scan.failed.store(true, Ordering::Relaxed);
                            *lock(&child_slot) = Some(JobOutcome {
                                own_error: Some(panic_error(payload)),
                                ..Default::default()
                            });
                        }
                        scan.finish_one();
                    });
                    continue;
                }

                make_room(stack);
                let frame = open_dir(&entry.path, scan);
                stack.push(frame);
            }
            EntryKind::File | EntryKind::Symlink => {
                if let Err(error) = collect_entry(entry, scan, outcome) {
                    outcome.own_error = Some(error);
                    break;
                }
            }
            EntryKind::Other => {}
        }
    }
}

/// 一个文件/符号链接条目对应的结果。排除判断在取元数据**之前**，与串行收集器同一顺序：被排除的
/// 条目不会因为元数据读不出来而让整轮失败。
fn collect_entry<F: ScanFs>(
    entry: Entry,
    scan: &Arc<Scan<F>>,
    outcome: &mut JobOutcome,
) -> Result<()> {
    let path_string = normalize_local_path_owned(entry.path.display().to_string());
    if scan.excludes.excludes_key(&local_path_key(&path_string)) {
        return Ok(());
    }

    let meta = match entry.meta {
        Some(meta) => meta,
        None => scan.fs.symlink_metadata(&entry.path)?,
    };
    push_file(path_string, meta, outcome).map_err(io_error)
}

/// 同上，但元数据已经拿在手上（显式目录入口那一支）；这里的错误没处返回，只能记进结果。
fn collect_path<F: ScanFs>(
    path: &Path,
    meta: EntryMeta,
    scan: &Arc<Scan<F>>,
    outcome: &mut JobOutcome,
) {
    let path_string = normalize_local_path_owned(path.display().to_string());
    if scan.excludes.excludes_key(&local_path_key(&path_string)) {
        return;
    }
    if let Err(error) = push_file(path_string, meta, outcome) {
        outcome.own_error = Some(io_error(error));
    }
}

/// 收下一个文件：到这里才真读修改时间，冒出去的就是串行实现里 `meta.modified()?` 那条裸
/// `io::Error`（不带路径）。
fn push_file(path_string: String, meta: EntryMeta, outcome: &mut JobOutcome) -> io::Result<()> {
    let date = meta.modified?;
    outcome.total_size += meta.size;
    outcome.files.push(WorkspaceFile {
        path_lower: local_path_key(&path_string),
        path: path_string,
        size: meta.size,
        date,
        filtered: false,
    });
    Ok(())
}

fn panic_error(payload: Box<dyn std::any::Any + Send>) -> anyhow::Error {
    let message = payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "无消息".to_owned());
    anyhow!("扫描线程 panic：{message}")
}

/// 一个待展开的作业帧。文件是 `IntoIter`，合并时按插入位搬运，不复制元素。
struct Frame {
    files: std::vec::IntoIter<WorkspaceFile>,
    splices: std::vec::IntoIter<Splice>,
    cursor: usize,
    own_error: Option<anyhow::Error>,
    num_dirs: usize,
    total_size: u64,
}

impl Frame {
    fn new(outcome: JobOutcome) -> Self {
        Frame {
            files: outcome.files.into_iter(),
            splices: outcome.splices.into_iter(),
            cursor: 0,
            own_error: outcome.own_error,
            num_dirs: outcome.num_dirs,
            total_size: outcome.total_size,
        }
    }
}

/// 整棵作业树上还没被搬走的文件总数：所有作业都写完了结果，所以这是个定值，拿它把目的表**一次**
/// 按最终大小建出来（自然增长会多出一份翻倍期的旧表，实测多占几百 MiB 峰值 WS）。
fn total_files(root: &JobOutcome) -> usize {
    let mut total = root.files.len();
    let mut stack: Vec<Slot> = root.splices.iter().map(|s| Arc::clone(&s.child)).collect();
    while let Some(slot) = stack.pop() {
        let guard = lock(&slot);
        let Some(child) = guard.as_ref() else {
            continue;
        };
        total += child.files.len();
        stack.extend(child.splices.iter().map(|s| Arc::clone(&s.child)));
    }
    total
}

/// 按 DFS 次序拼接所有作业，并选出第一个（DFS 最早）的错误。
///
/// `cuts == 0` 时根作业的表**就是**结果，原样返回、不另开容器。切过作业时数据还散在几百张源表里，
/// 而要交出去的是一张连续的表——这一张目的表免不掉（代价见 `docs/why-faster.md`）。
/// 用显式栈而不是递归：作业树深度跟着目录深度走，递归会压爆线程栈。
fn merge(root: JobOutcome, cuts: usize) -> (Vec<WorkspaceFile>, usize, u64, Option<anyhow::Error>) {
    if cuts == 0 {
        return (root.files, root.num_dirs, root.total_size, root.own_error);
    }

    let mut files = Vec::with_capacity(total_files(&root));
    let mut num_dirs = 0;
    let mut total_size = 0;
    let mut error: Option<anyhow::Error> = None;

    let mut stack = vec![Frame::new(root)];
    while !stack.is_empty() {
        let next = stack.last_mut().expect("栈非空").splices.next();

        let Some(splice) = next else {
            // 这个作业的插入点走完了：搬完剩下的文件，收下它的计数与（更晚的）错误。
            let frame = stack.pop().expect("栈非空");
            files.extend(frame.files);
            if error.is_none() {
                error = frame.own_error;
            }
            num_dirs += frame.num_dirs;
            total_size += frame.total_size;
            continue;
        };

        let frame = stack.last_mut().expect("栈非空");
        while frame.cursor < splice.at {
            files.push(frame.files.next().expect("插入位不超出本作业文件数"));
            frame.cursor += 1;
        }

        let child = splice
            .child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .expect("整轮结束前所有子作业都写下了结果");
        stack.push(Frame::new(child));
    }

    (files, num_dirs, total_size, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{TempTree, symlink_file};

    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    /// 产品门限之上再压满并行分支：产品路径上「树不够大就一次都不切」由另外两条用例单独钉。
    fn eager() -> ScanLimits {
        ScanLimits {
            min_dirs: 1,
            max_cuts: usize::MAX,
        }
    }

    /// 测试用的池：**显式**指定线程数，不依赖跑测试的机器有几个核——产品口径在单核机器上
    /// 不建池，那些用例会假失败。
    fn pool(threads: usize) -> Pool {
        Pool::Fixed(Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .thread_name(|index| format!("p4delta-scan-test-{index}"))
                .build()
                .expect("测试池一定能建起来"),
        ))
    }

    fn serial(
        root: &str,
        skip_keys: &HashSet<String>,
        excludes: &ExcludeSet,
    ) -> (Vec<WorkspaceFile>, usize, u64) {
        crate::workspace::collect_directory(root, skip_keys, excludes).expect("串行收集必须成功")
    }

    fn parallel(
        root: &str,
        skip_keys: &HashSet<String>,
        excludes: &ExcludeSet,
        limits: ScanLimits,
    ) -> (Vec<WorkspaceFile>, usize, u64, usize) {
        let (result, cuts) =
            collect_with(root, skip_keys, excludes, limits, Arc::new(RealFs), pool(3));
        let (files, dirs, size) = result.expect("并行收集必须成功");
        (files, dirs, size, cuts)
    }

    /// 按 `path_lower` 排序后逐项比较（含等键相对次序——两边都是稳定排序，拼接顺序错位会在这里
    /// 现形），再比 `num_dirs` 与 `total_size`。
    fn assert_same(
        serial: (Vec<WorkspaceFile>, usize, u64),
        parallel: (Vec<WorkspaceFile>, usize, u64),
    ) {
        let (mut expected, expected_dirs, expected_size) = serial;
        let (mut actual, actual_dirs, actual_size) = parallel;

        assert_eq!(expected_dirs, actual_dirs, "num_dirs 必须与串行一致");
        assert_eq!(expected_size, actual_size, "total_size 必须与串行一致");

        expected.sort_by(|a, b| a.path_lower.cmp(&b.path_lower));
        actual.sort_by(|a, b| a.path_lower.cmp(&b.path_lower));
        assert_eq!(expected.len(), actual.len(), "文件数必须一致");

        for (left, right) in expected.iter().zip(&actual) {
            assert_eq!(left.path, right.path);
            assert_eq!(left.path_lower, right.path_lower);
            assert_eq!(left.size, right.size);
            assert_eq!(left.date, right.date);
            assert_eq!(left.filtered, right.filtered);
        }
    }

    fn temp_root(tree: &TempTree) -> String {
        tree.root.to_string_lossy().to_string()
    }

    /// 目录链接（符号链接）。系统不允许时返回错误，由调用方降级到 junction 或跳过。
    fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(target, link)
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = (target, link);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "此平台不支持目录符号链接",
            ))
        }
    }

    /// Windows 的目录 junction 走另一套 reparse 语义（`IO_REPARSE_TAG_MOUNT_POINT`），std 与
    /// walkdir 都把它当符号链接：入口处当文件收、不跟下去，显式目录根则跟进去。
    #[cfg(windows)]
    fn junction(target: &Path, link: &Path) -> std::io::Result<()> {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other("mklink /J 建 junction 失败"))
        }
    }

    #[cfg(not(windows))]
    fn junction(_target: &Path, _link: &Path) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "junction 只在 Windows 上有",
        ))
    }

    /// 能不能在这个文件系统上建链接、建得出哪一种，取决于权限与平台：能做到哪一步就验哪一步，
    /// 一个都做不出来才跳过。
    fn make_directory_link(target: &Path, link: &Path) -> bool {
        symlink_dir(target, link).is_ok() || junction(target, link).is_ok()
    }

    // ---- 与串行 walkdir 收集器对拍 ----

    /// 混合树：多层嵌套、空目录、非 ASCII 名。逐字段与串行一致。
    #[test]
    fn parallel_scan_matches_serial_on_a_nested_tree() {
        let tree = TempTree::new("scan-parallel-nested");
        tree.file("a.txt", "root file");
        tree.file("sub/b.txt", "nested");
        tree.file("sub/deep/c.txt", "deeper");
        tree.file("sub/deep/d.txt", "deeper still");
        tree.file("中文/说明.txt", "unicode path");
        tree.file("empty-dir/.keep", "");
        tree.dir("really-empty-dir");
        tree.file("many/0.txt", "x");
        tree.file("many/1.txt", "yy");
        tree.file("many/2.txt", "zzz");

        let excludes = ExcludeSet::default();
        let root = temp_root(&tree);
        let (files, dirs, size, _) = parallel(&root, &HashSet::new(), &excludes, eager());
        assert_same(
            serial(&root, &HashSet::new(), &excludes),
            (files, dirs, size),
        );
    }

    /// 排除项与剪枝目录：命中的子树整段不进，`num_dirs` 不计它们，也不计它们的子孙。
    #[test]
    fn parallel_scan_matches_serial_with_excludes_and_pruning() {
        let tree = TempTree::new("scan-parallel-prune");
        tree.file("keep/a.txt", "keep");
        tree.file("keep/vendor/skip.txt", "excluded subtree");
        tree.file("node_modules/mod.js", "pruned by ignore");
        tree.file("node_modules/nested/deep.js", "pruned, nested");
        tree.file("src/app.txt", "normal");
        tree.file("src/lib/x.rs", "normal");

        let excluded_dir = tree.root.join("keep/vendor").display().to_string();
        let pruned_dir = tree.root.join("node_modules").display().to_string();
        let excludes = ExcludeSet::from_dir_keys(&[&local_path_key(&excluded_dir)]);
        let skip_keys: HashSet<String> = [local_path_key(&pruned_dir)].into_iter().collect();

        let root = temp_root(&tree);
        let (files, dirs, size, _) = parallel(&root, &skip_keys, &excludes, eager());
        let paths: Vec<String> = files.iter().map(|file| file.path.clone()).collect();
        assert_same(serial(&root, &skip_keys, &excludes), (files, dirs, size));

        assert!(
            !paths
                .iter()
                .any(|path| path.contains("vendor") || path.contains("node_modules")),
            "被排除与被剪枝的子树都不能进入结果"
        );
    }

    /// 深链与宽扇出：并行分支真的被用上（cuts > 0），结果仍与串行一致。
    #[test]
    fn parallel_scan_handles_deep_and_wide_shapes() {
        let tree = TempTree::new("scan-parallel-shapes");

        let mut chain = String::from("chain");
        for level in 0..24 {
            tree.file(&format!("{chain}/leaf.txt"), "x");
            chain.push_str(&format!("/d{level}"));
        }
        tree.file(&format!("{chain}/bottom.txt"), "x");

        for branch in 0..40 {
            tree.file(&format!("wide/branch{branch:02}/one.txt"), "1");
            tree.file(&format!("wide/branch{branch:02}/two.txt"), "22");
            tree.file(&format!("wide/branch{branch:02}/sub/three.txt"), "333");
        }

        fn fork(tree: &TempTree, prefix: &str, depth: usize) {
            tree.file(&format!("{prefix}/leaf.txt"), "x");
            if depth == 0 {
                return;
            }
            for side in ["left", "right"] {
                fork(tree, &format!("{prefix}/{side}"), depth - 1);
            }
        }
        for side in ["left", "right"] {
            fork(&tree, &format!("tree/{side}"), 7);
        }

        let excludes = ExcludeSet::default();
        let root = temp_root(&tree);
        let (serial_files, serial_dirs, serial_size) = serial(&root, &HashSet::new(), &excludes);
        let (parallel_files, parallel_dirs, parallel_size, cuts) =
            parallel(&root, &HashSet::new(), &excludes, eager());

        assert!(cuts > 0, "深链 + 宽扇出的形状必须真的被切开");
        assert_same(
            (serial_files, serial_dirs, serial_size),
            (parallel_files, parallel_dirs, parallel_size),
        );
    }

    /// 链接：内部目录链接与 junction 都不跟随（当文件收），文件链接同理，断链也照样收
    /// （大小按链接自己）。建不出链接就跳过。
    #[test]
    fn links_are_collected_without_following() {
        let tree = TempTree::new("scan-parallel-links");
        tree.file("real/target.bin", "payload");
        let target = tree.root.join("real");
        let dir_link = tree.root.join("dir-link");
        let file_link = tree.root.join("file-link.bin");
        let dangling = tree.root.join("dangling.bin");

        let has_dir_link = make_directory_link(&target, &dir_link);
        let has_file_link = symlink_file(&target.join("target.bin"), &file_link).is_ok();
        let has_dangling = symlink_file(&tree.root.join("nowhere.bin"), &dangling).is_ok();
        if !has_dir_link && !has_file_link && !has_dangling {
            eprintln!("跳过：这个环境下建不出任何链接");
            return;
        }

        let excludes = ExcludeSet::default();
        let root = temp_root(&tree);
        let (files, dirs, size, cuts) = parallel(&root, &HashSet::new(), &excludes, eager());
        let keys: Vec<String> = files.iter().map(|file| file.path_lower.clone()).collect();
        assert_same(
            serial(&root, &HashSet::new(), &excludes),
            (files, dirs, size),
        );
        assert!(cuts > 0);

        if has_dir_link {
            let link_key = local_path_key(&dir_link.display().to_string());
            assert!(keys.contains(&link_key), "目录链接必须当文件收，不能跟进去");
            assert!(
                !keys.iter().any(|key| key.starts_with(&format!(
                    "{}{}",
                    link_key,
                    std::path::MAIN_SEPARATOR
                ))),
                "目录链接下面不能收出任何东西"
            );
        }
        for (created, path) in [(has_file_link, &file_link), (has_dangling, &dangling)] {
            if created {
                let key = local_path_key(&path.display().to_string());
                assert!(keys.contains(&key), "{} 必须被收集", path.display());
            }
        }
    }

    /// 显式目录入口就是链接：链接本身当文件收，**并且**跟进目标子树——walkdir 的
    /// `follow_root_links` 就是这个形状，少一半都会与串行对不上。
    #[test]
    fn a_root_link_is_collected_and_descended() {
        let tree = TempTree::new("scan-parallel-root-link");
        tree.file("target/inner.txt", "inner");
        let link = tree.root.join("link");
        if !make_directory_link(&tree.root.join("target"), &link) {
            eprintln!("跳过：这个环境下建不出目录链接");
            return;
        }

        let excludes = ExcludeSet::default();
        let root = link.to_string_lossy().to_string();
        let expected = serial(&root, &HashSet::new(), &excludes);
        assert!(
            expected
                .0
                .iter()
                .any(|file| file.path_lower == local_path_key(&root)),
            "前提：串行收集器把根链接本身当文件收"
        );
        assert!(
            expected
                .0
                .iter()
                .any(|file| file.path.ends_with("inner.txt")),
            "前提：串行收集器跟进了链接目标"
        );

        let (files, dirs, size, _) = parallel(&root, &HashSet::new(), &excludes, eager());
        assert_same(expected, (files, dirs, size));
    }

    /// 断链的显式根：`follow_root_links` 会 stat 目标，失败就是整轮的错误（不是「空结果 + 成功」）。
    #[test]
    fn a_dangling_root_symlink_is_an_error() {
        let tree = TempTree::new("scan-parallel-dangling-root");
        let link = tree.root.join("dangling");
        if symlink_file(&tree.root.join("nowhere"), &link).is_err() {
            eprintln!("跳过：这个环境下建不出符号链接");
            return;
        }
        let excludes = ExcludeSet::default();
        let root = link.to_string_lossy().to_string();

        let serial_error = crate::workspace::collect_directory(&root, &HashSet::new(), &excludes)
            .expect_err("串行收集器在断链根上必须失败");
        let (result, _) = collect_with(
            &root,
            &HashSet::new(),
            &excludes,
            eager(),
            Arc::new(RealFs),
            pool(3),
        );
        let parallel_error = result.expect_err("并行扫描也必须失败");
        assert_eq!(
            parallel_error.to_string(),
            serial_error.to_string(),
            "换掉遍历实现不能改变报错文案"
        );
    }

    /// 根不存在：错误文案与串行逐字一致。
    #[test]
    fn a_missing_root_reports_the_same_error_as_the_serial_collector() {
        let tree = TempTree::new("scan-parallel-missing");
        let missing = tree.root.join("does-not-exist").display().to_string();
        let excludes = ExcludeSet::default();

        let serial_error =
            crate::workspace::collect_directory(&missing, &HashSet::new(), &excludes)
                .expect_err("前提：串行收集器在根不存在时出错");
        let (result, _) = collect_with(
            &missing,
            &HashSet::new(),
            &excludes,
            eager(),
            Arc::new(RealFs),
            pool(3),
        );
        let parallel_error = result.expect_err("并行扫描也必须出错");
        assert_eq!(parallel_error.to_string(), serial_error.to_string());
    }

    /// 默认上界下，目录数不到门限的树一次都不切。
    #[test]
    fn a_small_tree_is_never_split() {
        let tree = TempTree::new("scan-parallel-small");
        for index in 0..50 {
            tree.file(&format!("small/{index}/f.txt"), "x");
        }

        let excludes = ExcludeSet::default();
        let root = temp_root(&tree);
        let (result, cuts) = collect_with(
            &root,
            &HashSet::new(),
            &excludes,
            ScanLimits::default(),
            Arc::new(RealFs),
            pool(3),
        );

        assert_eq!(cuts, 0, "小树不能被切开");
        let (files, dirs, size) = result.expect("收集必须成功");
        assert_same(
            serial(&root, &HashSet::new(), &excludes),
            (files, dirs, size),
        );
    }

    /// 越过默认门限之后默认口径才真的切作业——这条把产品参数本身钉住，包括「门限以下是纯
    /// 串行下降」这个对外承诺。
    #[test]
    fn a_large_tree_is_split_with_the_default_limits() {
        let tree = TempTree::new("scan-parallel-large");
        let excludes = ExcludeSet::default();
        let skip_keys = HashSet::new();

        for group in 0..9 {
            for leaf in 0..100 {
                tree.dir(&format!("below/g{group:02}/d{leaf:02}"));
            }
        }
        let below = temp_root(&tree);
        let (result, cuts) = collect_with(
            &below,
            &skip_keys,
            &excludes,
            ScanLimits::default(),
            Arc::new(RealFs),
            pool(3),
        );
        assert_eq!(cuts, 0, "门限以下的树必须保持串行");
        let (files, dirs, size) = result.expect("收集必须成功");
        assert_same(serial(&below, &skip_keys, &excludes), (files, dirs, size));

        for group in 9..90 {
            for leaf in 0..100 {
                tree.dir(&format!("below/g{group:02}/d{leaf:02}"));
            }
        }
        for group in (0..90).step_by(10) {
            tree.file(&format!("below/g{group:02}/d00/f.txt"), "x");
        }

        let root = temp_root(&tree);
        let (result, cuts) = collect_with(
            &root,
            &skip_keys,
            &excludes,
            ScanLimits::default(),
            Arc::new(RealFs),
            pool(3),
        );
        let (files, dirs, size) = result.expect("收集必须成功");

        assert!(cuts > 0, "越过门限的树必须被切开");
        assert_same(serial(&root, &skip_keys, &excludes), (files, dirs, size));
    }

    /// 线程预算：池线程 + 调用线程不超过 [`SCAN_THREAD_BUDGET`]，单核时干脆不建池。
    #[test]
    fn the_thread_budget_includes_the_calling_thread() {
        assert_eq!(pool_workers(1), 0, "单核：调用线程自己就是全部预算");
        assert_eq!(pool_workers(2), 1);
        assert_eq!(pool_workers(4), 3);
        assert_eq!(pool_workers(16), SCAN_THREAD_BUDGET - 1);
        for available in 1..64 {
            assert!(pool_workers(available) < SCAN_THREAD_BUDGET);
        }
    }

    // ---- 注入文件系统：这些性质在真文件系统上做不出确定的验证 ----

    /// 假表里的文件/链接记录。真 `EntryMeta` 存着 `io::Result`（不可 Clone），假状态要能反复
    /// 取值，所以这里只留「取不取得到」这个开关，取数时再造错误。
    #[derive(Clone, Copy)]
    struct FakeRecord {
        kind: EntryKind,
        size: u64,
    }

    #[derive(Default)]
    struct FakeState {
        /// 目录 → 条目（名字，类型），顺序就是枚举顺序（建表时按名字排序，确定）。
        dirs: HashMap<String, Vec<(String, EntryKind)>>,
        /// 文件/链接 → 记录。目录不在这张表里。
        meta: HashMap<String, FakeRecord>,
        /// 这些目录打不开。
        unreadable: HashSet<String>,
        /// 这些目录打得开，但枚举时报错。
        unreadable_entries: HashSet<String>,
        /// 打开这些目录时 panic。
        panicking: HashSet<String>,
        /// 这些路径的元数据读不出来（条目不带元数据时，取元数据是**作业自己**的动作）。
        meta_fails: HashSet<String>,
        /// 这些路径的**修改时间**读不出来：元数据本身取得到，`modified()` 才失败。
        modified_fails: HashSet<String>,
        /// 记录成符号链接的路径 → 它「目标」的类型（`metadata()` 那一侧的答案）。缺省即目标不是
        /// 目录；用例用它钉根链接的两种下场。
        link_targets: HashMap<String, EntryKind>,
        /// 这些链接的**目标**元数据取不到修改时间——链接自己的 lstat 不受影响。
        target_modified_fails: HashSet<String>,
        /// 条目一律不带元数据：Unix 的 `read_dir` 不给元数据，取元数据这一步由拿到条目的作业
        /// 自己 stat——于是它发生在 worker 线程上，能让「谁先失败」被钉成确定顺序。
        opaque_meta: bool,
        /// 已经发生过的操作标识。
        happened: HashSet<String>,
        /// 操作标识 → 它要等的那件事。
        gates: HashMap<String, String>,
        /// 每个目录的 `read_dir` 被调用过几次——重开目录会在这里现形。
        opened: HashMap<String, usize>,
        open_now: usize,
        open_peak: usize,
    }

    /// 按假表造一份元数据。`modified` 的失败在这里才变成错误——与真实现同一时机（推迟到
    /// `push_file`）。
    fn fake_meta(state: &FakeState, key: &str, kind: EntryKind, size: u64) -> EntryMeta {
        EntryMeta {
            kind,
            size,
            modified: if state.modified_fails.contains(key) {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            } else {
                Ok(SystemTime::UNIX_EPOCH)
            },
        }
    }

    /// 一棵用表描述的假文件系统。它能按路径让 `read_dir` 失败、让枚举失败、让打开或取元数据
    /// panic；数每个目录被打开了几次、同时开着几个句柄；还能让一次操作**先等另一次操作发生
    /// 过**——把「谁先失败」「谁被挡在半路」钉成确定的顺序，而不是靠赛跑。
    #[derive(Default)]
    struct FakeFs {
        state: Arc<Mutex<FakeState>>,
    }

    /// 假路径一律按组件拼接造，免得字面量里的 `/` 与平台分隔符在字符串键上对不上。
    fn platform(path: &str) -> PathBuf {
        PathBuf::from_iter(path.split('/'))
    }

    fn key_of(path: &str) -> String {
        platform(path).display().to_string()
    }

    fn signal(state: &Arc<Mutex<FakeState>>, key: &str) {
        lock(state).happened.insert(key.to_owned());
    }

    fn wait_for(state: &Arc<Mutex<FakeState>>, key: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if lock(state).happened.contains(key) {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("等不到 {key}：这条用例要求它由另一个 worker 触发");
    }

    /// 走到某个操作标识时，先看它要不要等别人。
    fn gate(state: &Arc<Mutex<FakeState>>, key: &str) {
        let needed = lock(state).gates.get(key).cloned();
        if let Some(needed) = needed {
            wait_for(state, &needed);
        }
    }

    impl FakeFs {
        /// `entries` 是 (相对路径, 类型) 的扁平列表：缺的父目录自动补出来，每个目录的条目按名字
        /// 排序，枚举顺序因此是确定的。
        fn new(entries: &[(&str, EntryKind)]) -> Arc<FakeFs> {
            Self::build(entries)
        }

        fn build(entries: &[(&str, EntryKind)]) -> Arc<FakeFs> {
            let mut state = FakeState::default();
            for (path, kind) in entries {
                let full = platform(path);
                if *kind == EntryKind::Dir {
                    state.dirs.entry(full.display().to_string()).or_default();
                } else {
                    state.meta.insert(
                        full.display().to_string(),
                        FakeRecord {
                            kind: *kind,
                            size: path.len() as u64,
                        },
                    );
                }
                // 把这条路径挂进它的直接父目录，再把每一层父目录补成目录节点。
                let mut child = full;
                while let Some(parent) = child.parent() {
                    let parent_key = parent.display().to_string();
                    if parent_key.is_empty() {
                        break;
                    }
                    let name = child
                        .file_name()
                        .expect("非空路径一定有文件名")
                        .to_string_lossy()
                        .to_string();
                    let child_kind = match state.meta.get(&child.display().to_string()) {
                        Some(meta) => meta.kind,
                        None => EntryKind::Dir,
                    };
                    let children = state.dirs.entry(parent_key).or_default();
                    if !children.iter().any(|(existing, _)| existing == &name) {
                        children.push((name, child_kind));
                    }
                    child = parent.to_path_buf();
                }
            }
            for children in state.dirs.values_mut() {
                children.sort_by(|a, b| a.0.cmp(&b.0));
            }
            Arc::new(FakeFs {
                state: Arc::new(Mutex::new(state)),
            })
        }

        fn with(self: &Arc<Self>, edit: impl FnOnce(&mut FakeState)) -> Arc<FakeFs> {
            edit(&mut lock(&self.state));
            Arc::clone(self)
        }

        fn opened_times(&self, path: &str) -> usize {
            lock(&self.state)
                .opened
                .get(&key_of(path))
                .copied()
                .unwrap_or(0)
        }

        fn opened(&self, path: &str) -> bool {
            lock(&self.state).opened.contains_key(&key_of(path))
        }

        fn open_peak(&self) -> usize {
            lock(&self.state).open_peak
        }
    }

    /// 假目录流：条目枚举到一半也能出错或 panic；析构时归还句柄名额，峰值并发打开数才数得准。
    struct FakeEntries {
        state: Arc<Mutex<FakeState>>,
        key: String,
        items: std::vec::IntoIter<Result<Entry>>,
        started: bool,
        fails: bool,
        panics: bool,
        held: bool,
    }

    impl Iterator for FakeEntries {
        type Item = Result<Entry>;

        fn next(&mut self) -> Option<Result<Entry>> {
            if !self.started {
                self.started = true;
                signal(&self.state, &format!("entry:{}", self.key));
                gate(&self.state, &format!("entry:{}", self.key));
                // panic 在**枚举**里，也就是在拿到这个帧的那个作业自己的线程上。
                if self.panics {
                    panic!("injected panic while enumerating {}", self.key);
                }
                if self.fails {
                    signal(&self.state, &format!("fail:{}", self.key));
                    return Some(Err(path_error(
                        Path::new(&self.key),
                        io::Error::from(io::ErrorKind::PermissionDenied),
                    )));
                }
            }
            self.items.next()
        }
    }

    impl Drop for FakeEntries {
        fn drop(&mut self) {
            if self.held {
                lock(&self.state).open_now -= 1;
            }
        }
    }

    impl ScanFs for FakeFs {
        type Entries = FakeEntries;

        fn symlink_metadata(&self, path: &Path) -> Result<EntryMeta> {
            let key = path.display().to_string();
            // 取元数据是「哪个作业拿到这个条目」的动作，用它当观测点就能把失败顺序钉死。
            signal(&self.state, &format!("meta:{key}"));
            gate(&self.state, &format!("meta:{key}"));
            let state = lock(&self.state);
            if state.meta_fails.contains(&key) {
                drop(state);
                signal(&self.state, &format!("failmeta:{key}"));
                return Err(path_error(
                    path,
                    io::Error::from(io::ErrorKind::PermissionDenied),
                ));
            }
            if let Some(record) = state.meta.get(&key).copied() {
                return Ok(fake_meta(&state, &key, record.kind, record.size));
            }
            if state.dirs.contains_key(&key) {
                return Ok(fake_meta(&state, &key, EntryKind::Dir, 0));
            }
            Err(path_error(path, io::Error::from(io::ErrorKind::NotFound)))
        }

        fn metadata(&self, path: &Path) -> Result<EntryMeta> {
            let key = path.display().to_string();
            // 声明过目标的链接：这里返回的是**另一份**元数据（只由 `link_targets` 决定类型），
            // 于是「链接自己的 lstat」与「目标的 stat」可以分开注入。其余情况就是它自己那条记录。
            let (target, modified_fails) = {
                let state = lock(&self.state);
                (
                    state.link_targets.get(&key).copied(),
                    state.target_modified_fails.contains(&key),
                )
            };
            let Some(kind) = target else {
                return self.symlink_metadata(path);
            };
            Ok(EntryMeta {
                kind,
                size: 0,
                modified: if modified_fails {
                    Err(io::Error::from(io::ErrorKind::PermissionDenied))
                } else {
                    Ok(SystemTime::UNIX_EPOCH)
                },
            })
        }

        fn read_dir(&self, path: &Path) -> Result<FakeEntries> {
            let key = path.display().to_string();
            *lock(&self.state).opened.entry(key.clone()).or_default() += 1;
            if lock(&self.state).unreadable.contains(&key) {
                return Err(path_error(
                    path,
                    io::Error::from(io::ErrorKind::PermissionDenied),
                ));
            }

            let (children, fails, panics, opaque) = {
                let state = lock(&self.state);
                (
                    state.dirs.get(&key).cloned().unwrap_or_default(),
                    state.unreadable_entries.contains(&key),
                    state.panicking.contains(&key),
                    state.opaque_meta,
                )
            };

            let items: Vec<Result<Entry>> = children
                .into_iter()
                .map(|(name, kind)| {
                    let child = path.join(&name);
                    // 条目自带的元数据与 `symlink_metadata` 走同一份假表：`modified` 的失败也是
                    // 同一处注入，两条路都验得到。
                    let meta = (!opaque).then(|| {
                        let state = lock(&self.state);
                        let key = child.display().to_string();
                        state
                            .meta
                            .get(&key)
                            .copied()
                            .map(|record| fake_meta(&state, &key, record.kind, record.size))
                    });
                    Ok(Entry {
                        path: child,
                        kind,
                        meta: meta.flatten(),
                    })
                })
                .collect();

            let mut state = lock(&self.state);
            state.open_now += 1;
            state.open_peak = state.open_peak.max(state.open_now);
            drop(state);

            Ok(FakeEntries {
                state: Arc::clone(&self.state),
                key,
                items: items.into_iter(),
                started: false,
                fails,
                panics,
                held: true,
            })
        }
    }

    /// 用假文件系统跑一轮，返回（排序后的文件路径表 + 目录数 + 字节数，切点数）。
    fn fake_scan(
        fs: &Arc<FakeFs>,
        limits: ScanLimits,
        threads: usize,
    ) -> (Result<(Vec<String>, usize, u64)>, usize) {
        let (result, cuts) = collect_with(
            "root",
            &HashSet::new(),
            &ExcludeSet::default(),
            limits,
            Arc::clone(fs),
            pool(threads),
        );
        (
            result.map(|(files, dirs, size)| {
                let mut paths: Vec<String> = files.into_iter().map(|file| file.path).collect();
                paths.sort();
                (paths, dirs, size)
            }),
            cuts,
        )
    }

    /// **目录只被打开一次**：切出去的子作业拿到的是父作业那次打开的枚举结果，不重开。这条用例是
    /// 「错误洗不掉、TOCTOU 不存在」两个结论的观测点——重开一次就会在调用计数里留下第二个 1。
    #[test]
    fn every_directory_is_opened_exactly_once() {
        let fs = FakeFs::new(&[
            ("root/a.txt", EntryKind::File),
            ("root/one/b.txt", EntryKind::File),
            ("root/one/two/c.txt", EntryKind::File),
            ("root/three/d.txt", EntryKind::File),
            ("root/three/four/e.txt", EntryKind::File),
        ]);

        let (result, cuts) = fake_scan(&fs, eager(), 2);
        assert!(cuts > 0, "这棵注入的树必须真的被切开");
        let (files, dirs, _) = result.expect("扫描必须成功");
        assert_eq!(files.len(), 5, "五个文件都要收到");
        assert_eq!(dirs, 5, "root / one / two / three / four 共五个目录");

        let state = lock(&fs.state);
        assert!(!state.opened.is_empty());
        let mut opened: Vec<(&String, &usize)> = state.opened.iter().collect();
        opened.sort_by(|a, b| a.0.cmp(b.0));
        for (path, times) in opened {
            assert_eq!(
                *times, 1,
                "{path} 只能被打开一次（重开正是把错误洗掉的那条路）"
            );
        }
        for path in state.dirs.keys() {
            assert_eq!(
                state.opened.get(path).copied().unwrap_or(0),
                1,
                "{path} 在树里，却没有被恰好打开一次"
            );
        }
    }

    /// 首错按 DFS 位置裁决：更晚的子树先失败（用等待把它钉成确定顺序），答案仍必须是更早那个。
    ///
    /// 失败点选在**取元数据**上而不是打开目录上：条目不带元数据（Unix 的 `read_dir` 形态），
    /// 取元数据的正是拿到这个条目的那个子作业——于是两个子作业真的在各自线程上、以相反的先后
    /// 失败，「先被观察到的错」与「DFS 更早的错」不再是一回事。
    #[test]
    fn the_first_error_follows_traversal_order_not_completion_order() {
        let early = key_of("root/early/a.txt");
        let late = key_of("root/late/b.txt");
        let fs = FakeFs::new(&[
            ("root/early/a.txt", EntryKind::File),
            ("root/late/b.txt", EntryKind::File),
        ])
        .with(|state| {
            state.opaque_meta = true;
            state.meta_fails.insert(early.clone());
            state.meta_fails.insert(late.clone());
            // DFS 顺序：early 在前。它要先等 late 失败过才轮到自己失败——于是「先被观察到的
            // 错误」是 late 的，而正确答案是 early 的。
            state
                .gates
                .insert(format!("meta:{early}"), format!("failmeta:{late}"));
        });

        let (result, cuts) = fake_scan(&fs, eager(), 3);
        assert!(cuts >= 2, "两棵子树都要被切出去");

        let error = result.expect_err("两棵子树都失败").to_string();
        assert!(
            error.contains("early"),
            "DFS 更早的子树报的错必须胜出，哪怕更晚那个先失败；实际是 {error}"
        );
        assert!(
            lock(&fs.state)
                .happened
                .contains(&format!("failmeta:{late}")),
            "前提：更晚的子树的确先失败过"
        );
    }

    /// 被排除的目录就算打不开也不算错：串行实现里 walkdir 会先打开、随后把这次打开连同它的
    /// 延迟错误一起丢掉，所以并行实现连打开都不打开。
    #[test]
    fn a_skipped_directory_that_cannot_be_opened_does_not_fail_the_scan() {
        let fs = FakeFs::new(&[
            ("root/keep/a.txt", EntryKind::File),
            ("root/vendor/secret.txt", EntryKind::File),
        ]);
        fs.with(|state| {
            state.unreadable.insert(key_of("root/vendor"));
        });
        let excludes = ExcludeSet::from_dir_keys(&[&local_path_key(&key_of("root/vendor"))]);

        let (result, _) = collect_with(
            "root",
            &HashSet::new(),
            &excludes,
            eager(),
            Arc::clone(&fs),
            pool(2),
        );
        let (files, _, _) = result.expect("被排除的目录不能把整轮扫描搞失败");

        assert_eq!(files.len(), 1);
        assert_eq!(fs.opened_times("root/vendor"), 0, "它连打开都不该被打");
    }

    // ---- 修改时间的取数时机（B）：只有**真要收**的文件才会读到 `modified()` ----

    /// 被排除的文件、被排除的根、目录与特殊文件，都不该读到修改时间：这些的修改时间在假表里全是
    /// 「读不出来」，整轮仍然必须成功。串行实现里 `meta.modified()?` 排在排除判断与文件类型
    /// 判断**之后**，这里对齐的就是那个顺序。
    #[test]
    fn nothing_reads_the_modification_time_of_an_entry_it_does_not_collect() {
        let fs = FakeFs::new(&[
            ("root/keep/a.txt", EntryKind::File),
            ("root/vendor/skip.txt", EntryKind::File),
            ("root/empty/inner.txt", EntryKind::File),
            ("root/fifo", EntryKind::Other),
        ])
        .with(|state| {
            state.modified_fails.insert(key_of("root/vendor/skip.txt"));
            state.modified_fails.insert(key_of("root/vendor"));
            state.modified_fails.insert(key_of("root/empty"));
            state.modified_fails.insert(key_of("root/fifo"));
        });
        let excludes = ExcludeSet::from_dir_keys(&[&local_path_key(&key_of("root/vendor"))]);

        let (result, _) = collect_with(
            "root",
            &HashSet::new(),
            &excludes,
            eager(),
            Arc::clone(&fs),
            pool(2),
        );
        let (files, dirs, _) = result.expect("这些路径的修改时间根本不该被读，更不该因此失败");
        assert_eq!(files.len(), 2, "keep/a.txt 与 empty/inner.txt 都要收到");
        assert_eq!(
            dirs, 3,
            "root / keep / empty 三个目录（被排除的 vendor 不计）"
        );

        // 根自己也算一个：被排除的根只需要 lstat 出来的类型，修改时间不该被碰。
        let excluded_root = FakeFs::new(&[("root/a.txt", EntryKind::File)]).with(|state| {
            state.modified_fails.insert(key_of("root"));
        });
        let excludes = ExcludeSet::from_dir_keys(&[&local_path_key(&key_of("root"))]);
        let (result, _) = collect_with(
            "root",
            &HashSet::new(),
            &excludes,
            eager(),
            Arc::clone(&excluded_root),
            pool(2),
        );
        let (files, dirs, _) = result.expect("被排除的根不该因为修改时间读不出来而失败");
        assert!(files.is_empty());
        assert_eq!(dirs, 0);
        assert_eq!(
            excluded_root.opened_times("root"),
            0,
            "被排除的根连打开都不该被打"
        );
    }

    /// 真要收的文件读不出修改时间：整轮失败，且文案与串行实现一致——**裸 `io::Error`，不带路径**
    /// （串行里紧随 `entry.metadata()?` 的 `meta.modified()?` 就是这么冒出来的）。
    #[test]
    fn a_collected_file_without_a_modification_time_fails_with_a_bare_error() {
        let failing = key_of("root/bad.txt");
        let fs = FakeFs::new(&[
            ("root/bad.txt", EntryKind::File),
            ("root/ok.txt", EntryKind::File),
        ])
        .with(|state| {
            state.modified_fails.insert(failing);
        });

        let (result, _) = fake_scan(&fs, eager(), 2);
        let error = result
            .expect_err("收集到的文件读不出修改时间，必须整轮失败")
            .to_string();
        assert_eq!(
            error,
            io::Error::from(io::ErrorKind::PermissionDenied).to_string(),
            "文案必须是不带路径的裸 io::Error"
        );
        assert!(!error.contains("bad.txt"), "这条错误不带路径");
    }

    /// 根链接只用到**目标元数据的类型**：目标的修改时间读不出来，仍然要既收下链接本身、
    /// 又跟进去（walkdir 的 `follow_root_links`）。
    #[test]
    fn a_root_link_only_needs_the_target_kind() {
        let fs = FakeFs::new(&[
            ("root", EntryKind::Symlink),
            ("root/inner.txt", EntryKind::File),
        ])
        .with(|state| {
            state.link_targets.insert(key_of("root"), EntryKind::Dir);
            state.target_modified_fails.insert(key_of("root"));
        });

        let (result, _) = fake_scan(&fs, eager(), 2);
        let (files, _, _) = result.expect("目标读不出修改时间不该拦住根链接");
        let root = key_of("root");
        assert!(files.contains(&root), "根链接本身要当文件收下");
        assert!(
            files.iter().any(|path| path.ends_with("inner.txt")),
            "还要跟进去（只用到目标的类型）"
        );
        assert_eq!(files.len(), 2);
    }

    /// 出过错之后不再切新作业：更晚的工作停止扩张，也不再消耗切点预算。
    #[test]
    fn an_error_stops_further_splitting() {
        let scan = Arc::new(Scan::new(
            Arc::new(FakeFs::default()),
            pool(2),
            &HashSet::new(),
            &ExcludeSet::default(),
            eager(),
        ));

        let (_, slot) = scan.delegate(1).expect("出错之前必须还能切作业");
        assert_eq!(scan.cuts.load(Ordering::Relaxed), 1);
        drop(slot);

        scan.failed.store(true, Ordering::Relaxed);
        assert!(scan.delegate(1).is_none(), "出过错的扫描不能再发放作业");
        assert_eq!(scan.cuts.load(Ordering::Relaxed), 1, "也不能再消耗切点预算");
    }

    /// 句柄上限：同一时刻开着的目录不超过 [`MAX_OPEN_DIRS`]。
    #[test]
    fn no_more_than_the_handle_budget_is_open_at_once() {
        let mut chain = String::from("root");
        let mut entries = Vec::new();
        for level in 0..30 {
            chain.push_str(&format!("/d{level}"));
            entries.push((chain.clone(), EntryKind::Dir));
        }
        entries.push((format!("{chain}/leaf.txt"), EntryKind::File));
        let flat: Vec<(&str, EntryKind)> = entries
            .iter()
            .map(|(path, kind)| (path.as_str(), *kind))
            .collect();
        let fs = FakeFs::new(&flat);

        let (result, cuts) = fake_scan(&fs, ScanLimits::default(), 1);
        result.expect("这条深链必须被收完");
        assert_eq!(cuts, 0, "31 个目录远在默认门限之下");
        assert!(
            fs.open_peak() <= MAX_OPEN_DIRS,
            "同时开着的句柄不能超过 {MAX_OPEN_DIRS}，峰值是 {}",
            fs.open_peak()
        );
    }

    /// **切作业那一步也要先腾名额**：切出去的目录当场被打开再关掉，这个 `read_dir` 同样是一个
    /// 新开的句柄，不先 `make_room` 就会在栈满（10 个开着的帧）时开到第 11 个。
    ///
    /// 形状要挑得让**两个作业不重叠持有句柄**，否则进程级峰值里就没法把这次的句柄单独认出来：
    /// 深链把父作业的栈压满、第一个切点落在栈满之后，而被切走的那棵子树里只有文件——子作业一个
    /// 目录都不打开。于是峰值就等于父作业自己的峰值：正好 `MAX_OPEN_DIRS`（少一次 `make_room`
    /// 则是它加一）。
    #[test]
    fn a_cut_while_the_stack_is_full_still_respects_the_handle_budget() {
        let mut chain = String::from("root");
        let mut entries = Vec::new();
        for level in 0..10 {
            chain.push_str(&format!("/d{level:02}"));
            entries.push((chain.clone(), EntryKind::Dir));
            entries.push((format!("{chain}/leaf.txt"), EntryKind::File));
        }
        let cut = format!("{chain}/d10");
        entries.push((cut.clone(), EntryKind::Dir));
        for file in 0..4 {
            entries.push((format!("{cut}/f{file}.txt"), EntryKind::File));
        }
        let flat: Vec<(&str, EntryKind)> = entries
            .iter()
            .map(|(path, kind)| (path.as_str(), *kind))
            .collect();
        let fs = FakeFs::new(&flat);

        // `d10` 是这条链上的第 12 个目录（`root` 算第 1 个），那时父作业的栈已经满了。
        let limits = ScanLimits {
            min_dirs: 12,
            max_cuts: usize::MAX,
        };
        let (result, cuts) = fake_scan(&fs, limits, 2);
        let (files, dirs, _) = result.expect("这条深链必须被收完");
        assert_eq!(cuts, 1, "前提：切点真的落在 `d10` 上，且此后再无目录可切");
        assert_eq!(
            files.len(),
            14,
            "d00..d09 各一个 leaf.txt，加上 d10 里那 4 个"
        );
        assert_eq!(dirs, 12, "root + d00..d10");
        assert_eq!(
            fs.open_peak(),
            MAX_OPEN_DIRS,
            "前提：栈真的被压满了，切点才会落在这条路径上"
        );
    }

    /// **排队中的作业不占目录句柄**：切点有界只约束了在途内存，真按 fd 算，跑得比 worker 快的
    /// 父作业能把整棵树的目录都开着排队，小 fd 预算的机器就直接 EMFILE。
    ///
    /// 这里把唯一的 worker 挡在它第一个作业的取元数据那一步，父作业于是必须独自把 30 个子目录
    /// 全部切出去排队——这时开着的目录应当只有父作业自己的栈（交接给池的帧已经关了句柄）。
    #[test]
    fn a_queued_job_does_not_hold_a_directory_handle() {
        let entries: Vec<String> = (0..30)
            .map(|index| format!("root/d{index:02}/f.txt"))
            .collect();
        let flat: Vec<(&str, EntryKind)> = entries
            .iter()
            .map(|path| (path.as_str(), EntryKind::File))
            .collect();
        let held = key_of("root/d00/f.txt");
        let fs = FakeFs::new(&flat).with(|state| {
            state.opaque_meta = true;
            // 谁也别放行，除非用例自己 signal 这个键：worker 就停在第一个作业上。
            state
                .gates
                .insert(format!("meta:{held}"), format!("release:{held}"));
        });

        let worker_fs = Arc::clone(&fs);
        let scan = std::thread::spawn(move || fake_scan(&worker_fs, eager(), 1));

        // 父作业走到最后一个子目录，就说明它该切的全切出去了。
        let deadline = Instant::now() + Duration::from_secs(10);
        while !fs.opened("root/d29") {
            assert!(
                Instant::now() < deadline,
                "父作业没走到最后一个目录，用例前提不成立"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let peak = fs.open_peak();
        let queued = 30;

        signal(&fs.state, &format!("release:{held}"));

        let (result, cuts) = scan.join().expect("扫描线程不该 panic");
        result.expect("扫描必须成功");
        assert_eq!(cuts, queued, "前提：30 个子目录全被切出去排队");

        assert!(
            peak <= MAX_OPEN_DIRS,
            "队列里的作业不能再占目录句柄：{queued} 个作业排队时开着的目录只剩父作业自己的栈，\
             峰值最多 {MAX_OPEN_DIRS}，实际是 {peak}"
        );
    }

    /// worker panic 既不能 abort 也不能悬挂：它变成一个普通的作业失败。
    #[test]
    fn a_panicking_worker_becomes_an_error_instead_of_hanging() {
        // panic 落在子作业**自己**打开的那个目录上：交给池的帧是父作业打开并关掉的，
        // 在那里 panic 会炸掉调用线程，不是这条用例要验的东西。
        let fs = FakeFs::new(&[
            ("root/boom/sub/a.txt", EntryKind::File),
            ("root/ok/b.txt", EntryKind::File),
        ])
        .with(|state| {
            state.panicking.insert(key_of("root/boom/sub"));
        });

        let (result, _) = fake_scan(&fs, eager(), 2);
        let error = result.expect_err("panic 必须变成一个错误").to_string();
        assert!(error.contains("panic"), "实际是 {error}");
    }

    // ---- 合并规则本身的单元测试：不依赖文件系统，逐条钉住拼接与首错 ----

    fn file(path: &str) -> WorkspaceFile {
        WorkspaceFile {
            path_lower: local_path_key(path),
            path: path.to_owned(),
            size: 1,
            date: SystemTime::UNIX_EPOCH,
            filtered: false,
        }
    }

    /// 等键用的构造：手工把比较键设成同一个，**不走** `local_path_key`——那条路在 Unix 上不折
    /// 大小写，`Tie.txt` 与 `tie.txt` 根本不是同一个键，拿它造等键在 Unix 上是个假前提。
    fn tied(path: &str, key: &str) -> WorkspaceFile {
        WorkspaceFile {
            path_lower: key.to_owned(),
            path: path.to_owned(),
            size: 1,
            date: SystemTime::UNIX_EPOCH,
            filtered: false,
        }
    }

    fn outcome(files: &[&str], num_dirs: usize) -> JobOutcome {
        JobOutcome {
            files: files.iter().map(|path| file(path)).collect(),
            num_dirs,
            total_size: files.len() as u64,
            own_error: None,
            splices: Vec::new(),
        }
    }

    fn slot_of(outcome: JobOutcome) -> Slot {
        Arc::new(Mutex::new(Some(outcome)))
    }

    /// 没切过作业时根作业的表就是结果：不另开容器，也不把元素搬一遍。
    #[test]
    fn merge_returns_the_root_table_as_is_when_nothing_was_split() {
        let root = outcome(&["a/1.txt", "a/2.txt"], 3);
        let original = root.files.as_ptr();
        let capacity = root.files.capacity();

        let (files, num_dirs, total_size, error) = merge(root, 0);

        assert_eq!(files.as_ptr(), original, "结果表不能被复制");
        assert_eq!(files.capacity(), capacity);
        assert_eq!(num_dirs, 3);
        assert_eq!(total_size, 2);
        assert!(error.is_none());
    }

    /// 子作业整段插在父作业「切出去的那个位置」上：父作业切点前后的文件、以及多个切点之间的
    /// 相对次序都不能乱。
    #[test]
    fn merge_interleaves_children_at_their_insert_positions() {
        let first = slot_of(outcome(&["a/1.txt", "a/2.txt"], 1));
        let second = slot_of(outcome(&["b/1.txt"], 2));

        let mut root = outcome(&["root-before.txt", "root-middle.txt", "root-after.txt"], 1);
        root.splices.push(Splice {
            at: 0,
            child: Arc::clone(&first),
        });
        root.splices.push(Splice {
            at: 2,
            child: Arc::clone(&second),
        });

        let (files, num_dirs, total_size, error) = merge(root, 2);
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();

        assert_eq!(
            paths,
            vec![
                "a/1.txt",
                "a/2.txt",
                "root-before.txt",
                "root-middle.txt",
                "b/1.txt",
                "root-after.txt",
            ]
        );
        assert_eq!(num_dirs, 1 + 1 + 2);
        assert_eq!(total_size, 3 + 2 + 1);
        assert_eq!(
            files.capacity(),
            6,
            "目的表必须按最终大小一次建出来，不留增长余量"
        );
        assert!(error.is_none());
    }

    /// 首错按 DFS 位置裁决：更早切出去的子作业的错误胜过父作业自己的、也胜过更晚的子作业。
    #[test]
    fn merge_picks_the_earliest_error_in_traversal_order() {
        let late = slot_of(JobOutcome {
            own_error: Some(anyhow!("late child failed")),
            ..outcome(&["late/1.txt"], 1)
        });
        let mut root = outcome(&["before.txt", "middle.txt", "after.txt"], 1);
        root.own_error = Some(anyhow!("parent failed after splitting"));
        root.splices.push(Splice {
            at: 2,
            child: Arc::clone(&late),
        });

        let (_, _, _, error) = merge(root, 1);
        assert_eq!(
            error.expect("错误必须活过归并").to_string(),
            "late child failed",
            "DFS 更早切出去的那个子作业的错误胜过父作业自己的"
        );

        let early = slot_of(outcome(&["early/1.txt"], 1));
        let mut root = outcome(&["before.txt", "after.txt"], 1);
        root.own_error = Some(anyhow!("parent failed after splitting"));
        root.splices.push(Splice {
            at: 1,
            child: Arc::clone(&early),
        });

        let (_, _, _, error) = merge(root, 1);
        assert_eq!(
            error.expect("错误必须活过归并").to_string(),
            "parent failed after splitting"
        );
    }

    /// 更早的子作业出错时，更晚的错误一律不采用（哪怕是父作业自己的）。
    #[test]
    fn merge_never_prefers_a_later_error() {
        let first = slot_of(JobOutcome {
            own_error: Some(anyhow!("first child failed")),
            ..outcome(&[], 1)
        });
        let second = slot_of(JobOutcome {
            own_error: Some(anyhow!("second child failed")),
            ..outcome(&[], 1)
        });

        let mut root = outcome(&["root.txt"], 1);
        root.own_error = Some(anyhow!("parent failed"));
        root.splices.push(Splice {
            at: 0,
            child: Arc::clone(&first),
        });
        root.splices.push(Splice {
            at: 1,
            child: Arc::clone(&second),
        });

        let (_, _, _, error) = merge(root, 2);
        assert_eq!(
            error.expect("错误必须活过归并").to_string(),
            "first child failed"
        );
    }

    /// 等键（比较键相同）的相对次序跟着 DFS 走：拼接完再做稳定排序，与串行同序。
    /// `WorkspaceState::build_mapping` 是后者胜，所以这条相对次序是**结果**的一部分。
    #[test]
    fn merge_keeps_tied_keys_in_traversal_order() {
        let key = "tie.txt";
        let left = "Tie.txt";
        let right = "tie.txt";

        let child = slot_of(JobOutcome {
            files: vec![tied(right, key)],
            num_dirs: 1,
            total_size: 1,
            own_error: None,
            splices: Vec::new(),
        });
        let mut root = JobOutcome {
            files: vec![tied(left, key)],
            num_dirs: 1,
            total_size: 1,
            own_error: None,
            splices: Vec::new(),
        };
        root.splices.push(Splice {
            at: 1,
            child: Arc::clone(&child),
        });

        let (mut files, _, _, _) = merge(root, 1);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, left);
        assert_eq!(files[1].path, right);

        files.sort_by(|a, b| a.path_lower.cmp(&b.path_lower));
        assert_eq!(files[0].path, left, "稳定排序保住 DFS 的等键次序");
        assert_eq!(files[1].path, right);

        let mut state = crate::model::WorkspaceState {
            num_files: files.len(),
            files,
            ..Default::default()
        };
        state.build_mapping();
        assert_eq!(
            state.files[state.file_map[key]].path, right,
            "DFS 更晚的那个文件必须赢得映射"
        );
    }
}
