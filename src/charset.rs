//! p4 输出字符集的解析与解码。
//!
//! p4 会按 P4CHARSET 翻译它输出的所有元数据，用别的字符集解码会把非 ASCII 路径
//! 变成永远无法与本地文件系统匹配的乱码。这里集中处理字符集的解析、缓存与字节解码。

use std::borrow::Cow;
use std::env;
use std::path::Path;
use std::sync::OnceLock;

use crate::json::sayln;
use encoding_rs::{Encoding, UTF_8, WINDOWS_1252};

/// p4 写输出用的字符集，启动时解析一次。
/// p4 按 P4CHARSET 翻译它输出的全部元数据，用别的字符集解码会把非 ASCII 路径
/// 变成永远匹配不上本地文件系统的乱码。
static P4_ENCODING: OnceLock<&'static Encoding> = OnceLock::new();

/// 解码 p4 输出用的字符集。从未初始化时回落到 UTF-8。
pub(crate) fn p4_encoding() -> &'static Encoding {
    P4_ENCODING.get().copied().unwrap_or(UTF_8)
}

/// p4 解码命令行参数（包括 `-x` 送上来的那一批）用的字符集，同样只在启动时解析一次。
///
/// 它未必等于内容字符集：设了 `P4COMMANDCHARSET` 时 p4 用它解参数、用 `P4CHARSET` 译文件内容。
/// 按内容字符集编码参数会让非 ASCII 路径变成谁也匹配不上的乱码，所以参数必须按这一个来编。
/// （不用 `p4 -Q` 把两者强行统一：`-Q` 改的是 p4 生效的命令字符集，输出编码会跟着变，
/// 而输出仍按内容字符集解码——那等于把问题从参数搬到输出上。）
static P4_COMMAND_ENCODING: OnceLock<&'static Encoding> = OnceLock::new();

/// p4 解码参数用的字符集。回落到内容字符集——`P4COMMANDCHARSET` 没设时 p4 自己就是这么做的。
pub(crate) fn p4_command_encoding() -> &'static Encoding {
    P4_COMMAND_ENCODING
        .get()
        .copied()
        .unwrap_or_else(|| p4_encoding())
}

/// 解析并缓存 p4 的两套字符集。幂等；先到者胜。
/// `cwd` 有意义是因为 p4 会在自己的工作目录里找 `.p4config`。
pub(crate) fn init_p4_encoding(explicit: Option<&str>, cwd: &Path) -> &'static Encoding {
    if let Some(encoding) = P4_ENCODING.get() {
        return encoding;
    }

    let env_charset = non_empty_env("P4CHARSET");
    let env_command_charset = non_empty_env("P4COMMANDCHARSET");

    // 环境变量已经给出答案时不去跑 `p4 set`：它要起一个 p4 进程。p4 的参数优先级里
    // .p4config 能盖过环境变量，这条兜底路径由这个查询负责。
    let settings = if (explicit.is_some() || env_charset.is_some()) && env_command_charset.is_some()
    {
        P4SetCharsets::default()
    } else {
        query_p4_set_charsets(cwd)
    };

    let (encoding, source) = resolve_p4_charset(explicit, env_charset.as_deref(), &settings);
    let _ = P4_ENCODING.set(encoding);
    sayln!("Using p4 charset {} ({}).", encoding.name(), source);

    let (command_encoding, command_source) =
        resolve_command_charset(env_command_charset.as_deref(), &settings, encoding);
    let _ = P4_COMMAND_ENCODING.set(command_encoding);
    // 两者一致时不打第二行：这是绝大多数情况，多说一句只是噪音。
    if command_encoding != encoding {
        sayln!(
            "Using p4 command charset {} ({}).",
            command_encoding.name(),
            command_source
        );
    }

    p4_encoding()
}

