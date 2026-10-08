//! `p4 -G` 输出使用的 Python marshal 格式解析。

use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use encoding_rs::Encoding;

use crate::charset::{decode_p4_bytes, strip_bom};
use crate::json::sayln;
use crate::model::HaveRecord;
use crate::path::local_path_key;

// Python marshal 格式的类型码。
pub(crate) const TYPE_NULL: u8 = b'0';
pub(crate) const TYPE_DICT: u8 = b'{';
pub(crate) const TYPE_STRING: u8 = b's';
/// int：`'i'` + 4 字节小端。p4 的错误记录带这样的字段（实测 `severity` / `generic`）。
pub(crate) const TYPE_INT: u8 = b'i';

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

/// 从 Python marshal 格式里读一个字典值。
///
/// 值可以是字符串，也可以是 int——p4 的错误记录就是后者（实测 `p4 -G have` 对本地与
/// depot 都没有的路径返回 `{'code': 'error', 'data': '... - file(s) not on client.\n',
/// 'severity': 2, 'generic': 17}`，两个字段都是 int）。int 转成十进制文本，让字典
/// 保持完整；其余类型没在 p4 的输出里见过，宁可报错也不要静默错位。
fn read_marshal_value(cursor: &mut &[u8]) -> Result<Option<Vec<u8>>> {
    let probe = *cursor;

    if probe.is_empty() {
        return Ok(None);
    }

    if probe[0] != TYPE_INT {
        return read_marshal_string(cursor);
    }

    if probe.len() < 5 {
        return Ok(None);
    }
    let number = i32::from_le_bytes([probe[1], probe[2], probe[3], probe[4]]);
    *cursor = &probe[5..];
    Ok(Some(number.to_string().into_bytes()))
}

