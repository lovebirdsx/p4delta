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

/// 算二进制文件的摘要：直接对内容做 MD5。
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

/// 按行更新文本摘要：每行先归一化换行，再喂给 hasher。
///
/// 名字里的 utf8 是历史遗留——这个辅助函数不做任何解码，`compute_digest_text` 与
/// `compute_digest_utf8` 都调它；需要解码的是后者，它把解码器接在调用方那一侧。
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
                    // 只删 `\r`、保留 `\n`（Perforce 把 CRLF 归一化成 LF）。
                    line_buffer.remove(line_buffer.len() - 2);
                }
                md5::digest::Update::update(hasher, line_buffer);
                line_buffer.clear();
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// 算文本文件的摘要：按行归一化换行后再 MD5。
pub(crate) fn compute_digest_text(file: &WorkspaceFile, hasher: &mut Md5) -> Result<()> {
    let file = File::open(&file.path)?;
    let mut read = BufReader::with_capacity(READ_BUFFER_SIZE, file);
    let mut line_buffer = Vec::new();

    update_text_digest_utf8(&mut read, &mut line_buffer, hasher)
}

/// 算 utf8 文件的摘要：解码、剥 BOM、按行归一化换行后再 MD5。
///
/// 解码配置与整份读入时逐条相同（`utf8_passthru(false)`、BOM 嗅探、剥 BOM），改动只在
/// 「不再把整个文件读进内存」：解码器流式接进 `BufReader`，未完成的多字节序列由解码器
/// 自己跨 read 持有；按行归一化仍是共用的 [`update_text_digest_utf8`]，判定规则一个
/// 字节都没变。没有 BOM 的文件走解码器的透传路径，等同于直接读原始字节。
pub(crate) fn compute_digest_utf8(file: &WorkspaceFile, hasher: &mut Md5) -> Result<()> {
    let file = File::open(&file.path)?;
    let mut buffer = [0u8; READ_BUFFER_SIZE];
    let mut line_buffer = Vec::new();

    let decoded = DecodeReaderBytesBuilder::new()
        .utf8_passthru(false)
        .bom_sniffing(true)
        .strip_bom(true)
        .build_with_buffer(file, &mut buffer[..])?;

    let mut read = BufReader::with_capacity(READ_BUFFER_SIZE, decoded);
    update_text_digest_utf8(&mut read, &mut line_buffer, hasher)
}

/// 算符号链接的摘要。
///
/// Perforce 把符号链接的修订存成它的目标路径，用正斜杠书写、带一个结尾换行，
/// 好让同一份修订在 unix 与 Windows 上都能同步。对着真实 depot 校验过：`libjsig.so`
/// 链到 `../libjsig.so`，size 14，摘要是 `md5("../libjsig.so\n")`——Windows 客户端把
/// 目标报告成 `..\libjsig.so`，但摘要按正斜杠算。
///
/// 在不支持符号链接的平台上同步出来的工作区，会把目标存成一个普通文本文件；
/// 那种形态按文本读——这也是本函数从前对所有符号链接的做法。
pub(crate) fn compute_digest_symlink(file: &WorkspaceFile, hasher: &mut Md5) -> Result<()> {
    let metadata = std::fs::symlink_metadata(&file.path)?;

    if !metadata.file_type().is_symlink() {
        return compute_digest_utf8(file, hasher);
    }

    // `read_link` 读的是链接本身，所以悬空链接也能算出正确的摘要——若当普通文件读，
    // 要么失败，要么哈希成它所指向的那个文件。
    let target = std::fs::read_link(&file.path)?;
    let mut content = target.to_string_lossy().replace('\\', "/");
    content.push('\n');
    hasher.update(content.as_bytes());

    Ok(())
}

/// 判断文件自上次同步以来是否没被改过。
pub(crate) fn is_unchanged_since_sync(
    file: &WorkspaceFile,
    have_records: &HashMap<String, HaveRecord>,
) -> bool {
    if let Some(have_rec) = have_records.get(&file.path_lower)
        && let Some(sync_time) = have_rec.sync_time
    {
        // 截到秒：Perforce 用秒，SystemTime 带纳秒。
        let file_time_secs = match file.date.duration_since(UNIX_EPOCH) {
            Ok(duration) => duration.as_secs(),
            Err(_) => return false,
        };

        return file_time_secs.abs_diff(sync_time) <= 1;
    }
    false
}

