//! 摘要缓存的落盘。

use std::fs::{File, create_dir_all};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::READ_BUFFER_SIZE;
use crate::model::WorkspaceCache;

/// Flushes the digest cache to disk as work progresses.
///
/// The cache used to be written exactly once, after the whole run finished. On a workspace
/// large enough to run out of memory - or to hit any other failure - that meant every digest
/// computed so far was thrown away, so a run could never make progress across attempts.
pub(crate) struct CacheWriter {
    path: PathBuf,
    last_save: Instant,
    entries_at_last_save: usize,
}

/// Saving after every batch would thrash the disk, so a save happens when either of these trips.
pub(crate) const CACHE_SAVE_MIN_INTERVAL: Duration = Duration::from_secs(60);
pub(crate) const CACHE_SAVE_MIN_NEW_ENTRIES: usize = 100_000;

impl CacheWriter {
    pub(crate) fn new(path: PathBuf) -> Self {
        CacheWriter {
            path,
            // Backdate so the first call is allowed to save immediately.
            last_save: Instant::now() - CACHE_SAVE_MIN_INTERVAL,
            entries_at_last_save: 0,
        }
    }

    /// Writes the cache when enough has changed, or unconditionally when `force` is set.
    pub(crate) fn maybe_save(&mut self, cache: &WorkspaceCache, force: bool) -> Result<()> {
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

        // Write to a temporary file and rename it into place, so a crash part way through
        // cannot leave a corrupt cache for the next run to load.
        let config = bincode::config::standard();
        let temp_path = self.path.with_extension("bin.tmp");
        {
            let file = File::create(&temp_path)?;
            let mut writer = BufWriter::with_capacity(READ_BUFFER_SIZE, file);
            bincode::encode_into_std_write(cache, &mut writer, config)?;
            writer.flush()?;
        }

        // On Windows `rename` replaces an existing file in one step.
        if std::fs::rename(&temp_path, &self.path).is_err() {
            // Some filesystems refuse to replace an existing file; remove it first instead.
            let _ = std::fs::remove_file(&self.path);
            std::fs::rename(&temp_path, &self.path)?;
        }

        self.last_save = Instant::now();
        self.entries_at_last_save = cache.file_map.len();

        println!(
            "    Saved {} cached digests to {}.",
            cache.file_map.len(),
            self.path.display()
        );

        Ok(())
    }
}

/// Saves the cache when a cache location could be determined.
pub(crate) fn save_cache(
    cache_writer: &mut Option<CacheWriter>,
    cache: &WorkspaceCache,
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

    use std::env;
    use std::io::BufReader;
    use std::path::{Path, PathBuf};
    use std::time::UNIX_EPOCH;

    use crate::model::WorkspaceCacheEntry;

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

    /// A fresh path under the temp directory. Named after the test so tests cannot collide.
    fn temp_cache_path(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("p4delta-test-{}", name));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("digests.bin")
    }

    fn read_cache_back(path: &Path) -> WorkspaceCache {
        let mut file = BufReader::new(File::open(path).unwrap());
        bincode::decode_from_std_read(&mut file, bincode::config::standard()).unwrap()
    }

    #[test]
    fn cache_writer_round_trips_a_forced_save() {
        let path = temp_cache_path("round-trip");
        let cache = cache_with(3, true);

        CacheWriter::new(path.clone())
            .maybe_save(&cache, true)
            .unwrap();

        let loaded = read_cache_back(&path);
        assert_eq!(loaded.file_map.len(), 3);
        assert!(loaded.out_of_date);
        // The intermediate file must not be left behind, or the next save would see it as
        // the cache.
        assert!(!path.with_extension("bin.tmp").exists());
    }

    #[test]
    fn cache_writer_overwrites_the_previous_save() {
        let path = temp_cache_path("overwrite");
        let mut writer = CacheWriter::new(path.clone());

        writer.maybe_save(&cache_with(2, true), true).unwrap();
        writer.maybe_save(&cache_with(5, true), true).unwrap();

        assert_eq!(read_cache_back(&path).file_map.len(), 5);
    }

    #[test]
    fn cache_writer_skips_a_save_with_nothing_new() {
        let path = temp_cache_path("nothing-new");
        let mut writer = CacheWriter::new(path.clone());
        let cache = cache_with(1, true);

        writer.maybe_save(&cache, true).unwrap();
        // Delete the file: anything the second call writes has to recreate it.
        std::fs::remove_file(&path).unwrap();
        writer.maybe_save(&cache, false).unwrap();

        // Neither the entry count nor the clock has moved past its threshold, so there was
        // nothing worth paying a disk write for.
        assert!(!path.exists());
    }

    /// 间隔到了就该落盘，哪怕没有新增条目——两个阈值是「任一触发」，不是「都要满足」。
    /// `cache_writer_skips_a_save_with_nothing_new` 覆盖的是时间未到的那一侧。
    #[test]
    fn cache_writer_saves_once_the_interval_has_passed() {
        let path = temp_cache_path("interval-passed");
        let cache = cache_with(1, true);

        // 直接构造并回拨 last_save：测试是子模块，够得着私有字段，不必注入时钟，
        // 也不用真等 60 秒。
        let mut writer = CacheWriter {
            path: path.clone(),
            last_save: Instant::now() - CACHE_SAVE_MIN_INTERVAL,
            entries_at_last_save: cache.file_map.len(),
        };

        writer.maybe_save(&cache, false).unwrap();

        assert!(path.exists());
        assert_eq!(read_cache_back(&path).file_map.len(), 1);
    }

    #[test]
    fn cache_writer_skips_an_up_to_date_cache() {
        let path = temp_cache_path("up-to-date");

        // A cache loaded from disk and never added to. Saving it would rewrite the same bytes,
        // and a run that computed nothing must not look like a run that did.
        CacheWriter::new(path.clone())
            .maybe_save(&cache_with(4, false), true)
            .unwrap();

        assert!(!path.exists());
    }
}
