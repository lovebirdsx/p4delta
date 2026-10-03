//! `p4 -G` 输出使用的 Python marshal 格式解析。

use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use encoding_rs::Encoding;

use crate::charset::{decode_p4_bytes, strip_bom};
use crate::model::HaveRecord;
use crate::path::local_path_key;

// Python marshal 格式的类型码。
pub(crate) const TYPE_NULL: u8 = b'0';
pub(crate) const TYPE_DICT: u8 = b'{';
pub(crate) const TYPE_STRING: u8 = b's';

/// 从 Python marshal 格式里读一个字符串。
/// 格式：`'s'`（类型字节）+ 4 字节小端 i32（长度）+ N 字节（数据）。
///
/// 缓冲只是被截断时返回 `Ok(None)`，让流式调用方去等更多字节；`Err` 只留给永远不可能
/// 变合法的数据。
pub(crate) fn read_marshal_string(cursor: &mut &[u8]) -> Result<Option<Vec<u8>>> {
    // 在副本上试探：读到一半失败时调用方的游标保持不动。
    let mut probe = *cursor;

    if probe.is_empty() {
        return Ok(None);
    }

    if probe[0] != TYPE_STRING {
        bail!("Expected string type 's' (0x73), got 0x{:02x}", probe[0]);
    }
    probe = &probe[1..];

    if probe.len() < 4 {
        return Ok(None);
    }

    let length = i32::from_le_bytes([probe[0], probe[1], probe[2], probe[3]]) as usize;
    probe = &probe[4..];

    if probe.len() < length {
        return Ok(None);
    }

    let data = probe[..length].to_vec();
    probe = &probe[length..];

    *cursor = probe;
    Ok(Some(data))
}

/// 从 Python marshal 格式里读一个字典。
/// 格式：`'{'`（类型字节）+（键字符串 + 值字符串）* + `'0'`（终止符）。
///
/// `dict` 是清空后原地填充的：流式调用方复用一个 map，不必为几百万条记录里的每一条
/// 重新分配（那还会连带每条记录每个字段两个 `Vec`）。
pub(crate) fn read_marshal_dict_into(
    cursor: &mut &[u8],
    dict: &mut HashMap<Vec<u8>, Vec<u8>>,
) -> Result<Option<()>> {
    let mut probe = *cursor;

    if probe.is_empty() {
        return Ok(None);
    }

    if probe[0] != TYPE_DICT {
        bail!("Expected dict type '{{' (0x7b), got 0x{:02x}", probe[0]);
    }
    probe = &probe[1..];

    dict.clear();

    loop {
        if probe.is_empty() {
            return Ok(None);
        }

        if probe[0] == TYPE_NULL {
            probe = &probe[1..];
            break;
        }

        let Some(key) = read_marshal_string(&mut probe)? else {
            return Ok(None);
        };
        let Some(value) = read_marshal_string(&mut probe)? else {
            return Ok(None);
        };
        dict.insert(key, value);
    }

    *cursor = probe;
    Ok(Some(()))
}

/// 从解析好的 marshal 字典里取出一条 have 记录。
/// 非 stat 记录（错误、提示消息）返回 `Ok(None)`。
pub(crate) fn have_record_from_dict(
    dict: &HashMap<Vec<u8>, Vec<u8>>,
    encoding: &'static Encoding,
) -> Result<Option<(String, HaveRecord)>> {
    if dict.get(b"code".as_ref()).map(|code| code.as_slice()) != Some(b"stat") {
        return Ok(None);
    }

    let path = dict
        .get(b"path".as_ref())
        .ok_or_else(|| anyhow!("Record missing 'path' field"))?;

    // 路径按 p4 配置的字符集返回。用别的字符集解码会静默产生乱码，
    // 永远匹配不上本地文件系统。
    let (path_str, _had_replacements) = decode_p4_bytes(path, encoding);

    let sync_time = match dict.get(b"syncTime".as_ref()) {
        Some(value) => {
            let text = String::from_utf8_lossy(value);
            match text.parse::<u64>() {
                Ok(number) => Some(number),
                Err(e) => {
                    eprintln!("Warning: Failed to parse syncTime '{}': {}", text, e);
                    None
                }
            }
        }
        None => None,
    };

    // 转小写以支持 Windows 上的大小写不敏感匹配，同时统一分隔符，让这些键与本地文件系统、
    // fstat 的 clientFile 建出来的键落在同一处。
    Ok(Some((local_path_key(&path_str), HaveRecord { sync_time })))
}