/// 读环境变量，空串按「没设」处理——p4 对空值的处理和未设一样。
/// 值取不到（非 UTF-8）时也当没设，与遍历字符集表达不了的字符时同一个态度：宁可回落。
fn non_empty_env(name: &str) -> Option<String> {
    let value = env::var(name).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// 挑出 p4 正在用的字符集，顺序照抄 p4 解析自己设置的顺序：显式选项、环境变量、`p4 set`。
/// 端口级变量只在通用变量缺席时才看，与观察到的行为一致。
///
/// 环境变量与 `p4 set` 的值都由调用方传进来，不是为了好看：单元测试里改进程环境是全局操作，
/// 会和并行跑的用例互相踩。
fn resolve_p4_charset(
    explicit: Option<&str>,
    from_env: Option<&str>,
    settings: &P4SetCharsets,
) -> (&'static Encoding, String) {
    if let Some(name) = explicit {
        return match p4_charset_to_encoding(name) {
            Some(encoding) => (encoding, format!("--charset {}", name)),
            None => (
                UTF_8,
                format!("--charset {} (unknown charset, using utf8)", name),
            ),
        };
    }

    if let Some(name) = from_env
        && let Some(encoding) = p4_charset_to_encoding(name)
    {
        return (
            encoding,
            format!("P4CHARSET environment variable: {}", name),
        );
    }

    if let Some(name) = &settings.charset
        && let Some(encoding) = p4_charset_to_encoding(name)
    {
        return (encoding, format!("p4 set P4CHARSET={}", name));
    }

    (UTF_8, "default utf8, P4CHARSET is not set".to_owned())
}

/// 参数与输出的字符集。p4 自己的规则是「`P4COMMANDCHARSET` 缺省跟随 `P4CHARSET`」，
/// 这里照抄：所以回落到内容字符集，而不是再读一遍 P4CHARSET，
/// 用户用 `--charset` 显式告诉我们的值也能一并生效。
fn resolve_command_charset(
    from_env: Option<&str>,
    settings: &P4SetCharsets,
    content: &'static Encoding,
) -> (&'static Encoding, String) {
    if let Some(name) = from_env
        && let Some(encoding) = p4_charset_to_encoding(name)
    {
        return (
            encoding,
            format!("P4COMMANDCHARSET environment variable: {}", name),
        );
    }

    if let Some(name) = &settings.command_charset
        && let Some(encoding) = p4_charset_to_encoding(name)
    {
        return (encoding, format!("p4 set P4COMMANDCHARSET={}", name));
    }

    (content, "defaults to the content charset".to_owned())
}

/// `p4 set` 里与字符集有关的两个变量。
#[derive(Default)]
struct P4SetCharsets {
    /// `P4CHARSET`，或端口级的 `P4_<port>_CHARSET` 兜底。
    charset: Option<String>,

    /// `P4COMMANDCHARSET`：p4 用来解码参数、编码输出的字符集。
    command_charset: Option<String>,
}

/// 从 `p4 set` 读回字符集。这样配的值在注册表里而不在环境变量里，`env::var` 看不到。
fn query_p4_set_charsets(cwd: &Path) -> P4SetCharsets {
    // 定位不到 p4 与起不来是同一件事，都退回默认字符集——这里不该打断整轮，
    // P4_EXE 配错由入口处的前置校验负责报出来。
    let Ok(program) = crate::locate::p4_exe() else {
        return P4SetCharsets::default();
    };

    let output = std::process::Command::new(program)
        .arg("set")
        .current_dir(cwd)
        // p4 会用继承来的 PWD 而不是真实 cwd 找配置文件，必须清掉。
        .env_remove("PWD")
        .output();

    let Ok(output) = output else {
        return P4SetCharsets::default();
    };

    let text = String::from_utf8_lossy(&output.stdout);

    parse_p4_set_charsets(&text)
}

/// 从 `p4 set` 输出里取两个字符集变量。
fn parse_p4_set_charsets(text: &str) -> P4SetCharsets {
    P4SetCharsets {
        // 端口级的变量只在通用变量缺席时兜底，与观察到的行为一致。
        charset: parse_p4_set_value(text, "P4CHARSET").or_else(|| port_level_charset(text)),
        command_charset: parse_p4_set_value(text, "P4COMMANDCHARSET"),
    }
}

/// 取端口级 `P4_<port>_CHARSET`（例如 `P4_1666_CHARSET`）的值。
fn port_level_charset(text: &str) -> Option<String> {
    let mut port_level = None;

    for line in text.lines() {
        let Some((key, rest)) = line.split_once('=') else {
            continue;
        };

        let key = key.trim();
        if key.starts_with("P4_") && key.ends_with("_CHARSET") {
            // "utf8 (set)" 这样的来源说明不属于变量值。
            port_level = Some(rest.split(" (").next().unwrap_or("").trim().to_owned());
        }
    }

    port_level
}

/// 把 `P4CHARSET` 的值映射成编码。p4 的名字不带连字符、多数不是 WHATWG 标签，
/// 所以大部分要走下面这张显式映射表。`None` 表示「认不出」，调用方应当回落。
///
/// 兜底用 `Encoding::for_label_no_replacement` 而不是 `for_label`：后者连 `replacement`
/// 这个标签也认成一种编码，而那个「编码」解码出来全是 U+FFFD，等于把文件名整个毁掉。
pub(crate) fn p4_charset_to_encoding(name: &str) -> Option<&'static Encoding> {
    use encoding_rs::*;

    let name = name.trim().trim_matches('"').to_ascii_lowercase();

    let mapped = match name.as_str() {
        // "none" 关掉翻译，"auto" 由 p4 按系统区域解析。两者都不告诉我们真实的字节编码，
        // 所以交给调用方回落。
        "none" | "auto" | "" => return None,
        // encoding_rs 不支持 UTF-32；宁可拒绝，也不要静默毁掉路径。
        "utf32" | "utf32-nobom" | "utf32le" | "utf32le-bom" | "utf32be" | "utf32be-bom" => {
            return None;
        }

        "utf8" | "utf-8" | "utf8-bom" | "utf8bom" | "utf-8-bom" => UTF_8,
        "utf16" | "utf16-nobom" | "utf16le" | "utf16le-bom" => UTF_16LE,
        "utf16be" | "utf16be-bom" => UTF_16BE,

        "winansi" | "cp1252" => WINDOWS_1252,
        "cp1250" => WINDOWS_1250,
        "cp1251" => WINDOWS_1251,
        "cp1253" => WINDOWS_1253,
        "cp1254" => WINDOWS_1254,
        "cp1255" => WINDOWS_1255,
        "cp1256" => WINDOWS_1256,
        "cp1257" => WINDOWS_1257,
        "cp1258" => WINDOWS_1258,
        "cp936" => GBK,    // Simplified Chinese
        "cp950" => BIG5,   // Traditional Chinese
        "cp949" => EUC_KR, // Korean
        "shiftjis" => SHIFT_JIS,
        "eucjp" => EUC_JP,
        "macosroman" => MACINTOSH,
        "koi8-r" => KOI8_R,
        "iso8859-1" => WINDOWS_1252, // WHATWG 把 ISO-8859-1 当作 windows-1252 的别名
        "iso8859-2" => ISO_8859_2,
        "iso8859-5" => ISO_8859_5,
        "iso8859-7" => ISO_8859_7,
        "iso8859-15" => ISO_8859_15,

        // cp850/cp852/cp858 及其余 iso8859 变体在 encoding_rs 里没有对应项。
        // 最后再按 WHATWG 标签试一次。
        _ => return Encoding::for_label_no_replacement(name.as_bytes()),
    };

    Some(mapped)
}

