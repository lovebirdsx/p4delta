//! 本地路径的规范化与匹配键。
//!
//! P4V 等工具传进来的目录参数可能是正斜杠、反斜杠或混合分隔符，
//! 必须统一成与 p4 返回的 clientFile 一致的本地路径键才能正确匹配。

use std::borrow::Cow;

/// 本地路径分隔符的统一形式：Windows 上把 `/` 换成 `\`，其他平台原样返回。
/// 只用于本地路径（工作区文件与 p4 返回的 clientFile），不能用于 depot 路径。
pub(crate) fn normalize_local_path(path: &str) -> Cow<'_, str> {
    if cfg!(windows) && path.contains('/') {
        Cow::Owned(path.replace('/', "\\"))
    } else {
        Cow::Borrowed(path)
    }
}

/// 同上，但接收所有权，避免多余的分配。
pub(crate) fn normalize_local_path_owned(path: String) -> String {
    if cfg!(windows) && path.contains('/') {
        path.replace('/', "\\")
    } else {
        path
    }
}

/// map 比较键：统一分隔符后按平台的路径身份策略折小写，与 p4 返回的 clientFile 对齐。
///
/// 策略与编辑器一致（VSCode 那一侧的路径身份也是这个口径）：**Windows 与 macOS 的文件系统
/// 不区分大小写**，折掉它 `C:\ws\A.txt` 与 `c:\ws\a.txt` 才是同一个键；**其余平台区分**，
/// 折了会把 `A.txt` 与 `a.txt` 两个真文件塌成一个，扫盘与 depot 记录就会互相抵消成
/// 「本地删除 + 待新增」。折的是 Unicode 小写（不是 ASCII、也不是 locale 相关的那套），
/// 与编辑器的 `toLowerCase()` 同一口径。
///
/// 折叠会改变字节长度（`İ` 一个字符折成两个），所以这个键只能用来**比较**，不要拿它的
/// 下标去切原路径——按输入原文切片的活由 `json.rs` 的 `comparison_key` 那一路负责。
pub(crate) fn local_path_key(path: &str) -> String {
    let normalized = normalize_local_path(path);
    if path_identity_ignores_case() {
        normalized.to_lowercase()
    } else {
        normalized.into_owned()
    }
}

/// 平台的文件系统是否不区分大小写，见 [`local_path_key`]。
pub(crate) fn path_identity_ignores_case() -> bool {
    cfg!(any(windows, target_os = "macos"))
}

/// 盘符统一成大写：同一台机器上 `c:\ws` 与 `C:\ws` 是同一个目录，路径键与快照里都不该
/// 出现两种拼法（p4 回 clientFile 时也可能换一种）。
pub(crate) fn uppercase_drive_letter(path: &mut str) {
    if let Some(first_letter) = path.get_mut(0..1) {
        first_letter.make_ascii_uppercase();
    }
}

/// 入口路径处理：相对路径转绝对并清掉 `.`/`..`，再统一分隔符。
/// p4 返回的 clientFile 总是绝对路径，扫描结果必须与之一致，否则本地键会整体失配。
/// 解析失败时退回原路径，不让路径形式导致整个运行失败。
pub(crate) fn absolute_local_path(path: &str) -> String {
    std::path::absolute(path)
        .map(|absolute| normalize_local_path_owned(absolute.display().to_string()))
        .unwrap_or_else(|_| normalize_local_path_owned(path.to_owned()))
}

/// 范围里路径的规范拼法：绝对化、统一分隔符、盘符大写。
///
/// 范围配置（`.p4delta-scope`）的相对路径、`--exclude-*` 与 client root 都走这一套，
/// 免得同一个目录因为拼法不同而在两个地方各算一次（盘符大小写、正反斜杠是最常见的两种）。
pub(crate) fn canonical_local_path(path: &str) -> String {
    let mut absolute = absolute_local_path(path);
    uppercase_drive_letter(&mut absolute);
    absolute
}

/// 目录路径的比较键：在 [`local_path_key`] 之上再折掉末尾分隔符。
///
/// `X:\ws\` 与 `x:\ws` 是同一个目录，逐字符比会把同一份上下文判成不一致。根（`C:\`、
/// `/`）整条就是分隔符，折掉它会让所有根塌成同一个键，所以那种形状原样保留。
pub(crate) fn directory_key(path: &str) -> String {
    let key = local_path_key(path);
    let trimmed = key.trim_end_matches(std::path::MAIN_SEPARATOR);

    if trimmed.is_empty() || trimmed.ends_with(':') {
        return key;
    }

    trimmed.to_owned()
}

