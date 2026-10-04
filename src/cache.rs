//! 摘要缓存的落盘。

use std::ffi::OsString;
use std::fs::{File, create_dir_all};
use std::io::{BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::READ_BUFFER_SIZE;
use crate::json::sayln;
use crate::model::WorkspaceCache;

/// 分阶段把摘要缓存落盘。
///
/// 每个摘要阶段成功后按阈值保存，后续失败仍可复用已落盘阶段的成果。
/// 阶段内部不做 checkpoint，未完成或尚未保存的摘要仍可能丢失。
pub(crate) struct CacheWriter {
    path: PathBuf,
    last_save: Instant,
    entries_at_last_save: usize,
}

/// 每批都存会让磁盘反复读写、得不偿失，所以这两个阈值任一触发才存一次。
pub(crate) const CACHE_SAVE_MIN_INTERVAL: Duration = Duration::from_secs(60);
pub(crate) const CACHE_SAVE_MIN_NEW_ENTRIES: usize = 100_000;

/// 临时文件名的进程内序号，见 [`unique_temp_path`]。
static TEMP_SEQ: AtomicUsize = AtomicUsize::new(0);

/// 临时文件名撞车时的重试次数。撞上的只可能是同 pid 的历史残留（pid 被复用），
/// 换几个序号足够绕开。
const TEMP_NAME_ATTEMPTS: usize = 8;

/// 与 `target` 同目录、本次调用独占的临时文件名：`<缓存名>.<pid>.<序号>.tmp`。
///
/// 同目录是为了 rename 不跨文件系统；pid 加序号是为了让两个 p4delta 进程（开两个 P4V
/// 窗口就够了）各写各的——固定名会让它们往同一个文件里写，谁后改名谁就把两份字节混着发布。
fn unique_temp_path(target: &Path) -> PathBuf {
    let sequence = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut name = target
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| OsString::from("p4delta-cache"));
    name.push(format!(".{}.{}.tmp", std::process::id(), sequence));
    target.with_file_name(name)
}

/// 一次保存用的临时文件。没走到改名成功就把自己删掉——编码失败、flush 失败、改名失败
/// 都算；改名成功后 `disarm` 交接。只删 `create_new` 建出来的那一份，撞上同名文件时
/// 既不截断也不删。
struct TempFile {
    path: PathBuf,
    published: bool,
}