/// 把 p4 输出的字节解码成文本，同时报告有没有产生替换字符。
/// 那个标志是唯一可靠的「字符集用错」信号：encoding_rs 从不返回错误，
/// 单字节编码里每个字节都有对应的字符，永远谈不上「非法」。
pub(crate) fn decode_p4_bytes<'a>(
    bytes: &'a [u8],
    encoding: &'static Encoding,
) -> (Cow<'a, str>, bool) {
    // UTF-8 是绝大多数情况。先严格解码一次，好让使用旧的非 Unicode 客户端的场景
    // 退回此前的 windows-1252 行为，而不是吐出一堆 U+FFFD。
    if encoding == UTF_8 {
        return match std::str::from_utf8(bytes) {
            Ok(text) => (Cow::Borrowed(text), false),
            Err(_) => WINDOWS_1252.decode_without_bom_handling(bytes),
        };
    }

    // 不用 `decode`：它的 BOM 嗅探会覆盖调用方指定的编码，碰到恰好以 EF BB BF 开头的
    // cp936 流就会读错。BOM 改由 [`strip_bom`] 显式剥掉。
    encoding.decode_without_bom_handling(bytes)
}

/// 剥掉与 `encoding` 相符的字节序标记：p4 配了带 `-bom` 的字符集时会写它。
/// 留着会污染响应里的第一个键。
pub(crate) fn strip_bom<'a>(bytes: &'a [u8], encoding: &'static Encoding) -> &'a [u8] {
    match Encoding::for_bom(bytes) {
        Some((bom_encoding, len)) if bom_encoding == encoding => &bytes[len..],
        _ => bytes,
    }
}