/// 剥掉末尾的 p4 通配后缀（`/...` 或 `\...`）：P4V 等工具传进来的目录参数常常带着它。
///
/// 注意 `trim_end_matches` 剥的是**所有**连续匹配的后缀，所以 `C:\ws\...\...` 会被
/// 整个剥成 `C:\ws`。实践中不会出现双后缀，这里把「剥干净」当成显式契约写下来，
/// 而不是留给读者去推敲它是副作用还是本意。
pub(crate) fn strip_depot_wildcard_suffix(path: &str) -> &str {
    path.trim_end_matches(if cfg!(windows) { "\\..." } else { "/..." })
}

/// 本地路径 → p4 的 **file spec** 里可以安全出现的文本：把在 p4 语法里有特殊含义的字符
/// 转义成 `%XX`（`p4 help wildcards`）。
///
/// 起因是**本地文件名**而不是 depot 名字：`#` 引入修订号、`@` 引入 changelist/label、
/// `%` 是转义引导符、`*`/`?` 是通配符。一个叫 `notes#1.txt` 的本地文件原样交给 p4，会被读成
/// 「`notes` 的第 1 版」——而且报出来的是「找不到文件」，看上去像路径写错了。Windows 上
/// `#`、`@`、`%` 都是合法文件名字符，这不是病态输入。
///
/// 两个边界共用这一份实现：**范围入口**（`p4 fstat` / `p4 have` 的范围查询）与**动作**
/// （`p4 edit` / `add` / `delete` / `revert`）以及新增文件的映射查询（`p4 where`）。少了
/// 动作那一侧，「查得到、改不了」——分析报告一条 edit，下发时 p4 拿 `#` 当修订号拒收。
///
/// 转义的是**本地原始路径**，只在交给 p4 之前跑一次：所以入口里存的是用户写下的原文，
/// `%23` 这种形态不会被当成已转义的名字再转一遍（那会变成文件 `%23`，同样不是用户要的东西）。
///
/// 大小写十六进制都收，这里统一写大写，与 p4 自己的回显一致。
pub(crate) fn escape_file_spec(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());

    for character in path.chars() {
        match character {
            '%' => escaped.push_str("%25"),
            '#' => escaped.push_str("%23"),
            '@' => escaped.push_str("%40"),
            '*' => escaped.push_str("%2A"),
            '?' => escaped.push_str("%3F"),
            other => escaped.push(other),
        }
    }

    escaped
}

