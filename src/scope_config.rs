//! `.p4delta-scope` 的严格解析：client root 相对的 JSON 配置 → 一组带类型的条目。
//!
//! 契约（与编辑器侧共享，逐字对齐；语言无关的向量在 `tests/fixtures/scope-contract.json`）：
//!
//! ```json
//! {
//!   "include": [{ "dir": "." }],
//!   "exclude": [{ "dir": "Generated" }, { "file": "local.txt" }]
//! }
//! ```
//!
//! - 每条恰好一个 `dir` 或 `file`：`dir` 含整棵子树，`file` 只精确匹配那一个文件。
//!   类型由**写出来的那个键**决定，不 stat、不按本地存不存在猜。
//! - 省略 `include`（或整份写成 `{}`）＝整个 client root；`include: []` 是**明确的空集**，
//!   与「没写」不是一回事。`exclude` 省略与 `[]` 同义：没有显式排除。
//! - 路径必须是 **client root 相对的本地路径**，分隔符只认 `/`；反斜杠是错误而不是另一种
//!   写法。`..` 按组件消解，任何一步越出 root 都拒绝。
//! - 空文件、`null`、错类型、未知字段、重复键、非法路径一律报错，绝不退化成缺省值——
//!   一条被静默丢掉的 `exclude` 会让范围**放大**，那是 fail-open 的方向。
//!
//! 解析是纯函数：读文件、判 ENOENT、限长与编码校验都在 [`crate::scope`] 那一侧。

use std::fmt;

use anyhow::{Result, anyhow, bail};
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

use crate::scope::EntryKind;

/// 一条配置条目：已归一的 **client root 相对 POSIX 路径**（`"."` 表示 root 自身）+ 类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigEntry {
    pub(crate) path: String,
    pub(crate) kind: EntryKind,
}

impl ConfigEntry {
    fn new(path: String, kind: EntryKind) -> Self {
        ConfigEntry { path, kind }
    }

    /// 面向报错信息的一行说明。
    pub(crate) fn describe(&self) -> String {
        let field = match self.kind {
            EntryKind::Directory => "dir",
            EntryKind::File => "file",
        };
        format!("{field} \"{}\"", self.path)
    }
}

/// 一份解析好的 `.p4delta-scope`。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ScopeConfig {
    /// `None` = 没写 `include`（＝整个 root）；`Some([])` = 写成了空数组（＝明确的空集）。
    pub(crate) include: Option<Vec<ConfigEntry>>,

    pub(crate) exclude: Vec<ConfigEntry>,
}

impl ScopeConfig {
    /// 面向报错信息的整份说明：`include dir ".", exclude dir "gen"`。
    pub(crate) fn describe(&self) -> String {
        let mut parts = Vec::new();
        match &self.include {
            Some(include) if include.is_empty() => parts.push("include <empty>".to_owned()),
            Some(include) => parts.push(format!("include {}", describe_entries(include))),
            None => parts.push("include <the whole client root>".to_owned()),
        }
        if self.exclude.is_empty() {
            parts.push("exclude <none>".to_owned());
        } else {
            parts.push(format!("exclude {}", describe_entries(&self.exclude)));
        }

        parts.join(", ")
    }
}

fn describe_entries(entries: &[ConfigEntry]) -> String {
    entries
        .iter()
        .map(ConfigEntry::describe)
        .collect::<Vec<_>>()
        .join(", ")
}

/// 解析一份配置文件的正文。
pub(crate) fn parse_scope_config(text: &str) -> Result<ScopeConfig> {
    let value: Strict =
        serde_json::from_str(text).map_err(|error| anyhow!("not valid JSON: {error}"))?;

    let object = expect_object(&value, "the config")?;
    for (key, _) in object {
        if !matches!(key.as_str(), "include" | "exclude") {
            bail!(
                "unknown field \"{key}\"; the config accepts only \"include\" and \"exclude\" \
                 (an unrecognized key is usually a typo, and ignoring it would silently change \
                 the scope)"
            );
        }
    }

    let include = match object.iter().find(|(key, _)| key == "include") {
        None => None,
        Some((_, value)) => Some(parse_entries(value, "include")?),
    };
    let exclude = match object.iter().find(|(key, _)| key == "exclude") {
        None => Vec::new(),
        Some((_, value)) => parse_entries(value, "exclude")?,
    };

    Ok(ScopeConfig { include, exclude })
}