/// 剥掉末尾的一个换行。与 `std::io::BufRead::lines` 相近但不完全相同：这里是先剥 `\n`
/// 再剥 `\r`，所以对没有 `\n` 结尾的 `"value\r"` 也会剥掉那个 `\r`，而 `lines` 不会。
///
/// p4 写的是 CRLF：留着 `\r` 会污染解析出来的数字，也会让 `p4 ignores` 输出上的定宽切片
/// 整体错位。
pub(crate) fn trim_line_ending(bytes: &[u8]) -> &[u8] {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    bytes.strip_suffix(b"\r").unwrap_or(bytes)
}

/// 从 `p4 set` 输出里取变量值，例如 `P4IGNORE=.p4ignore (config 'file')`。
pub(crate) fn parse_p4_set_value(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        let Some((key, rest)) = line.split_once('=') else {
            continue;
        };

        if key.trim() != name {
            continue;
        }

        // 括号里是来源说明，不属于变量值。
        let value = rest.split(" (").next().unwrap_or("").trim();
        if !value.is_empty() {
            return Some(value.to_owned());
        }
    }

    None
}

/// 查询 p4 在目标目录解析出的变量值。
/// 必须用 `p4 set` 的生效值：p4 自己的优先级是 .p4config > 环境变量/注册表，
/// 直接读环境变量会在配置被 .p4config 覆盖时拿到错误的值。
/// （`p4 set` 也会打印只由环境变量提供的值，所以不需要再单独读环境变量。）
pub(crate) fn query_p4_variable(cwd: &Path, name: &str) -> Option<String> {
    let program = crate::locate::p4_exe().ok()?;

    let output = std::process::Command::new(program)
        .arg("set")
        .current_dir(cwd)
        // p4 会用继承来的 PWD 而不是真实 cwd 找配置文件，必须清掉。
        .env_remove("PWD")
        .output()
        .ok()?;

    // 失败时 stdout 可能只有半截内容，宁可不剪枝也不能拿它做判断。
    if !output.status.success() {
        return None;
    }

    parse_p4_set_value(&String::from_utf8_lossy(&output.stdout), name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;

    use encoding_rs::{UTF_8, WINDOWS_1252};

    use crate::prune::P4IGNORE_FILE_NAME;

    #[test]
    fn decodes_utf8_paths() {
        let (text, had_replacements) = decode_p4_bytes(CHINESE_NAME_UTF8, UTF_8);
        assert_eq!(text, "使用说明.txt");
        assert!(!had_replacements);
    }

    #[test]
    fn windows_1252_decoding_produces_mojibake() {
        // 正是这次修复要消除的那种损坏。同一串字节按 windows-1252 读出来，
        // 永远不等于本地文件系统报告的名字——这就是每个中文文件过去既像新增又像删除的原因。
        let (text, _) = decode_p4_bytes(CHINESE_NAME_UTF8, WINDOWS_1252);
        assert_eq!(text, "ä½¿ç”¨è¯´æ˜Ž.txt");
        assert_ne!(text, "使用说明.txt");
    }

    #[test]
    fn invalid_utf8_falls_back_to_windows_1252() {
        // 使用旧的非 Unicode 客户端。退回此前行为，好过吐出一堆 U+FFFD。
        let (text, had_replacements) = decode_p4_bytes(b"caf\xe9.txt", UTF_8);
        assert_eq!(text, "café.txt");
        assert!(!had_replacements);
    }

    #[test]
    fn charset_names_map_to_encodings() {
        use encoding_rs::*;

        assert_eq!(p4_charset_to_encoding("utf8"), Some(UTF_8));
        assert_eq!(p4_charset_to_encoding("UTF8"), Some(UTF_8));
        assert_eq!(p4_charset_to_encoding("utf8-bom"), Some(UTF_8));
        assert_eq!(p4_charset_to_encoding(" cp936 "), Some(GBK));
        assert_eq!(p4_charset_to_encoding("\"cp950\""), Some(BIG5));
        assert_eq!(p4_charset_to_encoding("cp949"), Some(EUC_KR));
        assert_eq!(p4_charset_to_encoding("shiftjis"), Some(SHIFT_JIS));
        assert_eq!(p4_charset_to_encoding("eucjp"), Some(EUC_JP));
        assert_eq!(p4_charset_to_encoding("winansi"), Some(WINDOWS_1252));
        assert_eq!(p4_charset_to_encoding("utf16le"), Some(UTF_16LE));
        assert_eq!(p4_charset_to_encoding("utf16be"), Some(UTF_16BE));
    }

    #[test]
    fn unmappable_charsets_are_rejected() {
        // p4 按系统区域解析这几个，我们无从知道真实字节编码。
        assert_eq!(p4_charset_to_encoding("auto"), None);
        assert_eq!(p4_charset_to_encoding("none"), None);
        assert_eq!(p4_charset_to_encoding(""), None);
        // encoding_rs 没有 UTF-32 解码器。
        assert_eq!(p4_charset_to_encoding("utf32le"), None);
    }

    /// p4 的字符集名不带连字符，多数不是 WHATWG 标签，所以必须逐条显式映射。
    #[test]
    fn windows_code_pages_and_legacy_charsets_map_to_their_encodings() {
        use encoding_rs::*;

        let cases: [(&str, &'static Encoding); 16] = [
            ("cp1250", WINDOWS_1250),
            ("cp1251", WINDOWS_1251),
            ("cp1252", WINDOWS_1252),
            ("cp1253", WINDOWS_1253),
            ("cp1254", WINDOWS_1254),
            ("cp1255", WINDOWS_1255),
            ("cp1256", WINDOWS_1256),
            ("cp1257", WINDOWS_1257),
            ("cp1258", WINDOWS_1258),
            ("macosroman", MACINTOSH),
            ("koi8-r", KOI8_R),
            // WHATWG 把 ISO-8859-1 当作 windows-1252 的别名，这里跟随它。
            ("iso8859-1", WINDOWS_1252),
            ("iso8859-2", ISO_8859_2),
            ("iso8859-5", ISO_8859_5),
            ("iso8859-7", ISO_8859_7),
            ("iso8859-15", ISO_8859_15),
        ];

        for (name, expected) in cases {
            assert_eq!(p4_charset_to_encoding(name), Some(expected), "{name}");
        }
    }

    #[test]
    fn utf_variants_map_to_their_byte_order() {
        use encoding_rs::*;

        for name in ["utf-8", "utf8bom", "utf-8-bom"] {
            assert_eq!(p4_charset_to_encoding(name), Some(UTF_8), "{name}");
        }
        for name in ["utf16", "utf16-nobom", "utf16le-bom"] {
            assert_eq!(p4_charset_to_encoding(name), Some(UTF_16LE), "{name}");
        }
        assert_eq!(p4_charset_to_encoding("utf16be-bom"), Some(UTF_16BE));
        // encoding_rs 没有 UTF-32 解码器，整个家族都要拒。
        for name in [
            "utf32",
            "utf32-nobom",
            "utf32le-bom",
            "utf32be",
            "utf32be-bom",
        ] {
            assert_eq!(p4_charset_to_encoding(name), None, "{name}");
        }
    }

    /// 既不在表里、又不是 WHATWG 标签的名字宁可拒绝，也不猜一个错的：
    /// 用错的字符集解码会让每个非 ASCII 文件名既像新增又像删除。
    #[test]
    fn charsets_without_an_encoding_are_rejected_rather_than_misdecoded() {
        for name in ["cp850", "cp852", "cp858", "not-a-charset"] {
            assert_eq!(p4_charset_to_encoding(name), None, "{name}");
        }
    }

    #[test]
    fn unknown_charsets_fall_back_to_whatwg_labels() {
        assert_eq!(
            p4_charset_to_encoding("gb18030"),
            Some(encoding_rs::GB18030)
        );
    }

    // ---- 生效字符集的解析 ----

    /// 优先级照抄 p4：命令行 > 环境变量 > `p4 set` > 默认 utf8。
    #[test]
    fn charset_resolution_follows_p4s_own_precedence() {
        use encoding_rs::*;

        let settings = P4SetCharsets {
            charset: Some("cp936".to_owned()),
            command_charset: None,
        };

        assert_eq!(
            resolve_p4_charset(Some("shiftjis"), Some("eucjp"), &settings).0,
            SHIFT_JIS
        );
        assert_eq!(resolve_p4_charset(None, Some("eucjp"), &settings).0, EUC_JP);
        assert_eq!(resolve_p4_charset(None, None, &settings).0, GBK);
        assert_eq!(
            resolve_p4_charset(None, None, &P4SetCharsets::default()).0,
            UTF_8
        );
    }

    /// 认不出的名字不猜：显式指定的退回 utf8，其余来源继续往下找。
    /// `auto` 由 p4 按系统区域决定，我们无从知道真实字节编码。
    #[test]
    fn unknown_charset_names_do_not_stop_the_search() {
        use encoding_rs::*;

        let settings = P4SetCharsets {
            charset: Some("cp936".to_owned()),
            command_charset: None,
        };

        assert_eq!(resolve_p4_charset(Some("auto"), None, &settings).0, UTF_8);
        assert_eq!(resolve_p4_charset(None, Some("auto"), &settings).0, GBK);
    }

    /// 命令字符集缺省跟随内容字符集（p4 自己的规则），设了 `P4COMMANDCHARSET` 则它优先。
    /// 用内容字符集编码、命令字符集解码时，非 ASCII 路径会变成谁也匹配不上的乱码。
    #[test]
    fn command_charset_defaults_to_the_content_charset() {
        use encoding_rs::*;

        let none = P4SetCharsets::default();

        assert_eq!(resolve_command_charset(None, &none, GBK).0, GBK);
        // 环境变量压过 `p4 set`，与 p4 的优先级一致。
        assert_eq!(resolve_command_charset(Some("cp950"), &none, GBK).0, BIG5);

        let from_set = P4SetCharsets {
            charset: None,
            command_charset: Some("shiftjis".to_owned()),
        };
        assert_eq!(resolve_command_charset(None, &from_set, GBK).0, SHIFT_JIS);
    }

    /// 端口级 `P4_<port>_CHARSET` 只在通用变量缺席时兜底，且来源说明不属于变量值。
    #[test]
    fn port_level_charset_is_the_last_resort() {
        use encoding_rs::*;

        let text = "P4_1666_CHARSET=cp936 (set)\nP4CHARSET=utf8 (config 'C:/ws/.p4config')\n";
        assert_eq!(
            parse_p4_set_value(text, "P4CHARSET"),
            Some("utf8".to_owned())
        );
        assert_eq!(port_level_charset(text), Some("cp936".to_owned()));

        // 通用变量缺席时才轮到它。
        assert_eq!(
            resolve_p4_charset(
                None,
                None,
                &P4SetCharsets {
                    charset: port_level_charset(text),
                    command_charset: None,
                }
            )
            .0,
            GBK
        );

        assert_eq!(port_level_charset("P4CHARSET=utf8 (set)\n"), None);
    }

    /// `p4 set` 的两个变量从同一份输出里取回：每跑一次就是一个 p4 进程。
    #[test]
    fn p4_set_query_collects_both_charsets() {
        let both = parse_p4_set_charsets("P4CHARSET=utf8 (set)\nP4COMMANDCHARSET=cp936 (set)\n");
        assert_eq!(both.charset, Some("utf8".to_owned()));
        assert_eq!(both.command_charset, Some("cp936".to_owned()));

        // `P4COMMANDCHARSET` 缺席时不拿端口级变量顶替：那个兜底是给内容字符集的。
        let content_only = parse_p4_set_charsets("P4_1666_CHARSET=cp936 (set)\n");
        assert_eq!(content_only.charset, Some("cp936".to_owned()));
        assert_eq!(content_only.command_charset, None);

        assert_eq!(
            parse_p4_set_charsets("P4CHARSET=\n").charset,
            None,
            "空值等于没设"
        );
    }

    #[test]
    fn trims_line_endings() {
        // p4 写的是 CRLF。留着 \r 会污染数字字段，也会让 `p4 ignores` 输出上的定宽切片错位。
        assert_eq!(trim_line_ending(b"value\r\n"), b"value");
        assert_eq!(trim_line_ending(b"value\n"), b"value");
        assert_eq!(trim_line_ending(b"value"), b"value");
        assert_eq!(trim_line_ending(b"\r\n"), b"");
        assert_eq!(trim_line_ending(b""), b"");
    }

    #[test]
    fn strips_only_a_matching_bom() {
        assert_eq!(strip_bom(b"\xef\xbb\xbfabc", UTF_8), b"abc");
        assert_eq!(strip_bom(b"abc", UTF_8), b"abc");
        // 属于另一种编码的 BOM 是数据，不是标记。
        assert_eq!(
            strip_bom(b"\xef\xbb\xbfabc", WINDOWS_1252),
            b"\xef\xbb\xbfabc"
        );
    }

    #[test]
    fn parses_p4_set_values_with_source_annotations() {
        let text = "P4CHARSET=utf8 (set)\n\
                    P4IGNORE=.p4ignore (config 'C:/ws/.p4config')\n\
                    P4IGNORE_LIST=a.ignore;b.ignore (set)\n\
                    P4EMPTY=\n";

        assert_eq!(parse_p4_set_value(text, "P4P4CHARSET"), None);
        assert_eq!(
            parse_p4_set_value(text, "P4CHARSET"),
            Some("utf8".to_owned())
        );
        assert_eq!(
            parse_p4_set_value(text, "P4IGNORE"),
            Some(P4IGNORE_FILE_NAME.to_owned())
        );
        // 多文件配置不是标准配置，拼出来的值不能被当成 .p4ignore
        assert_eq!(
            parse_p4_set_value(text, "P4IGNORE_LIST"),
            Some("a.ignore;b.ignore".to_owned())
        );
        assert_eq!(parse_p4_set_value(text, "P4EMPTY"), None);
    }
}