/// 按路径组件判断 `path` 是否在 `dir` 之下：`node_modules2` 不算 `node_modules` 的子路径。
///
/// 根目录（`C:\`、`/`、`//server/share/`）自己就以分隔符结尾，不能再要求下一位是分隔符——
/// 少了这一条，`C:\` 整个子树都会被判成「不在范围内」，而工作区边界完全可能就是一个根。
pub(crate) fn path_is_under_key(path_key: &str, dir_key: &str) -> bool {
    path_key.len() > dir_key.len()
        && path_key.starts_with(dir_key)
        && (dir_key.ends_with(std::path::MAIN_SEPARATOR)
            || path_key.as_bytes()[dir_key.len()] == std::path::MAIN_SEPARATOR as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;

    use std::env;

    use encoding_rs::UTF_8;

    use crate::p4::fstat::parse_p4_fstat_lines;
    use crate::p4::marshal::parse_p4_have_output;

    // ---- 本地路径键：工作区扫描、fstat、have 与 ignores 必须用同一套键 ----

    #[test]
    fn local_path_keys_unify_separators_and_case() {
        if cfg!(windows) {
            assert_eq!(local_path_key("E:/ws/File.txt"), "e:\\ws\\file.txt");
            assert_eq!(local_path_key("E:\\ws/File.txt"), "e:\\ws\\file.txt");
            assert_eq!(local_path_key("E:\\ws\\File.txt"), "e:\\ws\\file.txt");
            // 盘符根、UNC 与中文路径都要保留
            assert_eq!(local_path_key("C:/"), "c:\\");
            assert_eq!(local_path_key("C:/中文/说明.txt"), "c:\\中文\\说明.txt");
            assert_eq!(
                local_path_key("\\\\server\\share/File.txt"),
                "\\\\server\\share\\file.txt"
            );
        } else {
            // Unix 上不折大小写（见 `case_folding_follows_the_platform_identity`），
            // 也不把反斜杠当分隔符——它是合法的文件名字符。
            assert_eq!(local_path_key("/ws/File.txt"), "/ws/File.txt");
            assert_eq!(local_path_key("/ws/back\\slash.txt"), "/ws/back\\slash.txt");
        }
    }

    /// 大小写策略跟着**平台的文件系统**走：Windows 与 macOS 折掉，其余平台原样保留。
    /// 折了大小写的地方还必须是 Unicode 小写（`İ` → `i̇`）——编辑器那一侧用的是
    /// `toLowerCase()`，两边折的不是同一套字符就会在同一个目录上给出两个键。
    #[test]
    fn case_folding_follows_the_platform_identity() {
        if path_identity_ignores_case() {
            assert_eq!(local_path_key("C:/WS/İ.txt"), local_path_key("c:/ws/i̇.TXT"));
            assert_eq!(local_path_key("/Ws/A.TXT"), local_path_key("/ws/a.txt"));
        } else {
            assert_ne!(local_path_key("/ws/A.txt"), local_path_key("/ws/a.txt"));
            assert_eq!(local_path_key("/WS/x"), "/WS/x");
        }
    }

    #[test]
    fn already_normalized_paths_are_borrowed() {
        let path = if cfg!(windows) {
            "E:\\ws\\file.txt"
        } else {
            "/ws/file.txt"
        };
        assert!(matches!(normalize_local_path(path), Cow::Borrowed(_)));
    }

    #[test]
    fn entry_paths_are_made_absolute_without_dot_components() {
        let root = env::current_dir().unwrap();
        let expected = local_path_key(&root.join("src").display().to_string());

        assert_eq!(local_path_key(&absolute_local_path("./src")), expected);
        assert_eq!(local_path_key(&absolute_local_path("src/.")), expected);
    }

    #[test]
    fn fstat_client_paths_use_the_same_keys_as_the_workspace() {
        // p4 返回的 clientFile 与本地扫描结果未必逐字相同（Windows 上 p4 回正斜杠、
        // 扫描回反斜杠），但两者的键必须一致，否则整份工作区都会被误报。
        let (p4_form, scanned) = if cfg!(windows) {
            ("E:/ws/Sub/A.txt", "E:\\ws\\Sub\\A.txt")
        } else {
            // Unix 上反斜杠是普通文件名字符，没有分隔符要归一，这里覆盖大小写归一。
            ("/ws/Sub/A.txt", "/ws/Sub/A.txt")
        };
        let client_file_line = format!("... clientFile {p4_form}");

        let lines: Vec<&[u8]> = vec![
            b"... depotFile //depot/A.txt",
            client_file_line.as_bytes(),
            b"... headRev 1",
        ];

        let records = parse_p4_fstat_lines(lines, UTF_8).unwrap();

        let workspace_key = local_path_key(scanned);
        assert_eq!(records[0].client_file_lower, workspace_key);
        assert_eq!(records[0].client_file, *normalize_local_path(p4_form));
    }

    #[test]
    fn have_records_use_the_same_keys_as_the_workspace() {
        // 同 fstat：p4 返回的路径形式与本地扫描形式必须落到同一个键。
        let (p4_form, scanned) = if cfg!(windows) {
            ("E:/ws/Sub/A.txt", "E:\\ws\\Sub\\A.txt")
        } else {
            ("/ws/Sub/A.txt", "/ws/Sub/A.txt")
        };
        let data = marshal_dict(&[
            ("code", "stat"),
            ("path", p4_form),
            ("syncTime", "1700000000"),
        ]);

        let records = parse_p4_have_output(&data, UTF_8).unwrap();

        assert!(records.contains_key(&local_path_key(scanned)));
    }

    #[test]
    fn component_boundaries_are_respected() {
        let sep = std::path::MAIN_SEPARATOR;
        let node_modules = format!("c:{sep}ws{sep}node_modules");

        assert!(path_is_under_key(
            &format!("{node_modules}{sep}sub{sep}f.txt"),
            &node_modules
        ));
        assert!(!path_is_under_key(
            &format!("c:{sep}ws{sep}node_modules2"),
            &node_modules
        ));
        assert!(!path_is_under_key(
            &format!("c:{sep}ws{sep}node_modules"),
            &node_modules
        ));
    }

    /// 根本身就以分隔符结尾，不能再要求下一位是分隔符：范围边界完全可能是一个根
    /// （client Root 就是盘根），那一下会把整棵子树判成越界。
    #[test]
    fn a_root_directory_still_contains_its_subtree() {
        let sep = std::path::MAIN_SEPARATOR;
        let root = format!("c:{sep}");

        assert!(path_is_under_key(&format!("c:{sep}ws{sep}a.txt"), &root));
        assert!(!path_is_under_key(&root, &root));
        assert!(!path_is_under_key("d:\\ws\\a.txt", &root));
    }

    /// 目录比较键在本地路径键之上折掉末尾分隔符：`X:\ws\` 与 `x:\ws` 是同一个目录，
    /// 逐字符比会把同一份上下文判成不一致。大小写那一半跟平台的路径身份策略走。
    #[test]
    fn directory_keys_fold_case_and_trailing_separators() {
        let sep = std::path::MAIN_SEPARATOR;
        let base = if cfg!(windows) { "C:\\ws" } else { "/ws" };
        let expected = directory_key(base);

        assert_eq!(directory_key(&format!("{base}{sep}")), expected);
        assert_eq!(directory_key(&format!("{base}{sep}{sep}")), expected);

        if path_identity_ignores_case() {
            assert_eq!(directory_key(&base.to_ascii_uppercase()), expected);
        } else {
            assert_ne!(directory_key(&base.to_ascii_uppercase()), expected);
        }

        if cfg!(windows) {
            // UNC 共享根也是「自己以分隔符结尾」的形状。
            assert_eq!(
                directory_key(r"\\server\share\"),
                directory_key(r"\\server\share")
            );
        }
    }

    /// 根整条就是分隔符：折掉它会让所有根塌成同一个键。
    #[test]
    fn a_root_directory_key_keeps_its_separator() {
        let sep = std::path::MAIN_SEPARATOR;
        let root = if cfg!(windows) { "C:\\" } else { "/" };

        assert_eq!(directory_key(root), local_path_key(root));
        assert!(directory_key(root).ends_with(sep));
    }

    #[test]
    fn depot_wildcard_suffixes_are_stripped() {
        let sep = std::path::MAIN_SEPARATOR;
        let suffix = if cfg!(windows) { r"\..." } else { "/..." };
        let path = format!("c:{sep}ws{sep}stream{suffix}");

        assert_eq!(
            strip_depot_wildcard_suffix(&path),
            format!("c:{sep}ws{sep}stream")
        );
    }

    /// `trim_end_matches` 剥的是所有连续匹配的后缀。实践中不会有双后缀，
    /// 但把「剥干净」写成显式契约，好过让读者以为这是没注意到的副作用。
    #[test]
    fn repeated_wildcard_suffixes_are_all_stripped() {
        let sep = std::path::MAIN_SEPARATOR;
        let suffix = if cfg!(windows) { r"\..." } else { "/..." };

        assert_eq!(
            strip_depot_wildcard_suffix(&format!("c:{sep}ws{suffix}{suffix}")),
            format!("c:{sep}ws")
        );
    }

    #[test]
    fn only_a_trailing_wildcard_is_stripped() {
        let sep = std::path::MAIN_SEPARATOR;
        let suffix = if cfg!(windows) { r"\..." } else { "/..." };

        // 通配符在中间时什么都不动，只有末尾的才剥。
        let path = format!("c:{sep}ws{suffix}{sep}sub");
        assert_eq!(strip_depot_wildcard_suffix(&path), path);

        // 没有后缀时原样借用，不做多余的分配。
        let plain = format!("c:{sep}ws{sep}stream");
        let stripped = strip_depot_wildcard_suffix(&plain);
        assert_eq!(stripped, plain);
        assert!(std::ptr::eq(stripped.as_ptr(), plain.as_ptr()));
    }

    /// file spec 转义：五个元字符各转一次，`%` 先换（否则后面生成的 `%40` 会被再编码一遍），
    /// 没有元字符的路径一个字节都不动。范围查询、动作调用与映射查询共用这一份。
    #[test]
    fn file_spec_metacharacters_are_escaped_once() {
        assert_eq!(
            escape_file_spec(r"C:\ws\notes#1@2%3*4?5.txt"),
            r"C:\ws\notes%231%402%253%2A4%3F5.txt"
        );
        assert_eq!(escape_file_spec(r"C:\ws\readme.txt"), r"C:\ws\readme.txt");
        assert_eq!(escape_file_spec("/ws/中文 说明.txt"), "/ws/中文 说明.txt");
    }
}
