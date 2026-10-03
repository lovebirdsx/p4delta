//! 领域数据模型：depot 记录、工作区文件与摘要缓存。
//!
//! 这些类型是各模块共用的「词汇表」，自身不依赖 crate 内的其他模块。

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Error, Result, anyhow, bail};
use bincode::{Decode, Encode};

/// Record from `p4 -G have` output
#[derive(Debug, Clone)]
pub(crate) struct HaveRecord {
    pub(crate) sync_time: Option<u64>, // Unix timestamp (optional)
}

/// Possible actions of file records in p4 fstat response.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum FileAction {
    Add,
    Edit,
    Delete,
    Branch,
    MoveAdd,
    MoveDelete,
    Integrate,
    Import,
    Purge,
    Archive,
}

// This helps us parse the actions from the p4 fstat response text.
impl std::str::FromStr for FileAction {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        use FileAction::*;
        match s {
            "add" => Ok(Add),
            "edit" => Ok(Edit),
            "delete" => Ok(Delete),
            "branch" => Ok(Branch),
            "move/add" => Ok(MoveAdd),
            "move/delete" => Ok(MoveDelete),
            "integrate" => Ok(Integrate),
            "import" => Ok(Import),
            "purge" => Ok(Purge),
            "archive" => Ok(Archive),
            _ => Err(anyhow!("Invalid file action type \"{}\"", s)),
        }
    }
}

/// Possible types of file records with regards to how we should compute the digest.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum DigestType {
    Binary,
    Text,
    Utf8,
    Symlink,
}

/// Possible types of file records from Perforce
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum FileType {
    Binary,
    Text,
    Unicode,
    Utf8,
    Utf16,
    Apple,
    Resource,
    Symlink,
}

// This determines the appropriate digest type to use for a Perforce file type.
impl FileType {
    pub(crate) fn digest_type(&self) -> Result<DigestType> {
        Ok(match self {
            FileType::Binary => DigestType::Binary,
            FileType::Text | FileType::Unicode => DigestType::Text,
            FileType::Utf8 => DigestType::Utf8,

            // These are mysteriously also using an utf8 digest.
            FileType::Utf16 => DigestType::Utf8,

            // A symlink revision holds the link target, not the contents of what it points at.
            FileType::Symlink => DigestType::Symlink,

            // These do not match the underlying MD5 sum when calculated on Windows, so unclear how
            // to calculate the digest.
            FileType::Apple | FileType::Resource => bail!("Apple legacy formats are not supported"),
        })
    }
}

// This helps us parse the digest type from the p4 fstat response text.
impl std::str::FromStr for FileType {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        use FileType::*;
        if s.starts_with("binary") {
            Ok(Binary)
        } else if s.starts_with("text") {
            Ok(Text)
        } else if s.starts_with("utf8") {
            Ok(Utf8)
        } else if s.starts_with("symlink") {
            Ok(Symlink)
        } else if s.starts_with("utf16") {
            Ok(Utf16)
        } else if s.starts_with("apple") {
            Ok(Apple)
        } else if s.starts_with("resource") {
            Ok(Resource)
        } else if s.starts_with("unicode") {
            Ok(Unicode)
        } else {
            Err(anyhow!("Invalid file digest type \"{s}\""))
        }
    }
}

/// Information about a single file in the depot, returned by p4 fstat queries.
/// Many of the fields are optional and only appear in specific situations.
#[derive(Default, Debug)]
pub(crate) struct DepotFileRecord {
    /// Path in depot syntax, such as "//Depot/Stream/File.ext".
    pub(crate) depot_file: String,

    /// Path in depot syntax, such as "//depot/stream/file.ext".
    pub(crate) depot_file_lower: String,

    /// Path in workspace syntax, such as "C:\Workspace\File.ext" on windows.
    pub(crate) client_file: String,

    /// Path in workspace syntax, such as "c:\workspace\file.ext" on windows.
    pub(crate) client_file_lower: String,

    /// If the file is in the depot, holds the current file type, such as text+w or binary+l.
    pub(crate) head_type: Option<FileType>,

