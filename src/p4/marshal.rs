//! `p4 -G` 输出使用的 Python marshal 格式解析。

use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use encoding_rs::Encoding;

use crate::charset::{decode_p4_bytes, strip_bom};
use crate::model::HaveRecord;
use crate::path::local_path_key;

// Python marshal format type codes
pub(crate) const TYPE_NULL: u8 = b'0';
pub(crate) const TYPE_DICT: u8 = b'{';
pub(crate) const TYPE_STRING: u8 = b's';

/// Read one string from Python marshal format
/// Format: 's' (type byte) + 4-byte little-endian i32 (length) + N bytes (data)
///
/// Returns `Ok(None)` when the buffer is merely truncated, so a streaming caller can wait for
/// more bytes. `Err` is reserved for data that can never become valid.
pub(crate) fn read_marshal_string(cursor: &mut &[u8]) -> Result<Option<Vec<u8>>> {
    // Probe on a copy so a truncated read leaves the caller's cursor untouched.
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

/// Read one dictionary from Python marshal format
/// Format: '{' (type byte) + (key string + value string)* + '0' (null terminator)
///
/// `dict` is cleared and filled in place so a streaming caller can reuse a single map instead
/// of allocating a new one (plus two `Vec`s per field) for every one of millions of records.
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

        // Check for dict terminator
        if probe[0] == TYPE_NULL {
            probe = &probe[1..];
            break;
        }

        // Read key-value pair
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

/// Extracts one have record from a parsed marshal dict.
/// Returns `Ok(None)` for non-stat records (errors, info messages).
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

    // Paths come back in whatever charset p4 is configured with. Decoding them as anything
    // else silently produces mojibake that never matches the local filesystem.
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

    // Lowercase for case-insensitive matching on Windows, and unify the separator so these keys
    // match the ones built from the local filesystem and from fstat's clientFile.
    Ok(Some((local_path_key(&path_str), HaveRecord { sync_time })))
}

/// Incremental parser for the `p4 -G have` marshal stream.
///
/// The full response reaches several GB on a large workspace, so it is consumed in chunks and
/// parsed record by record. A single scratch dict is reused across all records.
pub(crate) struct MarshalStreamParser {
    encoding: &'static Encoding,
    buffer: Vec<u8>,
    /// Offset of the first unconsumed byte within `buffer`.
    consumed: usize,
    scratch: HashMap<Vec<u8>, Vec<u8>>,
    records: HashMap<String, HaveRecord>,
    total_parsed: usize,
    missing_sync_time: usize,
    /// Whether any chunk has been seen, so a BOM is only stripped once, at the very start.
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
        // A BOM can only ever appear at the very start of the stream.
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
                    // Truncated record: wait for the next chunk.
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

        // Compact once most of the buffer is consumed, so it does not grow with the stream.
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

/// Parses a complete `p4 -G have` response. Kept as a single entry point for tests.
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
        // Non-stat records (errors, info messages) are still skipped.
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
        // Regression guard for the original bug: a wrong charset silently produces a different
        // key, which is what made every non-ASCII file look both added and deleted.
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

        // Every strict prefix means "need more bytes", and must never be an error.
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
        // The chunked parser must produce identical results regardless of how the byte stream
        // is split, which is what makes it safe to read straight off the pipe.
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
        // A final record that gets cut off mid-way, as a killed p4 would produce.
        data.extend_from_slice(&marshal_dict(&[("code", "stat"), ("path", "E:\\b.txt")])[..10]);

        let mut parser = MarshalStreamParser::new(UTF_8);
        parser.push_chunk(&data).unwrap();
        let records = parser.finish();

        // The complete record survives and the truncated one is discarded rather than
        // corrupting the parse.
        assert_eq!(records.len(), 1);
        assert!(records.contains_key("e:\\a.txt"));
    }
}
