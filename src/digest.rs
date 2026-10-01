//! 文件摘要的计算与缓存复用。

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::time::UNIX_EPOCH;

use anyhow::{Result, bail};
use encoding_rs_io::DecodeReaderBytesBuilder;
use md5::{Digest, Md5};
use rayon::prelude::*;

use crate::READ_BUFFER_SIZE;
use crate::model::{DigestType, HaveRecord, WorkspaceCache, WorkspaceCacheEntry, WorkspaceFile};

/// Computes the digest for a binary file, simple MD5.
pub(crate) fn compute_digest_binary(file: &WorkspaceFile, hasher: &mut Md5) -> Result<()> {
    let mut file = File::open(&file.path)?;
    let mut buffer = [0; READ_BUFFER_SIZE];

    loop {
        match file.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(len) => hasher.update(&buffer[..len]),
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

/// Computes the digest for a text buffer, normalized line endings MD5.
pub(crate) fn update_text_digest_utf8<R: BufRead + Read>(
    input: &mut R,
    line_buffer: &mut Vec<u8>,
    hasher: &mut Md5,
) -> Result<()> {
    loop {
        match input.read_until(b'\n', line_buffer) {
            Ok(0) => return Ok(()),
            Ok(_n) => {
                if line_buffer.ends_with(b"\r\n") {
                    // Remove only the \r, keeping the \n (Perforce normalizes CRLF -> LF)
                    line_buffer.remove(line_buffer.len() - 2);
                }
                md5::digest::Update::update(hasher, line_buffer);
                line_buffer.clear();
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Computes the digest for a text file, normalized line endings MD5.
pub(crate) fn compute_digest_text(file: &WorkspaceFile, hasher: &mut Md5) -> Result<()> {
    let file = File::open(&file.path)?;
    let mut read = BufReader::with_capacity(READ_BUFFER_SIZE, file);
    let mut line_buffer = Vec::new();

    update_text_digest_utf8(&mut read, &mut line_buffer, hasher)
}

/// Computes the digest for a utf8 file, normalized line endings MD5 without BOM.
/// This function is a bit slower, but only few files have this encoding, so it is fine.
pub(crate) fn compute_digest_utf8(file: &WorkspaceFile, hasher: &mut Md5) -> Result<()> {
    let file = File::open(&file.path)?;
    let mut buffer = [0u8; READ_BUFFER_SIZE];
    let mut line_buffer = Vec::new();
    let mut full_buffer = Vec::new();

    let len = DecodeReaderBytesBuilder::new()
        .utf8_passthru(false)
        .bom_sniffing(true)
        .strip_bom(true)
        .build_with_buffer(file, &mut buffer[..])?
        .read_to_end(&mut full_buffer)?;

    let mut filled = &full_buffer[..len];
    update_text_digest_utf8(&mut filled, &mut line_buffer, hasher)
}

/// Computes the digest for a symlink.
///
/// Perforce stores a symlink revision as its target path, written with forward slashes and a
/// trailing newline, so that one revision can be synced on both unix and Windows. Verified
/// against a live depot: `libjsig.so` symlinked to `../libjsig.so` has size 14 and digest
/// `md5("../libjsig.so\n")` (the Windows client reports the target as `..\libjsig.so`).
///
/// A workspace synced without symlink support holds the target as a plain text file instead;
/// that form is read as text, which is what this used to do for every symlink.
pub(crate) fn compute_digest_symlink(file: &WorkspaceFile, hasher: &mut Md5) -> Result<()> {
    let metadata = std::fs::symlink_metadata(&file.path)?;

    if !metadata.file_type().is_symlink() {
        return compute_digest_utf8(file, hasher);
    }

    // `read_link` reads the link itself, so a dangling symlink still hashes correctly - and
    // reading it as a file would have failed or hashed whatever it points at.
    let target = std::fs::read_link(&file.path)?;
    let mut content = target.to_string_lossy().replace('\\', "/");
    content.push('\n');
    hasher.update(content.as_bytes());

    Ok(())
}

/// Check if a file is unchanged since last sync
pub(crate) fn is_unchanged_since_sync(
    file: &WorkspaceFile,
    have_records: &HashMap<String, HaveRecord>,
) -> bool {
    if let Some(have_rec) = have_records.get(&file.path_lower)
        && let Some(sync_time) = have_rec.sync_time
    {
        // Truncate to second precision (Perforce uses seconds, SystemTime has nanos)
        let file_time_secs = match file.date.duration_since(UNIX_EPOCH) {
            Ok(duration) => duration.as_secs(),
            Err(_) => return false,
        };

        return file_time_secs.abs_diff(sync_time) <= 1;
    }
    false
}

/// Computes digests for a number of files in the workspace.
pub(crate) fn parallel_compute_digests<'a>(
    files: Vec<(&'a WorkspaceFile, DigestType)>,
    cache: &mut WorkspaceCache,
) -> Result<Vec<(&'a WorkspaceFile, [u8; 16], bool)>> {
    let results: Result<Vec<(&'a WorkspaceFile, [u8; 16], bool)>> = files
        .into_par_iter()
        .with_max_len(1)
        .map(|file| -> Result<(&'a WorkspaceFile, [u8; 16], bool)> {
            // Check cache first
            if let Some(cache_entry) = cache.file_map.get(&file.0.path_lower)
                && cache_entry.size == file.0.size
                && cache_entry.date == file.0.date
            {
                return Ok((file.0, cache_entry.digest, true));
            }

            let mut hasher = Md5::new();
            let mut digest: [u8; 16] = Default::default();

            let file_data = std::fs::symlink_metadata(&file.0.path)?;
            // Perforce tracks symlinks as revisions of their own, so they are files here too.
            if !file_data.is_file() && !file_data.file_type().is_symlink() {
                bail!(
                    "Unsupported local file type for calculating digest ({:?})",
                    file_data.file_type()
                );
            }

            match file.1 {
                DigestType::Binary => compute_digest_binary(file.0, &mut hasher)?,
                DigestType::Text => compute_digest_text(file.0, &mut hasher)?,
                DigestType::Utf8 => compute_digest_utf8(file.0, &mut hasher)?,
                DigestType::Symlink => compute_digest_symlink(file.0, &mut hasher)?,
            }

            digest.copy_from_slice(&hasher.finalize()[..16]);
            Ok((file.0, digest, false))
        })
        .collect();

    let results = results?;

    // Update cache
    for result in &results {
        if !result.2 {
            cache.file_map.insert(
                result.0.path_lower.clone(),
                WorkspaceCacheEntry {
                    size: result.0.size,
                    date: result.0.date,
                    digest: result.1,
                },
            );
            cache.out_of_date = true;
        }
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;

    use std::time::Duration;

    use crate::path::local_path_key;

    // ---- 文本摘要的换行归一化 ----

    /// 用内存 reader 走一遍文本摘要；`&[u8]` 本身就实现了 `BufRead + Read`，不需要临时文件。
    fn text_digest(input: &[u8]) -> [u8; 16] {
        let mut hasher = Md5::new();
        let mut line_buffer = Vec::new();
        let mut reader = input;
        update_text_digest_utf8(&mut reader, &mut line_buffer, &mut hasher).unwrap();

        let mut digest = [0u8; 16];
        digest.copy_from_slice(&hasher.finalize()[..16]);
        digest
    }

    /// 期望值统一写成「某个字节串的 md5」，比十六进制字面量好读。
    fn md5_of(input: &[u8]) -> [u8; 16] {
        let digest: [u8; 16] = Md5::digest(input).into();
        digest
    }

    #[test]
    fn text_digest_normalizes_crlf_lines_to_lf() {
        assert_eq!(text_digest(b"a\r\nb\r\n"), md5_of(b"a\nb\n"));
    }

    /// 末行没有换行时原样保留，不能凭空补一个。
    #[test]
    fn text_digest_leaves_lf_and_unterminated_lines_alone() {
        assert_eq!(text_digest(b"a\r\nb\nc"), md5_of(b"a\nb\nc"));
    }

    /// 只删紧邻 `\n` 的那一个 `\r`，行中间的回车是文件内容的一部分。
    #[test]
    fn text_digest_removes_only_the_carriage_return_before_the_newline() {
        assert_eq!(text_digest(b"a\r\r\n"), md5_of(b"a\r\n"));
    }

    /// 文件末尾孤零零的 `\r` 不是 CRLF，必须留下。
    #[test]
    fn text_digest_keeps_a_lone_carriage_return_at_end_of_file() {
        assert_eq!(text_digest(b"a\r"), md5_of(b"a\r"));
    }

    /// 空文件不更新 hasher，结果就是 md5("")。
    #[test]
    fn text_digest_of_an_empty_file_is_the_md5_of_nothing() {
        assert_eq!(text_digest(b""), md5_of(b""));
    }

    #[test]
    fn text_digest_of_a_crlf_only_file_keeps_both_newlines() {
        assert_eq!(text_digest(b"\r\n\r\n"), md5_of(b"\n\n"));
    }

    // ---- 时间戳优化判据 ----

    fn have_map(path: &str, sync_time: Option<u64>) -> HashMap<String, HaveRecord> {
        HashMap::from([(local_path_key(path), HaveRecord { sync_time })])
    }

    fn file_dated(path: &str, since_epoch: Duration) -> WorkspaceFile {
        WorkspaceFile {
            path: path.to_owned(),
            path_lower: local_path_key(path),
            date: UNIX_EPOCH + since_epoch,
            ..Default::default()
        }
    }

    #[test]
    fn unchanged_since_sync_requires_a_have_record_with_a_sync_time() {
        const PATH: &str = r"C:\ws\a.txt";
        let file = file_dated(PATH, Duration::from_secs(100));

        assert!(
            !is_unchanged_since_sync(&file, &HashMap::new()),
            "no have record"
        );
        assert!(
            !is_unchanged_since_sync(&file, &have_map(PATH, None)),
            "have record without a syncTime"
        );
        assert!(
            !is_unchanged_since_sync(&file, &have_map(r"C:\ws\other.txt", Some(100))),
            "record filed under a different key"
        );
    }

    /// 文件系统与 p4 的秒级时间会互相差一秒，两个方向都要容忍；
    /// 差到两秒就不再认为是同一次同步。
    #[test]
    fn unchanged_since_sync_tolerates_one_second_of_skew_in_either_direction() {
        const PATH: &str = r"C:\ws\a.txt";

        let cases = [
            (100u64, 100u64, true),
            (100, 101, true), // 文件比 syncTime 新一秒
            (100, 99, true),  // 文件比 syncTime 旧一秒
            (100, 102, false),
            (100, 98, false),
        ];
        for (file_secs, sync_time, expected) in cases {
            let file = file_dated(PATH, Duration::from_secs(file_secs));
            assert_eq!(
                is_unchanged_since_sync(&file, &have_map(PATH, Some(sync_time))),
                expected,
                "file={file_secs} sync={sync_time}"
            );
        }
    }

    /// 文件时间里的亚秒部分被 `as_secs` 丢掉后才比较。
    #[test]
    fn unchanged_since_sync_truncates_the_file_time_to_seconds() {
        const PATH: &str = r"C:\ws\a.txt";

        let file = file_dated(PATH, Duration::from_millis(1500));
        assert!(is_unchanged_since_sync(&file, &have_map(PATH, Some(1))));

        // 0.9 秒截断成 0 秒，与 syncTime=1 相差一秒，仍在容差内。
        let file = file_dated(PATH, Duration::from_millis(900));
        assert!(is_unchanged_since_sync(&file, &have_map(PATH, Some(1))));

        // 1.9 秒截断成 1 秒，与 syncTime=3 相差两秒，超出容差。
        let file = file_dated(PATH, Duration::from_millis(1900));
        assert!(!is_unchanged_since_sync(&file, &have_map(PATH, Some(3))));
    }

    /// 早于 UNIX_EPOCH 的文件时间无法换算成秒，只能当作「不是未改动」。
    #[test]
    fn a_file_dated_before_the_epoch_is_never_unchanged() {
        const PATH: &str = r"C:\ws\a.txt";
        let file = WorkspaceFile {
            path: PATH.to_owned(),
            path_lower: local_path_key(PATH),
            date: UNIX_EPOCH - Duration::from_secs(1),
            ..Default::default()
        };

        assert!(!is_unchanged_since_sync(&file, &have_map(PATH, Some(0))));
    }

    /// p4 存的符号链接内容是"目标路径（正斜杠）+ 换行"。
    /// 公式取自真实 depot：//aki/.../libjsig.so 的 headType=symlink、fileSize=14，
    /// 摘要是 md5("../libjsig.so\n")，而 Windows 客户端上目标显示为 `..\libjsig.so`。
    #[test]
    fn symlink_digest_matches_the_depot_representation() {
        let tree = TempTree::new("symlink-digest");
        let target = tree.file("real.so", "content");
        let link = tree.root.join("link.so");
        if let Err(error) = symlink_file(&target, &link) {
            eprintln!("skipping: cannot create symlinks here ({error})");
            return;
        }

        let file = WorkspaceFile {
            path: link.display().to_string(),
            path_lower: local_path_key(&link.display().to_string()),
            ..Default::default()
        };

        let mut hasher = Md5::new();
        compute_digest_symlink(&file, &mut hasher).unwrap();
        let digest = hasher.finalize();

        let expected = format!("{}\n", target.display().to_string().replace('\\', "/"));
        assert_eq!(digest, Md5::digest(expected.as_bytes()));
        // 摘要描述的是链接本身，不能退化成所指向文件的内容摘要
        assert_ne!(digest, Md5::digest(b"content"));
    }

    /// 走完整的摘要管线：符号链接必须能通过文件类型检查，并按链接目标计算、写入缓存。
    /// 时间戳优化生效时这一步会被整个跳过，所以只有这里能覆盖到分派。
    #[test]
    fn parallel_digests_handle_symlinks() {
        let tree = TempTree::new("symlink-pipeline");
        let target = tree.file("real.so", "content");
        let link = tree.root.join("link.so");
        if let Err(error) = symlink_file(&target, &link) {
            eprintln!("skipping: cannot create symlinks here ({error})");
            return;
        }

        let file = WorkspaceFile {
            path: link.display().to_string(),
            path_lower: local_path_key(&link.display().to_string()),
            ..Default::default()
        };

        let mut cache = WorkspaceCache::default();
        let results =
            parallel_compute_digests(vec![(&file, DigestType::Symlink)], &mut cache).unwrap();

        let expected_content = format!("{}\n", target.display().to_string().replace('\\', "/"));
        let expected: [u8; 16] = Md5::digest(expected_content.as_bytes()).into();
        assert_eq!(results[0].1, expected);
        assert!(
            !results[0].2,
            "digest must be computed, not served from the cache"
        );
        assert!(cache.file_map.contains_key(&file.path_lower));
    }

    /// 以普通文本文件形式检出的符号链接（工作区不支持符号链接时）仍然按文本读取。
    #[test]
    fn a_symlink_stored_as_a_plain_file_is_hashed_as_text() {
        let tree = TempTree::new("symlink-as-file");
        let path = tree.file("link.so", "../libjsig.so\n");

        let file = WorkspaceFile {
            path: path.display().to_string(),
            path_lower: local_path_key(&path.display().to_string()),
            ..Default::default()
        };

        let mut hasher = Md5::new();
        compute_digest_symlink(&file, &mut hasher).unwrap();

        assert_eq!(hasher.finalize(), Md5::digest(b"../libjsig.so\n"));
    }
}