/// 从 Python marshal 格式里读一个字典。
/// 格式：`'{'`（类型字节）+（键字符串 + 值）* + `'0'`（终止符）。
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
        let Some(value) = read_marshal_value(&mut probe)? else {
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

        sayln!(
            "      Parsed {} have records ({} missing syncTime).",
            self.total_parsed,
            self.missing_sync_time
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

// ---- 通用记录流 ----
//
// `-G have` 有它自己的投影（[`MarshalStreamParser`] 直接产出 have 记录）。普通同步要的是
// 原始记录本身：它的字段（`code` / `depotFile` / `action` / `severity` …）语义由调用方定，
// 而且**解析失败必须是失败**——把半条记录悄悄丢掉，等于让 p4 的答案少几行而工具照报成功。

/// 一条完整的 marshal 记录：字段名 → 原始字节。
#[derive(Debug, Clone, Default)]
pub(crate) struct MarshalRecord {
    fields: HashMap<Vec<u8>, Vec<u8>>,
}

impl MarshalRecord {
    fn from_dict(dict: &HashMap<Vec<u8>, Vec<u8>>) -> Self {
        MarshalRecord {
            fields: dict.clone(),
        }
    }

    /// 字段的原始字节。字段缺席返回 `None`（p4 只写它有的字段，缺席不是错误）。
    pub(crate) fn raw(&self, key: &str) -> Option<&[u8]> {
        self.fields.get(key.as_bytes()).map(Vec::as_slice)
    }

    /// 字段文本，按 p4 配置的字符集解码。
    ///
    /// 解码出替换字符时返回 `Err` 而不是「差不多的文本」：路径一旦对不上本地文件系统，
    /// 后续的范围判断与下发规格会跟着错，而错得看不出来。[`decode_p4_bytes`] 的
    /// windows-1252 兜底对它是最后一道防线——真落到兜底上说明字符集配置本身有问题。
    pub(crate) fn text(&self, key: &str, encoding: &'static Encoding) -> Result<Option<String>> {
        let Some(raw) = self.raw(key) else {
            return Ok(None);
        };

        let (text, had_replacements) = decode_p4_bytes(raw, encoding);
        if had_replacements {
            bail!(
                "p4 returned a {key} value that is not valid {}",
                encoding.name()
            );
        }

        Ok(Some(text.into_owned()))
    }
}

/// 通用 marshal 记录流的增量读取器。
///
/// 与 [`MarshalStreamParser`] 的两点不同：
///
/// - 记录不投影，逐条交给调用方（大响应不必先攒成一份专门的 map）；
/// - **截断是错误**。[`MarshalStreamParser::finish`] 对尾部残记录只警告一句（have 查询
///   少几条只影响时间戳快筛），而普通同步拿这个流当「p4 要对哪些文件做什么」的完整答案，
///   半条记录丢掉就是漏一个文件的动作。
pub(crate) struct MarshalRecordReader {
    encoding: &'static Encoding,
    buffer: Vec<u8>,
    /// `buffer` 里第一个未消费字节的偏移。
    consumed: usize,
    scratch: HashMap<Vec<u8>, Vec<u8>>,
    /// 是否已经见过数据块：BOM 只在最开头剥一次。
    started: bool,
    parsed: usize,
}

impl MarshalRecordReader {
    pub(crate) fn new(encoding: &'static Encoding) -> Self {
        MarshalRecordReader {
            encoding,
            buffer: Vec::new(),
            consumed: 0,
            scratch: HashMap::new(),
            started: false,
            parsed: 0,
        }
    }

    /// 喂一块数据，把其中已经完整的记录逐条交给 `on_record`。
    pub(crate) fn push_chunk(
        &mut self,
        chunk: &[u8],
        on_record: &mut impl FnMut(&MarshalRecord) -> Result<()>,
    ) -> Result<()> {
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
            self.parsed += 1;

            let record = MarshalRecord::from_dict(&self.scratch);
            on_record(&record)?;
        }

        // 缓冲区大部分消费掉之后就压缩一次，避免它随流一直增长。
        if self.consumed >= (1 << 20) && self.consumed * 2 >= self.buffer.len() {
            self.buffer.drain(..self.consumed);
            self.consumed = 0;
        }

        Ok(())
    }

    /// 收尾：还有没消费完的字节就是流被截断了。
    ///
    /// 返回解析出的记录条数，调用方可以拿它区分「p4 什么都没说」与「p4 说了但没解析出来」。
    pub(crate) fn finish(self) -> Result<usize> {
        if self.consumed < self.buffer.len() {
            bail!(
                "p4 output ended in the middle of a record ({} trailing byte(s)); \
                 the result is incomplete.",
                self.buffer.len() - self.consumed
            );
        }

        Ok(self.parsed)
    }
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
        // 键照旧走 `local_path_key`：折叠平台上折成小写，其余平台原样。
        let key = local_path_key("E:\\中文\\私服使用说明.docx");
        let record = records
            .get(&key)
            .expect("key should be the utf8 path under the platform's path identity");
        assert_eq!(record.sync_time, Some(1700000000));
    }

    /// p4 的错误记录带 int 字段：`p4 -G have` 对本地与 depot 都没有的路径返回
    /// `{'code': 'error', 'data': '... - file(s) not on client.\n', 'severity': 2,
    /// 'generic': 17}`。整条流不能因为这几个整数中止，而且后面的记录要照常解析。
    #[test]
    fn dict_values_may_be_marshal_ints() {
        // `marshal_dict` 只造字符串字段，int 字段手工追加在终止符之前。
        let mut data = marshal_dict(&[
            ("code", "error"),
            ("data", "E:\\ws\\gone.txt - file(s) not on client.\n"),
        ]);
        data.pop();
        for (key, number) in [("severity", 2i32), ("generic", 17)] {
            data.extend_from_slice(&marshal_string(key));
            data.push(TYPE_INT);
            data.extend_from_slice(&number.to_le_bytes());
        }
        data.push(TYPE_NULL);

        // 紧跟一条正常记录：int 字段若被读错长度，这一条会解析不出来。
        data.extend_from_slice(&marshal_dict(&[
            ("code", "stat"),
            ("path", "E:\\ws\\kept.txt"),
            ("syncTime", "1700000000"),
        ]));

        let records = parse_p4_have_output(&data, UTF_8).unwrap();

        assert_eq!(records.len(), 1, "错误记录不该产出 have 记录");
        assert!(
            records.contains_key(&local_path_key("E:\\ws\\kept.txt")),
            "{records:?}"
        );
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
        assert!(records.contains_key(&local_path_key("E:\\a.txt")));
    }
}