    /// 原始的 `headType` 字符串，只在 [`Self::head_type`] 解析不出来时非空。
    /// 认不出的类型算不了摘要，但不该让整轮 reconcile 失败——分析阶段会把这类
    /// 记录挑出来转交 `p4 reconcile`。
    pub(crate) unsupported_type: Option<String>,

    /// If the file is in the depot, holds the type of the last change made in the depot.
    /// This tells us if the file existed once but was deleted from the depot.
    pub(crate) head_action: Option<FileAction>,

    /// If the file is in the depot, holds the most recent revision number on the server.
    pub(crate) head_rev: Option<u32>,

    /// If the file is in the workspace, holds the latest revision that we synced.
    /// This may be different from head_rev if we are behind, then the digest does not apply.
    pub(crate) have_rev: Option<u32>,

    /// If the file is in a pending changelist, holds what we are doing with it.
    pub(crate) action: Option<FileAction>,

    /// If the file is in the depot, holds the expected size on disk.
    pub(crate) file_size: Option<u64>,

    /// If the file is in the depot, holds the expected normalized MD5 digest.
    pub(crate) digest: Option<[u8; 16]>,
}

/// Information about the entire depot, returned by fstat queries.
#[derive(Default, Debug)]
pub(crate) struct DepotState {
    /// All file records in the depot.
    pub(crate) file_records: Vec<DepotFileRecord>,

    /// Map used to index file_records by depot_file_lower.
    depot_map: HashMap<String, usize>,

    /// Map used to index file_records by client_file_lower.
    client_map: HashMap<String, usize>,
}

impl DepotState {
    pub(crate) fn build_mapping(&mut self) {
        self.depot_map.reserve(self.file_records.len());
        self.client_map.reserve(self.file_records.len());

        for (i, record) in self.file_records.iter().enumerate() {
            self.depot_map.insert(record.depot_file_lower.clone(), i);

            // Handle case-sensitivity collisions on Windows (e.g., Snow_Normal.uasset vs Snow_normal.uasset)
            // If there's already an entry, prefer the non-deleted one with haveRev
            if let Some(&existing_idx) = self.client_map.get(&record.client_file_lower) {
                let existing = &self.file_records[existing_idx];
                let existing_is_deleted = matches!(
                    existing.head_action,
                    Some(FileAction::Delete | FileAction::MoveDelete)
                );
                let current_is_deleted = matches!(
                    record.head_action,
                    Some(FileAction::Delete | FileAction::MoveDelete)
                );

                // Prefer non-deleted over deleted, or the one with haveRev
                let should_replace = (!current_is_deleted && existing_is_deleted)
                    || (current_is_deleted == existing_is_deleted
                        && record.have_rev.is_some()
                        && existing.have_rev.is_none());

                if should_replace {
                    self.client_map.insert(record.client_file_lower.clone(), i);
                }
            } else {
                self.client_map.insert(record.client_file_lower.clone(), i);
            }
        }
    }

    pub(crate) fn get_depot_record_mut(&mut self, file: &str) -> Option<&mut DepotFileRecord> {
        self.depot_map.get(file).map(|&i| &mut self.file_records[i])
    }

    pub(crate) fn get_client_record(&self, file: &str) -> Option<&DepotFileRecord> {
        self.client_map.get(file).map(|&i| &self.file_records[i])
    }
}

/// 同步目标里一条记录的状态：该路径在目标版本（head 或某个 changelist）上是什么。
///
/// 与 [`DepotFileRecord`] 分开是刻意的。那份记录里的 `digest`、`head_action`、`file_size`
/// 描述的都是 **have 版本**——fstat 的补查会把它们换成 have 的（见 `p4/fstat.rs` 的回填），
/// 而这里回答的是另一个问题：「要拉到哪」。两个概念混用一个结构，迟早会有人读错字段。
///
/// 目标时刻**不在库**的路径不进这张表：调用方查不到即视为不在库。两种来源都归此列——
/// 目标时刻已是删除版本，或（只有 changelist 目标才可能）目标时刻它还没进 depot。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TargetRecord {
    /// 目标时刻的修订号。
    pub(crate) rev: u32,

    /// 目标时刻的动作。删除类动作表示该路径在目标时刻不在库。
    pub(crate) action: FileAction,
}