fn expect_object<'a>(value: &'a Strict, what: &str) -> Result<&'a Vec<(String, Strict)>> {
    match value {
        Strict::Object(entries) => Ok(entries),
        other => bail!("{what} must be a JSON object, got {}", other.type_name()),
    }
}

/// 条目数组。必须显式写成数组：`null` 与别的类型都不是「没有」——只有缺席才是。
fn parse_entries(value: &Strict, field: &str) -> Result<Vec<ConfigEntry>> {
    let Strict::Array(items) = value else {
        bail!(
            "\"{field}\" must be an array, got {}; write [] when there is nothing in it",
            value.type_name()
        );
    };

    items
        .iter()
        .enumerate()
        .map(|(index, item)| parse_entry(item, &format!("{field}[{index}]")))
        .collect()
}

fn parse_entry(value: &Strict, what: &str) -> Result<ConfigEntry> {
    let object = expect_object(value, what)?;
    for (key, _) in object {
        if !matches!(key.as_str(), "dir" | "file") {
            bail!(
                "{what}: unknown field \"{key}\"; an entry is either {{\"dir\": …}} or {{\"file\": …}}"
            );
        }
    }

    let dir = object.iter().find(|(key, _)| key == "dir");
    let file = object.iter().find(|(key, _)| key == "file");
    let (field, kind, raw) = match (dir, file) {
        (Some((_, raw)), None) => ("dir", EntryKind::Directory, raw),
        (None, Some((_, raw))) => ("file", EntryKind::File, raw),
        (Some(_), Some(_)) => bail!(
            "{what}: exactly one of \"dir\" or \"file\" is allowed; two of them would make the \
             type a guess, and a guess can widen the scope"
        ),
        (None, None) => bail!("{what}: expected exactly one of \"dir\" or \"file\""),
    };

    let Strict::String(raw) = raw else {
        bail!(
            "{what}: \"{field}\" must be a string, got {}",
            raw.type_name()
        );
    };

    Ok(ConfigEntry::new(
        normalize_config_path(raw, kind).map_err(|error| anyhow!("{what}: {error}"))?,
        kind,
    ))
}

/// 把配置里的路径归一成 **client root 相对 POSIX 路径**（`"."` 表示 root 自身）。
///
/// 自己的组件解析而不是交给 `Path`：配置的语法是固定的 POSIX 形状，与跑在哪台机器上无关——
/// 交给平台路径 API 会让同一份配置在两个平台上语义不同（Windows 上 `\` 变成分隔符、
/// 盘符被当根），而这份配置是要在编辑器与 CLI 之间共享的。
fn normalize_config_path(raw: &str, kind: EntryKind) -> Result<String> {
    if raw.is_empty() {
        return Err(anyhow!("empty path"));
    }
    if raw.contains('\0') {
        return Err(anyhow!("\"{raw}\" contains a NUL byte"));
    }
    if raw.contains('\\') {
        return Err(anyhow!(
            "\"{raw}\" contains a backslash; the config uses \"/\" as its only separator"
        ));
    }
    if raw.starts_with('/') {
        return Err(anyhow!(
            "\"{raw}\" is an absolute path; config entries are relative to the client root"
        ));
    }
    let mut characters = raw.chars();
    if let (Some(first), Some(':')) = (characters.next(), characters.next())
        && first.is_ascii_alphabetic()
    {
        return Err(anyhow!(
            "\"{raw}\" starts with a drive letter; config entries are relative to the client root"
        ));
    }
    if let Some(wildcard) = raw.chars().find(|c| matches!(c, '*' | '?')) {
        return Err(anyhow!(
            "\"{raw}\" contains the wildcard \"{wildcard}\"; config entries are literal local \
             paths, one per \"dir\"/\"file\" entry"
        ));
    }

    let mut components: Vec<&str> = Vec::new();
    for component in raw.split('/') {
        match component {
            // 重复分隔符与 `.` 都归一掉。
            "" | "." => continue,
            ".." => {
                if components.pop().is_none() {
                    return Err(anyhow!(
                        "\"{raw}\" steps above the client root; every step of the path must stay \
                         inside it"
                    ));
                }
            }
            "..." => {
                return Err(anyhow!(
                    "\"{raw}\" contains \"...\"; that is p4's recursive wildcard, not a path \
                     component. A directory is written as {{\"dir\": \"…\"}} — the type is \
                     declared, not spelled with a suffix"
                ));
            }
            other => components.push(other),
        }
    }

    if components.is_empty() {
        return match kind {
            EntryKind::Directory => Ok(".".to_owned()),
            EntryKind::File => Err(anyhow!(
                "\"{raw}\" names the client root itself, which is a directory, not a file"
            )),
        };
    }

    Ok(components.join("/"))
}