/// `p4 -G have` marshal 流的增量解析器。
///
/// 大工作区的完整响应有好几 GB，所以按块消费、逐条解析；所有记录复用同一个 scratch 字典。
pub(crate) struct MarshalStreamParser {
    encoding: &'static Encoding,
    buffer: Vec<u8>,
    /// `buffer` 里第一个未消费字节的偏移。
    consumed: usize,
    scratch: HashMap<Vec<u8>, Vec<u8>>,
    records: HashMap<String, HaveRecord>,
    total_parsed: usize,
    missing_sync_time: usize,
    /// 是否已经见过数据块：BOM 只在最开头剥一次。
    started: bool,
}

impl MarshalStreamParser {
    pub(crate) fn new(encoding: &'static Encoding) -> Self {
        MarshalStreamParser {
            encoding,
            buffer: Vec::new(),
            consumed: 0,
            scratch: HashMap::new(),
            records: HashMap::new(),
            total_parsed: 0,
            missing_sync_time: 0,
            started: false,
        }
    }

    pub(crate) fn push_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        // BOM 只会出现在流的最开头。
        let chunk = if self.started {
            chunk
        } else {
            self.started = true;
            strip_bom(chunk, self.encoding)
        };

        self.buffer.extend_from_slice(chunk);

        loop {
            let parsed_length = {
                let mut cursor = &self.buffer[self.consumed..];
                match read_marshal_dict_into(&mut cursor, &mut self.scratch)? {
                    Some(()) => Some(self.buffer.len() - self.consumed - cursor.len()),
                    // 记录被截断：等下一块数据。
                    None => None,
                }
            };

            let Some(parsed_length) = parsed_length else {
                break;
            };
            self.consumed += parsed_length;
            self.total_parsed += 1;

            if let Some((key, record)) = have_record_from_dict(&self.scratch, self.encoding)? {
                if record.sync_time.is_none() {
                    self.missing_sync_time += 1;
                }
                self.records.insert(key, record);
            }
        }

        // 缓冲区大部分消费掉之后就压缩一次，避免它随流一直增长。
        if self.consumed >= (1 << 20) && self.consumed * 2 >= self.buffer.len() {
            self.buffer.drain(..self.consumed);
            self.consumed = 0;
        }

        Ok(())
    }

    pub(crate) fn finish(self) -> HashMap<String, HaveRecord> {
        if self.consumed < self.buffer.len() {
            eprintln!(
                "Warning: Failed to parse a trailing marshal record, output may be truncated."
            );
        }

        println!(
            "      Parsed {} have records ({} missing syncTime).",
            self.total_parsed, self.missing_sync_time
        );

        self.records
    }
}