/// 目标版本的全部记录，键是 `depot_file_lower`（与 [`DepotState`] 的 depot 索引同口径）。
pub(crate) type TargetMap = HashMap<String, TargetRecord>;

/// Information about a single file in the workspace.
#[derive(Debug)]
pub(crate) struct WorkspaceFile {
    /// Path in workspace syntax, such as C:\Workspace\File.ext on windows.
    pub(crate) path: String,

    /// Path in workspace syntax, such as c:\workspace\file.ext on windows.
    pub(crate) path_lower: String,

    /// The size of the file on disk.
    pub(crate) size: u64,

    /// The modified time of the file on disk.
    pub(crate) date: SystemTime,

    /// Whether this file has been eliminated by one of the ignore filters.
    pub(crate) filtered: bool,
}

// Needed because SystemTime sucks
impl Default for WorkspaceFile {
    fn default() -> Self {
        WorkspaceFile {
            path: String::new(),
            path_lower: String::new(),
            size: 0,
            date: UNIX_EPOCH,
            filtered: false,
        }
    }
}

/// Information about the entire workspace.
#[derive(Default, Debug)]
pub(crate) struct WorkspaceState {
    /// All files in the workspace.
    pub(crate) files: Vec<WorkspaceFile>,

    /// Map used to index files by path_lower.
    pub(crate) file_map: HashMap<String, usize>,

    // The number of files not filtered.
    pub(crate) num_files: usize,
}

/// Used to store cached digest for a file.
#[derive(Debug, Encode, Decode)]
pub(crate) struct WorkspaceCacheEntry {
    /// File size during last run.
    pub(crate) size: u64,

    /// Modified date during last run.
    pub(crate) date: SystemTime,

    /// Digest during last run.
    pub(crate) digest: [u8; 16],
}

// Needed because SystemTime sucks
impl Default for WorkspaceCacheEntry {
    fn default() -> Self {
        WorkspaceCacheEntry {
            size: 0,
            date: UNIX_EPOCH,
            digest: [0; 16],
        }
    }
}

/// Used to store cached digests for a workspace.
#[derive(Default, Debug, Encode, Decode)]
pub(crate) struct WorkspaceCache {
    /// Map used to index files by path_lower.
    pub(crate) file_map: HashMap<String, WorkspaceCacheEntry>,

    /// Whether the cache is out of date.
    pub(crate) out_of_date: bool,
}

impl WorkspaceState {
    pub(crate) fn build_mapping(&mut self) {
        self.file_map.reserve(self.files.len());

        for (i, file) in self.files.iter().enumerate() {
            self.file_map.insert(file.path_lower.clone(), i);
        }
    }

    pub(crate) fn has_file(&self, file: &str) -> bool {
        self.file_map.contains_key(file)
    }

    /// 文件存在**且未被忽略**。名字里的 unfiltered 是关键词：被忽略的文件和不存在
    /// 的文件一样返回 false，所以否定形式读作「不存在，或已被忽略」。
    pub(crate) fn has_unfiltered_file(&self, file: &str) -> bool {
        match self.file_map.get(file) {
            Some(i) => !self.files[*i].filtered,
            None => false,
        }
    }

    /// 只被测试使用：生产路径一律走 [`Self::get_filtered`] 或 [`Self::has_filtered`]。
    #[cfg(test)]
    pub(crate) fn get_file(&self, file: &str) -> Option<&WorkspaceFile> {
        self.file_map.get(file).map(|&i| &self.files[i])
    }