/// 摘要计算要不要吃缓存。
///
/// 绝大多数调用点用 [`Self::Use`]——缓存是第二轮快一个数量级的全部原因。`--verify-all`
/// 用 [`Self::Ignore`]：缓存里放的是**上一轮**算出来的值，拿它下结论就还是推断，而那一档
/// 的全部意义就是把推断换成验证。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum CachePolicy {
    Use,
    Ignore,
}

/// 单个文件的摘要结果：文件本身、16 字节摘要，以及这个摘要是直接取自缓存
/// （`from_cache`）还是这一轮真算出来的——统计「Hash 了多少字节」要靠它区分。
#[derive(Debug)]
pub(crate) struct DigestOutcome<'a> {
    pub(crate) file: &'a WorkspaceFile,
    pub(crate) digest: [u8; 16],
    pub(crate) from_cache: bool,
}

/// 为工作区里的多个文件算摘要。
///
/// 返回的结果与传入的 `files` 按位对应（rayon 收进 Vec 保序）。
/// `policy` 为 [`CachePolicy::Ignore`] 时跳过缓存查找、逐个重算；重算的结果照常写回缓存，
/// 默认档接着受益。
pub(crate) fn parallel_compute_digests<'a>(
    files: Vec<(&'a WorkspaceFile, DigestType)>,
    cache: &mut WorkspaceCache,
    policy: CachePolicy,
) -> Result<Vec<DigestOutcome<'a>>> {
    let results: Result<Vec<DigestOutcome<'a>>> = files
        .into_par_iter()
        .with_max_len(1)
        .map(|(file, digest_type)| -> Result<DigestOutcome<'a>> {
            if policy == CachePolicy::Use
                && let Some(cache_entry) = cache.file_map.get(&file.path_lower)
                && cache_entry.size == file.size
                && cache_entry.date == file.date
            {
                return Ok(DigestOutcome {
                    file,
                    digest: cache_entry.digest,
                    from_cache: true,
                });
            }

            let mut hasher = Md5::new();
            let mut digest: [u8; 16] = Default::default();

            let file_data = std::fs::symlink_metadata(&file.path)?;
            // Perforce 把符号链接当作独立修订跟踪，所以这里它们也算文件。
            if !file_data.is_file() && !file_data.file_type().is_symlink() {
                bail!(
                    "Unsupported local file type for calculating digest ({:?})",
                    file_data.file_type()
                );
            }

            match digest_type {
                DigestType::Binary => compute_digest_binary(file, &mut hasher)?,
                DigestType::Text => compute_digest_text(file, &mut hasher)?,
                DigestType::Utf8 => compute_digest_utf8(file, &mut hasher)?,
                DigestType::Symlink => compute_digest_symlink(file, &mut hasher)?,
            }

            digest.copy_from_slice(&hasher.finalize()[..16]);
            Ok(DigestOutcome {
                file,
                digest,
                from_cache: false,
            })
        })
        .collect();

    let results = results?;

    for result in &results {
        if !result.from_cache {
            cache.file_map.insert(
                result.file.path_lower.clone(),
                WorkspaceCacheEntry {
                    size: result.file.size,
                    date: result.file.date,
                    digest: result.digest,
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

    use std::path::{Path, PathBuf};
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

    // ---- UTF-8 解码（BOM、流式读取） ----

    fn workspace_file(path: &Path) -> WorkspaceFile {
        WorkspaceFile {
            path: path.display().to_string(),
            path_lower: local_path_key(&path.display().to_string()),
            ..Default::default()
        }
    }

    /// 把原始字节写进临时树：BOM、UTF-16 这类内容没法用 `TempTree::file` 的 `&str` 表达。
    fn write_bytes(tree: &TempTree, name: &str, contents: &[u8]) -> PathBuf {
        let path = tree.root.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// 走磁盘上的 `compute_digest_utf8` 全流程，覆盖解码 + 按行归一化。
    fn utf8_file_digest(path: &Path) -> [u8; 16] {
        let mut hasher = Md5::new();
        compute_digest_utf8(&workspace_file(path), &mut hasher).unwrap();

        let mut digest = [0u8; 16];
        digest.copy_from_slice(&hasher.finalize()[..16]);
        digest
    }

    /// 带 BOM 的 UTF-16 编码，用来构造 UTF-16 输入。
    fn utf16_with_bom(text: &str, big_endian: bool) -> Vec<u8> {
        let mut out = if big_endian {
            vec![0xFE, 0xFF]
        } else {
            vec![0xFF, 0xFE]
        };
        for unit in text.encode_utf16() {
            out.extend_from_slice(&if big_endian {
                unit.to_be_bytes()
            } else {
                unit.to_le_bytes()
            });
        }
        out
    }

    /// 流式改造之前的实现，逐字保留，**只作测试 oracle**：整份解码进内存，再交给共用的
    /// 按行归一化。它与新实现的差别只有缓冲策略，所以两者必须逐字节一致。
    fn oracle_utf8_digest(path: &Path) -> [u8; 16] {
        let file = File::open(path).unwrap();
        let mut buffer = [0u8; READ_BUFFER_SIZE];
        let mut line_buffer = Vec::new();
        let mut full_buffer = Vec::new();

        let len = DecodeReaderBytesBuilder::new()
            .utf8_passthru(false)
            .bom_sniffing(true)
            .strip_bom(true)
            .build_with_buffer(file, &mut buffer[..])
            .unwrap()
            .read_to_end(&mut full_buffer)
            .unwrap();

        let mut hasher = Md5::new();
        let mut filled = &full_buffer[..len];
        update_text_digest_utf8(&mut filled, &mut line_buffer, &mut hasher).unwrap();

        let mut digest = [0u8; 16];
        digest.copy_from_slice(&hasher.finalize()[..16]);
        digest
    }

    /// BOM 要在算摘要前剥掉：内容相同、有没有 BOM，摘要必须一样。
    #[test]
    fn utf8_digest_strips_the_bom() {
        let tree = TempTree::new("utf8-bom");
        let with_bom = write_bytes(&tree, "bom.txt", "\u{FEFF}alpha\r\nbeta\n".as_bytes());
        let without_bom = write_bytes(&tree, "plain.txt", "alpha\r\nbeta\n".as_bytes());

        let expected = md5_of(b"alpha\nbeta\n");
        assert_eq!(utf8_file_digest(&with_bom), expected, "带 UTF-8 BOM");
        assert_eq!(utf8_file_digest(&without_bom), expected, "不带 BOM");

        // 只剩 BOM 的文件解码后就是空文件。
        let only_bom = write_bytes(&tree, "only-bom.txt", "\u{FEFF}".as_bytes());
        assert_eq!(utf8_file_digest(&only_bom), md5_of(b""));
    }

    /// BOM 嗅探认得 UTF-16LE/BE：解码成 UTF-8、剥 BOM，再按同一套换行归一化。
    /// 期望值是手写的「UTF-8 内容（LF 结尾）的 md5」，不经过任何待测路径，避免自证。
    #[test]
    fn utf8_digest_decodes_utf16_with_a_bom() {
        let tree = TempTree::new("utf8-utf16");
        let text = "第一行\r\nsecond line\r\n第三行";
        let expected = md5_of("第一行\nsecond line\n第三行".as_bytes());

        let le = write_bytes(&tree, "le.txt", &utf16_with_bom(text, false));
        let be = write_bytes(&tree, "be.txt", &utf16_with_bom(text, true));

        assert_eq!(utf8_file_digest(&le), expected, "UTF-16LE");
        assert_eq!(utf8_file_digest(&be), expected, "UTF-16BE");
    }

    /// 单行比读缓冲（128 KiB）还长：`read_until` 要跨多次填充把整行拼齐，
    /// 末尾的 CRLF 照样只删 `\r`，末行没有换行也不能补。
    #[test]
    fn utf8_digest_normalizes_a_line_longer_than_the_read_buffer() {
        let tree = TempTree::new("utf8-long-line");

        let prefix = vec![b'a'; READ_BUFFER_SIZE - 1];
        let mut raw = prefix.clone();
        raw.extend_from_slice(b"\r\nb\r\nc");
        let mut expected = prefix;
        expected.extend_from_slice(b"\nb\nc");

        let path = write_bytes(&tree, "long.txt", &raw);
        assert_eq!(utf8_file_digest(&path), md5_of(&expected));
    }

    /// `\r` 落在读缓冲填充的接缝上（最后一个字节、以及前后各一个字节）时，
    /// 跨填充的 CRLF 仍要当成一个换行处理——按行归一化的实现天然正确，
    /// 这条用例锁住的是「别哪天退化成固定块状态机」。
    #[test]
    fn utf8_digest_normalizes_crlf_split_across_read_buffer_fills() {
        let tree = TempTree::new("utf8-crlf-boundary");

        for offset in [READ_BUFFER_SIZE - 1, READ_BUFFER_SIZE, READ_BUFFER_SIZE + 1] {
            let mut raw = vec![b'a'; offset];
            raw.extend_from_slice(b"\r\n");
            raw.extend_from_slice(b"tail");

            let mut expected = vec![b'a'; offset];
            expected.extend_from_slice(b"\ntail");

            let path = write_bytes(&tree, &format!("split-{offset}.txt"), &raw);
            assert_eq!(
                utf8_file_digest(&path),
                md5_of(&expected),
                "offset={offset}"
            );
        }

        // 三条内容各自不同，防的是三条断言其实在看同一份数据。
        let a = utf8_file_digest(&tree.root.join("split-131071.txt"));
        let b = utf8_file_digest(&tree.root.join("split-131072.txt"));
        let c = utf8_file_digest(&tree.root.join("split-131073.txt"));
        assert!(a != b && b != c && a != c);
    }

    /// 多字节字符跨解码缓冲边界：3 字节的汉字在 128 KiB 处必然被切一刀，
    /// 解码器要握住半个字符（而不是吐 U+FFFD）；UTF-16 的代理对同理。
    #[test]
    fn utf8_digest_keeps_multibyte_characters_across_buffer_boundaries() {
        let tree = TempTree::new("utf8-decode-boundary");

        // 150000 字节 > 128 KiB，且 131072 % 3 == 2，切缝必然落在某个字符中间。
        let text = "中".repeat(50_000);
        let raw = format!("\u{FEFF}{text}\r\ntail");
        let path = write_bytes(&tree, "utf8-boundary.txt", raw.as_bytes());
        assert_eq!(
            utf8_file_digest(&path),
            md5_of(format!("{text}\ntail").as_bytes())
        );

        // UTF-16 一侧：140000 字节原始输入，解码输出 210000 字节，两侧都跨缓冲。
        let utf16_text = "中".repeat(70_000);
        let utf16 = utf16_with_bom(&format!("{utf16_text}\r\n"), false);
        let path = write_bytes(&tree, "utf16-boundary.txt", &utf16);
        assert_eq!(
            utf8_file_digest(&path),
            md5_of(format!("{utf16_text}\n").as_bytes())
        );
    }

    /// 新旧实现对拍：固定期望值防「两边一起错」，这个 oracle 防「新实现悄悄偏离旧行为」。
    /// `oracle_utf8_digest` 就是改造前的整份读入版本，输入覆盖空文件、无末尾换行、
    /// 孤立 CR、BOM、UTF-16 与跨缓冲的长行。
    #[test]
    fn streaming_utf8_digest_agrees_with_the_buffered_oracle() {
        let tree = TempTree::new("utf8-oracle");

        let long_line = {
            let mut content = vec![b'x'; READ_BUFFER_SIZE + 7];
            content.extend_from_slice(b"\r\n");
            content.extend_from_slice(b"tail");
            content
        };

        let payloads: [(&str, Vec<u8>); 10] = [
            ("empty", Vec::new()),
            ("no-trailing-newline", b"line\r\nline2".to_vec()),
            ("lone-cr", b"lone\rcarriage\r".to_vec()),
            ("crlf-only", b"\r\n\r\n".to_vec()),
            ("bom", "\u{FEFF}a\r\nb\r\n".as_bytes().to_vec()),
            ("bom-only", "\u{FEFF}".as_bytes().to_vec()),
            ("utf16le", utf16_with_bom("one\r\ntwo\r\n", false)),
            ("utf16be", utf16_with_bom("one\r\ntwo\r\n", true)),
            (
                "multibyte-boundary",
                format!("\u{FEFF}{}\r\n", "中".repeat(50_000)).into_bytes(),
            ),
            ("long-line", long_line),
        ];

        for (name, payload) in payloads {
            let path = write_bytes(&tree, &format!("{name}.txt"), &payload);
            assert_eq!(
                utf8_file_digest(&path),
                oracle_utf8_digest(&path),
                "payload={name}"
            );
        }
    }

    /// 空文件、孤立 CR、无末尾换行这几条边界，走流式全流程再过一遍。
    #[test]
    fn utf8_digest_edge_cases_survive_the_streaming_path() {
        let tree = TempTree::new("utf8-edges");

        let cases: [(&str, &[u8], &[u8]); 4] = [
            ("empty", b"", b""),
            ("lone-cr", b"a\r", b"a\r"),
            ("crlf", b"a\r\nb", b"a\nb"),
            ("no-final-newline", b"a\nb", b"a\nb"),
        ];

        for (name, raw, expected) in cases {
            let path = write_bytes(&tree, &format!("{name}.txt"), raw);
            assert_eq!(utf8_file_digest(&path), md5_of(expected), "case={name}");
        }
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
        let results = parallel_compute_digests(
            vec![(&file, DigestType::Symlink)],
            &mut cache,
            CachePolicy::Use,
        )
        .unwrap();

        let expected_content = format!("{}\n", target.display().to_string().replace('\\', "/"));
        let expected: [u8; 16] = Md5::digest(expected_content.as_bytes()).into();
        assert_eq!(results[0].digest, expected);
        assert!(
            !results[0].from_cache,
            "digest must be computed, not served from the cache"
        );
        assert!(cache.file_map.contains_key(&file.path_lower));
    }

    /// `--verify-all` 的立身之本：缓存里的值再「新鲜」也不能拿来下结论。
    ///
    /// 用例先把缓存正常填上，再把摘要改成**错值**、size 与 mtime 保持原样——在默认档看来
    /// 它完全可信。两个策略跑同一份输入：`Use` 交出那个错值，`Ignore` 交出真值。
    /// 哪天 `Ignore` 被写回成「照查缓存」，这条用例立刻变红。
    #[test]
    fn ignoring_the_cache_recomputes_the_digest() {
        let tree = TempTree::new("cache-policy");
        let path = tree.file("data.txt", "real content");

        let file = WorkspaceFile {
            path: path.display().to_string(),
            path_lower: local_path_key(&path.display().to_string()),
            ..Default::default()
        };

        let mut cache = WorkspaceCache::default();
        let first = parallel_compute_digests(
            vec![(&file, DigestType::Text)],
            &mut cache,
            CachePolicy::Use,
        )
        .unwrap();
        assert!(!first[0].from_cache, "空缓存必然 miss");
        let correct = first[0].digest;

        // 把缓存里的摘要换成错值：size 与 date 都还对着，默认档看不出破绽。
        cache.file_map.get_mut(&file.path_lower).unwrap().digest = [0xAB; 16];

        let cached = parallel_compute_digests(
            vec![(&file, DigestType::Text)],
            &mut cache,
            CachePolicy::Use,
        )
        .unwrap();
        assert!(cached[0].from_cache, "默认档该命中缓存");
        assert_eq!(cached[0].digest, [0xAB; 16], "命中缓存就是交出缓存里的值");

        let recomputed = parallel_compute_digests(
            vec![(&file, DigestType::Text)],
            &mut cache,
            CachePolicy::Ignore,
        )
        .unwrap();
        assert!(!recomputed[0].from_cache, "验证档不该命中缓存");
        assert_eq!(recomputed[0].digest, correct, "验证档要的是重算出来的真值");
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