impl TempFile {
    fn create(target: &Path) -> Result<(Self, File)> {
        for _ in 0..TEMP_NAME_ATTEMPTS {
            let path = unique_temp_path(target);
            match File::create_new(&path) {
                Ok(file) => {
                    return Ok((
                        TempFile {
                            path,
                            published: false,
                        },
                        file,
                    ));
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "Failed to create the temporary cache file {}",
                            path.display()
                        )
                    });
                }
            }
        }

        bail!(
            "Failed to find an unused temporary cache file name next to {}",
            target.display()
        )
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// 改名成功，这个路径已经是正式缓存，不再归 `Drop` 管。
    fn disarm(mut self) {
        self.published = true;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl CacheWriter {
    pub(crate) fn new(path: PathBuf) -> Self {
        CacheWriter {
            path,
            // 把时间回拨，好让第一次调用就能立即保存。
            last_save: Instant::now() - CACHE_SAVE_MIN_INTERVAL,
            entries_at_last_save: 0,
        }
    }

    /// 需要时写盘；`force` 跳过节流阈值。
    ///
    /// 取 `&mut` 是因为发布成功会改缓存的状态：文件里已经是这些摘要，`out_of_date` 随之
    /// 归 false。失败或没到阈值时原样保留——内存里那份仍是唯一的副本，下次调用再试。
    ///
    /// `force` 只跳过阈值，不跳过「有没有东西要写」：干净的缓存永远不重写，否则一次什么都
    /// 没算的运行也会动文件，看起来像算出了东西。
    pub(crate) fn maybe_save(&mut self, cache: &mut WorkspaceCache, force: bool) -> Result<()> {
        if !cache.out_of_date {
            return Ok(());
        }

        let new_entries = cache
            .file_map
            .len()
            .saturating_sub(self.entries_at_last_save);
        if !force
            && new_entries < CACHE_SAVE_MIN_NEW_ENTRIES
            && self.last_save.elapsed() < CACHE_SAVE_MIN_INTERVAL
        {
            return Ok(());
        }

        if let Some(prefix) = self.path.parent() {
            create_dir_all(prefix)?;
        }

        // 先写临时文件再改名：写一半崩掉不会留下解不开的缓存，两个 writer 抢同一个缓存
        // 也不会把字节交错在一起。
        let (temp, file) = TempFile::create(&self.path)?;
        let config = bincode::config::standard();
        {
            let mut writer = BufWriter::with_capacity(READ_BUFFER_SIZE, file);
            // `&mut` 是给下面的记账用的，编码只读这份缓存。
            bincode::encode_into_std_write(&*cache, &mut writer, config)
                .with_context(|| format!("Failed to encode the digest cache {:?}", self.path))?;
            writer
                .flush()
                .with_context(|| format!("Failed to write the digest cache {:?}", self.path))?;
        }

        // Windows 与 POSIX 的 `rename` 都能一步替换已存在的文件；换不掉就在原地报错，
        // 旧缓存原样留着，临时文件由 `Drop` 清掉。
        std::fs::rename(temp.path(), &self.path).with_context(|| {
            format!("Failed to replace the digest cache {}", self.path.display())
        })?;
        temp.disarm();

        // 已经落盘，不再过期。必须放在改名之后：提前清的话，发布失败会把内存里唯一的
        // 副本标成「已保存」，再也不会写。
        cache.out_of_date = false;
        self.last_save = Instant::now();
        self.entries_at_last_save = cache.file_map.len();

        sayln!(
            "    Saved {} cached digests to {}.",
            cache.file_map.len(),
            self.path.display()
        );

        Ok(())
    }
}

/// 能确定缓存位置时保存缓存。
pub(crate) fn save_cache(
    cache_writer: &mut Option<CacheWriter>,
    cache: &mut WorkspaceCache,
    force: bool,
) -> Result<()> {
    match cache_writer {
        Some(writer) => writer.maybe_save(cache, force),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::BufReader;
    use std::path::Path;
    use std::thread;
    use std::time::UNIX_EPOCH;

    use crate::model::WorkspaceCacheEntry;
    use crate::test_util::TempTree;

    fn cache_with(entries: usize, out_of_date: bool) -> WorkspaceCache {
        let mut cache = WorkspaceCache {
            out_of_date,
            ..Default::default()
        };
        for i in 0..entries {
            cache.file_map.insert(
                format!("c:\\file{}.txt", i),
                WorkspaceCacheEntry {
                    size: i as u64,
                    date: UNIX_EPOCH + Duration::from_secs(i as u64),
                    digest: [i as u8; 16],
                },
            );
        }
        cache
    }

    /// 每个用例一个独立的缓存路径，借 `TempTree`（析构即删目录）。名字里带上 pid 与序号：
    /// 两份并行跑的测试进程（比如同时开着的 libtest 与 nextest）不能撞到同一个目录上，
    /// 否则一方开头的 `remove_dir_all` 会删掉另一方正在写的文件。
    ///
    /// 返回的树必须活到用例结束：绑成 `_tree` 而不是 `_`。
    fn temp_cache_path() -> (TempTree, PathBuf) {
        let sequence = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tree = TempTree::new(&format!("cache-{}-{}", std::process::id(), sequence));
        // 再深一层：目标目录留给保存自己去建。
        let path = tree.root.join("nested").join("digests.bin");
        (tree, path)
    }

    /// 目录里的文件名，排好序，用来断言没有多余的临时产物。
    fn dir_entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn read_cache_back(path: &Path) -> WorkspaceCache {
        let mut file = BufReader::new(File::open(path).unwrap());
        bincode::decode_from_std_read(&mut file, bincode::config::standard()).unwrap()
    }

    /// `loaded` 是不是恰好等于 `cache_with(entries, ..)` 写出去的那一份。
    fn is_complete_save_of(loaded: &WorkspaceCache, entries: usize) -> bool {
        let expected = cache_with(entries, false);
        loaded.file_map.len() == expected.file_map.len()
            && expected.file_map.iter().all(|(key, value)| {
                loaded
                    .file_map
                    .get(key)
                    .is_some_and(|got| got.size == value.size && got.digest == value.digest)
            })
    }

    /// 先正常落一份缓存，返回写进去的字节，供「失败后原样还在」的断言比对。
    #[cfg(windows)]
    fn saved_cache(path: &Path) -> Vec<u8> {
        CacheWriter::new(path.to_owned())
            .maybe_save(&mut cache_with(3, true), true)
            .unwrap();
        std::fs::read(path).unwrap()
    }

    #[test]
    fn cache_writer_round_trips_a_forced_save() {
        let (_tree, path) = temp_cache_path();
        let mut cache = cache_with(3, true);

        CacheWriter::new(path.clone())
            .maybe_save(&mut cache, true)
            .unwrap();

        let loaded = read_cache_back(&path);
        assert_eq!(loaded.file_map.len(), 3);
        // 落进文件的是保存那一刻的内存镜像（那会儿还是 dirty）；真正作数的是内存里这一份，
        // 成功发布后它不再过期。
        assert!(loaded.out_of_date);
        assert!(
            !cache.out_of_date,
            "a published cache is no longer out of date"
        );
        // 中间文件必须清理干净：目录里只该剩下正式缓存。
        assert_eq!(dir_entries(path.parent().unwrap()), vec!["digests.bin"]);
    }

    #[test]
    fn cache_writer_overwrites_the_previous_save() {
        let (_tree, path) = temp_cache_path();
        let mut writer = CacheWriter::new(path.clone());

        writer.maybe_save(&mut cache_with(2, true), true).unwrap();
        writer.maybe_save(&mut cache_with(5, true), true).unwrap();

        assert_eq!(read_cache_back(&path).file_map.len(), 5);
    }

    #[test]
    fn cache_writer_skips_a_save_with_nothing_new() {
        let (_tree, path) = temp_cache_path();
        let mut cache = cache_with(1, true);
        let mut writer = CacheWriter::new(path.clone());

        writer.maybe_save(&mut cache, true).unwrap();
        // 把文件删掉：第二次调用若真写了什么，就必须把它重建出来。
        std::fs::remove_file(&path).unwrap();
        // 又脏了，但条目数与时钟都没动过：两个阈值都没触发，这次调用该被挡回去。
        cache.out_of_date = true;
        writer.maybe_save(&mut cache, false).unwrap();

        // 条目数与时钟都没越过各自的阈值，不值得为它付一次磁盘写。
        assert!(!path.exists());
    }

    /// 间隔到了就该落盘，哪怕没有新增条目——两个阈值是「任一触发」，不是「都要满足」。
    /// `cache_writer_skips_a_save_with_nothing_new` 覆盖的是时间未到的那一侧。
    #[test]
    fn cache_writer_saves_once_the_interval_has_passed() {
        let (_tree, path) = temp_cache_path();
        let mut cache = cache_with(1, true);

        // 直接构造并回拨 last_save：测试是子模块，够得着私有字段，不必注入时钟，
        // 也不用真等 60 秒。
        let mut writer = CacheWriter {
            path: path.clone(),
            last_save: Instant::now() - CACHE_SAVE_MIN_INTERVAL,
            entries_at_last_save: cache.file_map.len(),
        };

        writer.maybe_save(&mut cache, false).unwrap();

        assert!(path.exists());
        assert_eq!(read_cache_back(&path).file_map.len(), 1);
        assert!(!cache.out_of_date);
    }

    #[test]
    fn cache_writer_skips_an_up_to_date_cache() {
        let (_tree, path) = temp_cache_path();

        // 从磁盘加载、之后一条都没加过的缓存。存它只会把同样的字节重写一遍，
        // 而「什么都没算」的运行不能看起来像算过。
        CacheWriter::new(path.clone())
            .maybe_save(&mut cache_with(4, false), true)
            .unwrap();

        assert!(!path.exists());
    }

    /// `force` 只跳过阈值，不跳过「有没有东西要写」：刚保存过的缓存已经和磁盘一致，
    /// 再 `force` 一次也不该重写它。
    #[test]
    fn cache_writer_does_not_rewrite_a_clean_cache_even_when_forced() {
        let (_tree, path) = temp_cache_path();
        let mut cache = cache_with(3, true);
        let mut writer = CacheWriter::new(path.clone());

        writer.maybe_save(&mut cache, true).unwrap();
        assert!(!cache.out_of_date);

        // 哨兵：文件要是被重写，读回来的就是 bincode 而不是它。
        std::fs::write(&path, b"sentinel").unwrap();
        writer.maybe_save(&mut cache, true).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"sentinel");
        assert!(!cache.out_of_date);
    }

    /// 条目数没长也要写：同一个键换了摘要（文件被改过、重算过）同样得落盘。
    /// `entries_at_last_save` 只管阈值，不是「有没有变化」的判据。
    #[test]
    fn cache_writer_writes_an_entry_updated_in_place() {
        let (_tree, path) = temp_cache_path();
        let mut cache = cache_with(3, true);
        let mut writer = CacheWriter::new(path.clone());

        writer.maybe_save(&mut cache, true).unwrap();

        // 同一个键、新的摘要：map 的长度没变，变的只是值。
        let key = "c:\\file1.txt".to_owned();
        cache.file_map.get_mut(&key).unwrap().digest = [0xAB; 16];
        // 生产代码里这个标记由 digest.rs 在算完摘要后置上。
        cache.out_of_date = true;
        writer.maybe_save(&mut cache, true).unwrap();

        assert_eq!(read_cache_back(&path).file_map[&key].digest, [0xAB; 16]);
        assert!(!cache.out_of_date);
    }

    /// 保存失败时缓存必须保持 dirty：内存里的摘要是唯一的副本，下一次调用得再试一次，
    /// 不能因为「这次没写成」把它当成已经落盘。
    #[test]
    fn cache_writer_keeps_the_cache_dirty_when_the_save_fails() {
        let (_tree, path) = temp_cache_path();
        // 让目标位置先被一个目录占住：临时文件写得出来，改名那一步一定失败。
        std::fs::create_dir_all(&path).unwrap();
        let mut cache = cache_with(3, true);
        let mut writer = CacheWriter::new(path.clone());

        assert!(writer.maybe_save(&mut cache, true).is_err());

        assert!(
            cache.out_of_date,
            "a failed save must leave the cache out of date"
        );
        // 失败方只清理自己创建的临时文件：占位的那个目录、以及它自己，都不能动。
        assert_eq!(dir_entries(path.parent().unwrap()), vec!["digests.bin"]);
    }

    /// 改名失败时旧缓存必须原封不动。Windows 上若其他句柄不允许删除共享，rename 会失败；
    /// Rust 默认打开文件允许删除共享，此处特意模拟更严格的外部文件占用。
    #[cfg(windows)]
    #[test]
    fn cache_writer_keeps_the_old_cache_when_the_target_is_locked() {
        use std::os::windows::fs::OpenOptionsExt;

        let (_tree, path) = temp_cache_path();
        let before = saved_cache(&path);
        let mut cache = cache_with(5, true);
        let mut writer = CacheWriter::new(path.clone());

        // 只共享读：别的句柄删不掉它，`rename` 也就换不掉。
        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();

        assert!(writer.maybe_save(&mut cache, true).is_err());

        assert_eq!(std::fs::read(&path).unwrap(), before, "旧缓存不该被动过");
        assert!(cache.out_of_date, "失败的保存之后缓存仍是 dirty");
        // 临时文件照旧由这次保存自己清掉。
        assert_eq!(dir_entries(path.parent().unwrap()), vec!["digests.bin"]);
    }

    /// 只管自己创建的那一份：同一个目录里别人的文件（上一版留下的固定名、别的进程正在用的
    /// 临时文件）一个都不能动。
    #[test]
    fn cache_writer_leaves_files_it_does_not_own_alone() {
        let (_tree, path) = temp_cache_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // 旧版固定名留下的残留，和另一个进程按 `unique_temp_path` 命名的临时文件。
        let stale = path.with_extension("bin.tmp");
        std::fs::write(&stale, b"stale").unwrap();
        let other_writer = path.with_file_name("digests.bin.999999.7.tmp");
        std::fs::write(&other_writer, b"other").unwrap();

        CacheWriter::new(path.clone())
            .maybe_save(&mut cache_with(2, true), true)
            .unwrap();

        assert_eq!(std::fs::read(&stale).unwrap(), b"stale");
        assert_eq!(std::fs::read(&other_writer).unwrap(), b"other");
        assert!(path.exists());
    }

    /// 两个 writer 朝同一个缓存路径发布：盘上那份必须始终是完整的一份（能解码，条目数与
    /// 摘要都对得上其中一个 writer），不能是两份字节交错出来的东西。中间的失败不算错
    /// ——失败的那一方没有发布，盘上留下的仍是另一方的完整缓存——但每个 writer 都得
    /// 真的发布过，否则这个用例什么也没证明。
    #[test]
    fn cache_writer_publishes_a_complete_cache_when_two_writers_race() {
        let (_tree, path) = temp_cache_path();
        let saved = [AtomicUsize::new(0), AtomicUsize::new(0)];

        thread::scope(|scope| {
            for (slot, entries) in saved.iter().zip([2usize, 5]) {
                let path = path.clone();
                scope.spawn(move || {
                    let mut cache = cache_with(entries, true);
                    let mut writer = CacheWriter::new(path);
                    for _ in 0..64 {
                        // 每轮都标回 dirty：真实的运行里两次保存之间总在算新摘要，
                        // 否则第一次保存之后剩下的 63 轮都会被「干净缓存不写」挡掉。
                        cache.out_of_date = true;
                        if writer.maybe_save(&mut cache, true).is_ok() {
                            slot.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });

        assert!(saved.iter().all(|slot| slot.load(Ordering::Relaxed) > 0));
        let loaded = read_cache_back(&path);
        assert!(
            [2usize, 5].iter().any(|&n| is_complete_save_of(&loaded, n)),
            "published cache must be one writer's complete map, got {} entries",
            loaded.file_map.len()
        );
    }
}
