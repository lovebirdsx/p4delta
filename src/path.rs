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

/// map 比较键：统一分隔符后 ASCII 小写，与 p4 返回的 clientFile 对齐。
pub(crate) fn local_path_key(path: &str) -> String {
    normalize_local_path(path).to_ascii_lowercase()
}

/// 入口路径处理：相对路径转绝对并清掉 `.`/`..`，再统一分隔符。
/// p4 返回的 clientFile 总是绝对路径，扫描结果必须与之一致，否则本地键会整体失配。
/// 解析失败时退回原路径，不让路径形式导致整个运行失败。
pub(crate) fn absolute_local_path(path: &str) -> String {
    std::path::absolute(path)
        .map(|absolute| normalize_local_path_owned(absolute.display().to_string()))
        .unwrap_or_else(|_| normalize_local_path_owned(path.to_owned()))
}

/// 剥掉末尾的 p4 通配后缀（`/...` 或 `\...`）：P4V 等工具传进来的目录参数常常带着它。
///
/// 注意 `trim_end_matches` 剥的是**所有**连续匹配的后缀，所以 `C:\ws\...\...` 会被
/// 整个剥成 `C:\ws`。实践中不会出现双后缀，这里把「剥干净」当成显式契约写下来，
/// 而不是留给读者去推敲它是副作用还是本意。
pub(crate) fn strip_depot_wildcard_suffix(path: &str) -> &str {
    path.trim_end_matches(if cfg!(windows) { "\\..." } else { "/..." })
}

/// 按路径组件判断 `path` 是否在 `dir` 之下：`node_modules2` 不算 `node_modules` 的子路径。
pub(crate) fn path_is_under_key(path_key: &str, dir_key: &str) -> bool {
    path_key.len() > dir_key.len()
        && path_key.starts_with(dir_key)
        && path_key.as_bytes()[dir_key.len()] == std::path::MAIN_SEPARATOR as u8
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
            assert_eq!(local_path_key("/Ws/File.txt"), "/ws/file.txt");
            // Unix 上反斜杠是普通文件名字符，不能被当成分隔符
            assert_eq!(local_path_key("/ws/back\\slash.txt"), "/ws/back\\slash.txt");
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
}