// ---- 严格 JSON：拒掉重复键 ----

/// 一个保留了「对象里出现过哪些键」的 JSON 值。
///
/// 用 `serde_json::Value` 的话后来的键会直接盖掉前一个，而配置里一个重复的 `include`
/// 会把整份范围换成另一个——静默放大范围，正是这套契约要 fail closed 的方向。
#[derive(Debug, Clone, PartialEq)]
enum Strict {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Strict>),
    Object(Vec<(String, Strict)>),
}

impl Strict {
    fn type_name(&self) -> &'static str {
        match self {
            Strict::Null => "null",
            Strict::Bool(_) => "a boolean",
            Strict::Number(_) => "a number",
            Strict::String(_) => "a string",
            Strict::Array(_) => "an array",
            Strict::Object(_) => "an object",
        }
    }
}

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Strict;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Strict, E> {
        Ok(Strict::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Strict, E> {
        Ok(Strict::Number(value as f64))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Strict, E> {
        Ok(Strict::Number(value as f64))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Strict, E> {
        Ok(Strict::Number(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<Strict, E> {
        Ok(Strict::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Strict, E> {
        Ok(Strict::String(value))
    }

    fn visit_none<E>(self) -> Result<Strict, E> {
        Ok(Strict::Null)
    }

    fn visit_unit<E>(self) -> Result<Strict, E> {
        Ok(Strict::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Strict, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = sequence.next_element::<Strict>()? {
            items.push(item);
        }
        Ok(Strict::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Strict, A::Error> {
        let mut entries: Vec<(String, Strict)> = Vec::new();
        while let Some((key, value)) = map.next_entry::<String, Strict>()? {
            if entries.iter().any(|(existing, _)| *existing == key) {
                return Err(de::Error::custom(format!(
                    "duplicate key \"{key}\"; the config is read as written, so a repeated key \
                     would silently replace the earlier one"
                )));
            }
            entries.push((key, value));
        }
        Ok(Strict::Object(entries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, kind: EntryKind) -> ConfigEntry {
        ConfigEntry::new(path.to_owned(), kind)
    }

    fn parse(text: &str) -> ScopeConfig {
        parse_scope_config(text).unwrap_or_else(|error| panic!("{text}\n  -> {error}"))
    }

    fn error(text: &str) -> String {
        parse_scope_config(text)
            .expect_err("这一条必须是错误")
            .to_string()
    }

    #[test]
    fn a_minimal_config_parses() {
        let config = parse(r#"{"include": [{"dir": "."}]}"#);
        assert_eq!(config.include, Some(vec![entry(".", EntryKind::Directory)]));
        assert!(config.exclude.is_empty());
    }

    #[test]
    fn an_empty_object_means_the_whole_root() {
        let config = parse("{}");
        assert_eq!(config.include, None);
        assert!(config.exclude.is_empty());
    }

    /// 空数组与缺席是两回事：一个说「什么都不要」，一个说「整个 root」。
    #[test]
    fn an_explicitly_empty_include_is_not_the_same_as_a_missing_one() {
        assert_eq!(parse(r#"{"include": []}"#).include, Some(Vec::new()));
        assert_eq!(parse("{}").include, None);
    }

    #[test]
    fn both_kinds_and_both_lists_parse() {
        let config = parse(
            r#"{"include": [{"dir": "src"}, {"file": "readme.txt"}],
                "exclude": [{"dir": "Generated"}, {"file": "local.txt"}]}"#,
        );
        assert_eq!(
            config.include,
            Some(vec![
                entry("src", EntryKind::Directory),
                entry("readme.txt", EntryKind::File),
            ])
        );
        assert_eq!(
            config.exclude,
            vec![
                entry("Generated", EntryKind::Directory),
                entry("local.txt", EntryKind::File),
            ]
        );
    }

    /// 合法文件名里的空格、中文、分号与 p4 元字符原样保留：它们是**本地路径**，不是文本
    /// 语法的一部分，转义在 p4 边界上做一次（见 `ScopeEntry::file_spec`）。
    #[test]
    fn legal_file_names_survive_verbatim() {
        let config = parse(r#"{"include": [{"file": "使用 说明#1@x%.txt"}, {"dir": "a;b"}]}"#);
        assert_eq!(
            config.include,
            Some(vec![
                entry("使用 说明#1@x%.txt", EntryKind::File),
                entry("a;b", EntryKind::Directory),
            ])
        );
    }

    #[test]
    fn duplicate_keys_are_refused() {
        let message = error(r#"{"include": [{"dir": "a"}], "include": [{"dir": "."}]}"#);
        assert!(message.contains("duplicate key \"include\""), "{message}");

        let message = error(r#"{"include": [{"dir": "a", "dir": "b"}]}"#);
        assert!(message.contains("duplicate key"), "{message}");
    }

    #[test]
    fn unknown_fields_are_refused() {
        let message = error(r#"{"include": [{"dir": "."}], "focus": ["src"]}"#);
        assert!(message.contains("unknown field \"focus\""), "{message}");

        let message = error(r#"{"include": [{"dir": ".", "wildcard": true}]}"#);
        assert!(message.contains("unknown field \"wildcard\""), "{message}");

        // `[...]` 这种「看着像旧文本格式」的写法也在这一档被挡住。
        let message = error(r#"{"dir": "src"}"#);
        assert!(message.contains("unknown field \"dir\""), "{message}");
    }

    #[test]
    fn an_entry_needs_exactly_one_type_key() {
        let message = error(r#"{"include": [{"dir": "a", "file": "b"}]}"#);
        assert!(message.contains("exactly one"), "{message}");

        let message = error(r#"{"include": [{}]}"#);
        assert!(message.contains("exactly one"), "{message}");

        let message = error(r#"{"include": [{"dir": 7}]}"#);
        assert!(message.contains("\"dir\" must be a string"), "{message}");
    }

    #[test]
    fn wrong_container_types_are_refused() {
        for text in [
            "null",
            "[]",
            "\"src\"",
            "7",
            r#"{"include": "src"}"#,
            r#"{"include": null}"#,
            r#"{"exclude": {"dir": "gen"}}"#,
        ] {
            let message = error(text);
            assert!(
                message.contains("must be a JSON object") || message.contains("must be an array"),
                "{text}: {message}"
            );
        }

        // 语法错误也要点名是 JSON 的问题，而不是含糊的「解析失败」。
        assert!(error("{").contains("not valid JSON"));
        // 空文件是错误，不是「没有配置」。
        assert!(error("").contains("not valid JSON"));
    }

    #[test]
    fn paths_are_normalized_component_wise() {
        let config = parse(r#"{"include": [{"dir": "./src//deep/./"}, {"dir": "a/../b"}]}"#);
        assert_eq!(
            config.include,
            Some(vec![
                entry("src/deep", EntryKind::Directory),
                entry("b", EntryKind::Directory),
            ])
        );

        // 归一之后落到 root 自身的几种写法。
        assert_eq!(
            parse(r#"{"include": [{"dir": "a/.."}]}"#).include,
            Some(vec![entry(".", EntryKind::Directory)])
        );
        assert_eq!(
            parse(r#"{"include": [{"dir": "./"}]}"#).include,
            Some(vec![entry(".", EntryKind::Directory)])
        );
    }

    #[test]
    fn illegal_paths_are_refused() {
        for (text, expected) in [
            (r#"{"include": [{"dir": ""}]}"#, "empty path"),
            (r#"{"include": [{"dir": "/abs"}]}"#, "is an absolute path"),
            (r#"{"include": [{"dir": "C:/ws"}]}"#, "drive letter"),
            (r#"{"include": [{"dir": "src\\deep"}]}"#, "backslash"),
            (
                r#"{"include": [{"dir": ".."}]}"#,
                "steps above the client root",
            ),
            (
                r#"{"include": [{"dir": "a/../../b"}]}"#,
                "steps above the client root",
            ),
            (r#"{"include": [{"dir": "src/*.txt"}]}"#, "wildcard \"*\""),
            (r#"{"include": [{"dir": "src/..."}]}"#, "\"...\""),
            (
                r#"{"include": [{"file": "."}]}"#,
                "names the client root itself",
            ),
            (
                r#"{"include": [{"file": "./"}]}"#,
                "names the client root itself",
            ),
        ] {
            let message = error(text);
            assert!(message.contains(expected), "{text}: {message}");
        }
    }

    #[test]
    fn the_description_lists_what_is_in_effect() {
        assert_eq!(
            parse(r#"{"exclude": [{"dir": "gen"}]}"#).describe(),
            "include <the whole client root>, exclude dir \"gen\""
        );
        assert_eq!(
            parse(r#"{"include": []}"#).describe(),
            "include <empty>, exclude <none>"
        );
    }
}