    pub(crate) fn get_filtered(&self, file: &str) -> Option<&WorkspaceFile> {
        match self.file_map.get(file) {
            Some(i) => {
                let file = &self.files[*i];
                if file.filtered { None } else { Some(file) }
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::path::local_path_key;

    // ---- 枚举解析 ----

    #[test]
    fn file_actions_parse_from_fstat_text() {
        use FileAction::*;

        let cases: [(&str, FileAction); 10] = [
            ("add", Add),
            ("edit", Edit),
            ("delete", Delete),
            ("branch", Branch),
            ("move/add", MoveAdd),
            ("move/delete", MoveDelete),
            ("integrate", Integrate),
            ("import", Import),
            ("purge", Purge),
            ("archive", Archive),
        ];
        for (text, expected) in cases {
            assert_eq!(text.parse::<FileAction>().unwrap(), expected, "{text}");
        }
    }

    #[test]
    fn invalid_file_actions_are_rejected() {
        // p4 的输出是固定小写，大小写变体属于「没见过的形态」，必须响亮失败。
        for text in ["Add", "move-add", "", "delete ", "movedelete"] {
            let error = text.parse::<FileAction>().unwrap_err().to_string();
            assert!(
                error.contains("Invalid file action type"),
                "{text}: {error}"
            );
        }
    }

    #[test]
    fn file_types_parse_their_p4_modifiers() {
        use FileType::*;

        let cases: [(&str, FileType); 13] = [
            ("text", Text),
            ("text+w", Text),
            ("text+k", Text),
            ("text+c", Text),
            ("binary", Binary),
            ("binary+l", Binary),
            ("utf8", Utf8),
            ("utf8-bom", Utf8),
            ("utf16", Utf16),
            ("unicode", Unicode),
            ("symlink", Symlink),
            ("apple", Apple),
            ("resource", Resource),
        ];
        for (text, expected) in cases {
            assert_eq!(text.parse::<FileType>().unwrap(), expected, "{text}");
        }
    }

    /// 认不出的类型名在这里返回 Err，但 `FstatParser` 会把它降级成「转交 p4 reconcile」，
    /// 而不是让整轮 fstat 失败——所以这个 Err 不再是致命的（见 `p4/fstat.rs`）。
    #[test]
    fn unknown_file_types_are_rejected() {
        for text in ["", "Text", "Text +w", "not-a-type"] {
            assert!(text.parse::<FileType>().is_err(), "{text}");
        }
    }

    #[test]
    fn digest_types_follow_the_p4_type() {
        let cases = [
            (FileType::Binary, DigestType::Binary),
            (FileType::Text, DigestType::Text),
            (FileType::Unicode, DigestType::Text),
            (FileType::Utf8, DigestType::Utf8),
            // utf16 也用 utf8 摘要，这不是笔误。
            (FileType::Utf16, DigestType::Utf8),
            // 符号链接的修订存的是链接目标，与所指文件的内容无关。
            (FileType::Symlink, DigestType::Symlink),
        ];
        for (file_type, expected) in cases {
            assert_eq!(file_type.digest_type().unwrap(), expected, "{file_type:?}");
        }
    }

    #[test]
    fn apple_legacy_types_have_no_digest() {
        for file_type in [FileType::Apple, FileType::Resource] {
            let error = file_type.digest_type().unwrap_err().to_string();
            assert!(error.contains("Apple legacy formats"), "{error}");
        }
    }

    // ---- DepotState::build_mapping 的大小写冲突决胜 ----

    /// 两条只在大小写上不同的客户端路径；`local_path_key` 把它们折成同一个键，
    /// 于是后插入的记录必须与已存在的那条比个高下。
    const UPPER_PATH: &str = r"C:\WS\Snow_Normal.uasset";
    const LOWER_PATH: &str = r"C:\WS\Snow_normal.uasset";

    /// `depot_file` 在这里当标签用，决胜结果靠它区分是哪一条胜出。
    fn tagged(
        client_file: &str,
        tag: &str,
        head_action: Option<FileAction>,
        have_rev: Option<u32>,
    ) -> DepotFileRecord {
        DepotFileRecord {
            depot_file: tag.to_owned(),
            depot_file_lower: tag.to_ascii_lowercase(),
            client_file: client_file.to_owned(),
            client_file_lower: local_path_key(client_file),
            head_action,
            have_rev,
            ..Default::default()
        }
    }

    fn upper(head_action: Option<FileAction>, have_rev: Option<u32>) -> DepotFileRecord {
        tagged(UPPER_PATH, "upper", head_action, have_rev)
    }

    fn lower(head_action: Option<FileAction>, have_rev: Option<u32>) -> DepotFileRecord {
        tagged(LOWER_PATH, "lower", head_action, have_rev)
    }

    /// 按给定顺序建索引，返回该键最终指向的记录标签。键在冲突后必须仍然存在。
    fn winner(records: Vec<DepotFileRecord>) -> String {
        let key = records[0].client_file_lower.clone();
        let mut state = DepotState {
            file_records: records,
            ..Default::default()
        };
        state.build_mapping();
        state
            .get_client_record(&key)
            .expect("the shared key must survive a collision")
            .depot_file
            .clone()
    }

    /// 在库记录胜过删除记录，与插入顺序无关——haveRev 也救不回一条删除记录。
    #[test]
    fn case_collisions_prefer_the_synced_non_deleted_record() {
        use FileAction::*;

        // 有 haveRev 的删除记录仍然输给没有 haveRev 的在库记录。
        assert_eq!(
            winner(vec![upper(Some(Edit), None), lower(Some(Delete), Some(7))]),
            "upper"
        );
        assert_eq!(
            winner(vec![lower(Some(Delete), Some(7)), upper(Some(Edit), None)]),
            "upper"
        );

        // 两边都没有 haveRev 时同理。
        assert_eq!(
            winner(vec![upper(Some(Edit), None), lower(Some(Delete), None)]),
            "upper"
        );
        assert_eq!(
            winner(vec![lower(Some(Delete), None), upper(Some(Edit), None)]),
            "upper"
        );

        // 反过来：删除记录无论先后都不能顶掉在库记录。
        assert_eq!(
            winner(vec![upper(Some(Delete), Some(7)), lower(Some(Edit), None)]),
            "lower"
        );
        assert_eq!(
            winner(vec![lower(Some(Edit), None), upper(Some(Delete), Some(7))]),
            "lower"
        );
    }

    /// 同为在库记录时，有 haveRev 的胜出；两边都有则先到者胜。
    #[test]
    fn case_collisions_prefer_the_record_with_a_have_revision() {
        use FileAction::*;

        assert_eq!(
            winner(vec![upper(Some(Edit), None), lower(Some(Edit), Some(7))]),
            "lower"
        );
        assert_eq!(
            winner(vec![lower(Some(Edit), Some(7)), upper(Some(Edit), None)]),
            "lower"
        );

        assert_eq!(
            winner(vec![upper(Some(Edit), Some(7)), lower(Some(Edit), None)]),
            "upper"
        );
        assert_eq!(
            winner(vec![lower(Some(Edit), None), upper(Some(Edit), Some(7))]),
            "upper"
        );

        // 都有 haveRev：没有可比较的判据，保留先插入的那条。
        assert_eq!(
            winner(vec![upper(Some(Edit), Some(3)), lower(Some(Edit), Some(9))]),
            "upper"
        );
        assert_eq!(
            winner(vec![lower(Some(Edit), Some(9)), upper(Some(Edit), Some(3))]),
            "lower"
        );
    }

    /// 同为删除记录时仍要保留一条。键不能因为两条都是删除就消失，
    /// 否则工作区里那个同名文件会被当成「depot 里没有」而误报新增。
    #[test]
    fn deleted_collisions_still_keep_one_record() {
        use FileAction::*;

        assert_eq!(
            winner(vec![
                upper(Some(Delete), None),
                lower(Some(Delete), Some(7))
            ]),
            "lower"
        );
        assert_eq!(
            winner(vec![
                lower(Some(Delete), Some(7)),
                upper(Some(Delete), None)
            ]),
            "lower"
        );

        assert_eq!(
            winner(vec![upper(Some(Delete), None), lower(Some(Delete), None)]),
            "upper"
        );
        assert_eq!(
            winner(vec![lower(Some(Delete), None), upper(Some(Delete), None)]),
            "lower"
        );
    }

    /// `move/delete` 与 `delete` 一样算删除；`move/add` 与 `add` 一样不算。
    #[test]
    fn move_actions_are_classified_like_their_plain_counterparts() {
        use FileAction::*;

        assert_eq!(
            winner(vec![
                upper(Some(MoveDelete), Some(7)),
                lower(Some(Edit), None)
            ]),
            "lower"
        );
        assert_eq!(
            winner(vec![
                lower(Some(Edit), None),
                upper(Some(MoveDelete), Some(7))
            ]),
            "lower"
        );

        assert_eq!(
            winner(vec![
                upper(Some(MoveAdd), None),
                lower(Some(Delete), Some(7))
            ]),
            "upper"
        );
        assert_eq!(
            winner(vec![
                lower(Some(Delete), Some(7)),
                upper(Some(MoveAdd), None)
            ]),
            "upper"
        );
    }

    // ---- 两套索引的查询语义 ----

    #[test]
    fn depot_records_are_indexed_by_the_folded_depot_path() {
        let mut state = DepotState {
            file_records: vec![DepotFileRecord {
                depot_file: "//Depot/Stream/File.ext".to_owned(),
                depot_file_lower: "//depot/stream/file.ext".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        };
        state.build_mapping();

        assert!(
            state
                .get_depot_record_mut("//depot/stream/file.ext")
                .is_some()
        );
        assert!(state.get_depot_record_mut("//depot/other.ext").is_none());
        // 查询本身不折叠：调用方必须自己传 depot_file_lower 形式的键（见 p4/fstat.rs 的补查回填）。
        assert!(
            state
                .get_depot_record_mut("//DEPOT/STREAM/FILE.EXT")
                .is_none()
        );
    }

    /// fstat 补查把 have 版本的字段回填到原记录上，改动必须能被后续查询看到。
    #[test]
    fn mutating_a_record_through_the_depot_map_is_visible() {
        let mut state = DepotState {
            file_records: vec![DepotFileRecord {
                depot_file_lower: "//depot/a.txt".to_owned(),
                client_file: r"C:\ws\a.txt".to_owned(),
                client_file_lower: local_path_key(r"C:\ws\a.txt"),
                ..Default::default()
            }],
            ..Default::default()
        };
        state.build_mapping();

        let record = state
            .get_depot_record_mut("//depot/a.txt")
            .expect("indexed");
        record.head_type = Some(FileType::Text);
        record.file_size = Some(42);

        let record = state
            .get_client_record(&local_path_key(r"C:\ws\a.txt"))
            .expect("indexed by the client path too");
        assert_eq!(record.head_type, Some(FileType::Text));
        assert_eq!(record.file_size, Some(42));
    }

    fn workspace_with(entries: &[(&str, bool)]) -> WorkspaceState {
        let mut state = WorkspaceState {
            files: entries
                .iter()
                .map(|(path, filtered)| WorkspaceFile {
                    path: (*path).to_owned(),
                    path_lower: local_path_key(path),
                    size: 1,
                    date: UNIX_EPOCH,
                    filtered: *filtered,
                })
                .collect(),
            ..Default::default()
        };
        state.build_mapping();
        state
    }

    /// `has_unfiltered_file` 只在「文件存在**且未被忽略**」时为真。被忽略与不存在
    /// 一样返回 false，所以调用点写成 `!has_unfiltered_file(..)` 表达「不存在或已忽略」。
    #[test]
    fn has_unfiltered_file_is_false_for_missing_or_filtered_files() {
        let state = workspace_with(&[(r"C:\ws\kept.txt", false), (r"C:\ws\ignored.log", true)]);

        assert!(state.has_unfiltered_file(&local_path_key(r"C:\ws\kept.txt")));
        assert!(!state.has_unfiltered_file(&local_path_key(r"C:\ws\ignored.log")));
        assert!(!state.has_unfiltered_file(&local_path_key(r"C:\ws\absent.txt")));

        // has_file 只看索引，不看 filtered 标记。
        assert!(state.has_file(&local_path_key(r"C:\ws\ignored.log")));
        assert!(!state.has_file(&local_path_key(r"C:\ws\absent.txt")));
    }

    #[test]
    fn get_filtered_hides_filtered_and_missing_files() {
        let state = workspace_with(&[(r"C:\ws\kept.txt", false), (r"C:\ws\ignored.log", true)]);

        assert!(
            state
                .get_filtered(&local_path_key(r"C:\ws\kept.txt"))
                .is_some()
        );
        assert!(
            state
                .get_filtered(&local_path_key(r"C:\ws\ignored.log"))
                .is_none()
        );
        assert!(
            state
                .get_filtered(&local_path_key(r"C:\ws\absent.txt"))
                .is_none()
        );

        // 被忽略的文件仍然在索引里，只是 get_filtered 不返回它。
        let hidden = state
            .get_file(&local_path_key(r"C:\ws\ignored.log"))
            .expect("still indexed");
        assert!(hidden.filtered);
    }
}