/// 解析一份完整的 `p4 -G have` 响应。留作测试的单一入口。
#[cfg(test)]
pub(crate) fn parse_p4_have_output(
    data: &[u8],
    encoding: &'static Encoding,
) -> Result<HashMap<String, HaveRecord>> {
    let mut parser = MarshalStreamParser::new(encoding);
    parser.push_chunk(data)?;
    Ok(parser.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;

    use encoding_rs::{UTF_8, WINDOWS_1252};

    #[test]
    fn parses_have_records_with_non_ascii_paths() {
        let mut data = marshal_dict(&[
            ("code", "stat"),
            ("path", "E:\\中文\\私服使用说明.docx"),
            ("syncTime", "1700000000"),
        ]);
        // 非 stat 记录（错误、提示消息）照样跳过。
        data.extend_from_slice(&marshal_dict(&[
            ("code", "error"),
            ("path", "E:\\ignored.txt"),
        ]));

        let records = parse_p4_have_output(&data, UTF_8).unwrap();

        assert_eq!(records.len(), 1);
        let record = records
            .get("e:\\中文\\私服使用说明.docx")
            .expect("key should be the ascii-lowercased utf8 path");
        assert_eq!(record.sync_time, Some(1700000000));
    }

    #[test]
    fn wrong_charset_yields_a_different_key() {
        // 原始 bug 的回归守卫：字符集用错会静默产生另一个键，
        // 那正是「每个非 ASCII 文件既像新增又像删除」的成因。
        let data = marshal_dict(&[
            ("code", "stat"),
            ("path", "E:\\中文.txt"),
            ("syncTime", "1"),
        ]);

        let utf8_keys: Vec<_> = parse_p4_have_output(&data, UTF_8)
            .unwrap()
            .into_keys()
            .collect();
        let cp1252_keys: Vec<_> = parse_p4_have_output(&data, WINDOWS_1252)
            .unwrap()
            .into_keys()
            .collect();

        assert_ne!(utf8_keys, cp1252_keys);
    }

    #[test]
    fn marshal_reader_reports_truncation_instead_of_failing() {
        let full = marshal_string("hello");

        // 每个真前缀都表示「还需要更多字节」，绝不能算错误。
        for length in 0..full.len() {
            let mut cursor = &full[..length];
            assert_eq!(
                read_marshal_string(&mut cursor).unwrap(),
                None,
                "prefix of {length} bytes should ask for more data"
            );
        }

        let mut cursor = &full[..];
        assert_eq!(
            read_marshal_string(&mut cursor).unwrap(),
            Some(b"hello".to_vec())
        );
        assert!(cursor.is_empty(), "a complete read must consume the cursor");
    }

    #[test]
    fn marshal_reader_rejects_invalid_type_bytes() {
        let mut cursor = &b"X"[..];
        assert!(read_marshal_string(&mut cursor).is_err());
    }

    #[test]
    fn streaming_marshal_parsing_matches_a_single_shot_parse() {
        // 字节流怎么切都不该影响分块解析的结果——这正是它能直接对着管道读的前提。
        let mut data = marshal_dict(&[
            ("code", "stat"),
            ("path", "E:\\中文\\a.txt"),
            ("syncTime", "1700000000"),
        ]);
        data.extend_from_slice(&marshal_dict(&[("code", "error"), ("path", "E:\\b.txt")]));
        data.extend_from_slice(&marshal_dict(&[
            ("code", "stat"),
            ("path", "E:\\c.txt"),
            ("syncTime", "1700000001"),
        ]));

        let expected = parse_p4_have_output(&data, UTF_8).unwrap();
        assert_eq!(expected.len(), 2);

        for chunk_size in [1usize, 3, 7, 64] {
            let mut parser = MarshalStreamParser::new(UTF_8);
            for chunk in data.chunks(chunk_size) {
                parser.push_chunk(chunk).unwrap();
            }
            let streamed = parser.finish();

            assert_eq!(streamed.len(), expected.len(), "chunk size {chunk_size}");
            for (key, record) in &expected {
                assert_eq!(
                    streamed.get(key).map(|found| found.sync_time),
                    Some(record.sync_time),
                    "chunk size {chunk_size}, key {key}"
                );
            }
        }
    }

    #[test]
    fn streaming_parser_drops_a_truncated_trailing_record() {
        let mut data = marshal_dict(&[("code", "stat"), ("path", "E:\\a.txt"), ("syncTime", "1")]);
        // 最后一条记录从中间被切断，就像 p4 被杀掉时那样。
        data.extend_from_slice(&marshal_dict(&[("code", "stat"), ("path", "E:\\b.txt")])[..10]);

        let mut parser = MarshalStreamParser::new(UTF_8);
        parser.push_chunk(&data).unwrap();
        let records = parser.finish();

        // 完整的记录留了下来，被截断的那条丢弃，而不是污染这次解析。
        assert_eq!(records.len(), 1);
        assert!(records.contains_key("e:\\a.txt"));
    }
}
