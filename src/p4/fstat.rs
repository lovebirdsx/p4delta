//! `p4 fstat` 查询与流式解析。

use std::collections::HashSet;
use std::time::Instant;

use anyhow::{Error, Result, anyhow, bail};
use encoding_rs::Encoding;
use futures::StreamExt;
use futures::io::AsyncBufReadExt;
use hex::FromHex;

use crate::READ_BUFFER_SIZE;
use crate::charset::{decode_p4_bytes, p4_encoding, strip_bom, trim_line_ending};
use crate::cli::Options;
use crate::json::sayln;
use crate::model::{DepotFileRecord, DepotState, FileAction, TargetMap, TargetRecord};
use crate::p4::process::{
    MAX_PARALLEL_P4_COMMANDS, P4Pipes, build_p4_command, compute_batches, read_p4_stderr,
    take_p4_pipes, write_p4_arguments,
};
use crate::path::{local_path_key, normalize_local_path_owned};

/// `p4 fstat` 文本输出的流式解析器。
///
/// 消费未解码的原始行：只有我们要留下的那几个字段的值会被解码或分配。它取代的是把整个
/// 响应（大工作区上几千万行）缓冲成 `Vec<String>` 的旧做法。
pub(crate) struct FstatParser {
    encoding: &'static Encoding,
    records: Vec<DepotFileRecord>,
    pending: DepotFileRecord,
    /// `pending` 是否已经见过字段：这样末尾的空行不会提交出一条空记录。
    started: bool,
}

impl FstatParser {
    fn new(encoding: &'static Encoding, capacity: usize) -> Self {
        FstatParser {
            encoding,
            records: Vec::with_capacity(capacity),
            pending: Default::default(),
            started: false,
        }
    }

    fn push_line(&mut self, line: &[u8]) -> Result<()> {
        // 记录之间用空行分隔。旧解析器以 `len > 3` 为准，短行同样结束一条记录；
        // 这里保持一致，好让畸形输出解析出相同的结果。
        if line.len() <= 3 {
            self.commit();
            return Ok(());
        }

        // 行的形状是 "... key value"。键一律是 ASCII。
        let body = line.strip_prefix(b"... ").unwrap_or(line);
        let (key, value) = match body.iter().position(|byte| *byte == b' ') {
            Some(index) => (&body[..index], &body[index + 1..]),
            None => (body, &b""[..]),
        };

        // 只有路径字段可能含非 ASCII，所以只有它们需要按字符集解码。
        match key {
            b"depotFile" => {
                let value = decode_p4_bytes(value, self.encoding).0.into_owned();
                self.pending.depot_file_lower = value.to_ascii_lowercase();
                self.pending.depot_file = value;
            }
            b"clientFile" => {
                let value = decode_p4_bytes(value, self.encoding).0.into_owned();
                // clientFile 是本地路径：统一分隔符后再做键，与工作区扫描的结果对齐。
                self.pending.client_file_lower = local_path_key(&value);
                self.pending.client_file = normalize_local_path_owned(value);
            }
            b"headType" => {
                let value = std::str::from_utf8(value)?;
                // 认不出的类型名不能让整轮 fstat 失败，记下原串转交 p4 reconcile。
                match value.parse() {
                    Ok(file_type) => self.pending.head_type = Some(file_type),
                    Err(_) => self.pending.unsupported_type = Some(value.to_owned()),
                }
            }
            b"headRev" => self.pending.head_rev = Some(std::str::from_utf8(value)?.parse()?),
            b"haveRev" => self.pending.have_rev = Some(std::str::from_utf8(value)?.parse()?),
            b"headAction" => self.pending.head_action = Some(std::str::from_utf8(value)?.parse()?),
            b"action" => self.pending.action = Some(std::str::from_utf8(value)?.parse()?),
            b"digest" => {
                self.pending.digest = Some(<[u8; 16]>::from_hex(std::str::from_utf8(value)?)?)
            }
            b"fileSize" => self.pending.file_size = Some(std::str::from_utf8(value)?.parse()?),
            // 认不出的键（含错误行）照旧忽略——且不能把它算作「记录已开始」，
            // 否则紧随其后的空行会提交出一条空记录。
            _ => return Ok(()),
        }

        self.started = true;
        Ok(())
    }

    fn commit(&mut self) {
        if self.started {
            self.records.push(std::mem::take(&mut self.pending));
            self.started = false;
        }
    }

    /// 补交末尾那条没有空行收尾的记录。丢掉它会让响应里的最后一个文件看起来根本不在
    /// depot 里，从而给一个已存在的文件报出假的新增。
    fn finish(mut self) -> Vec<DepotFileRecord> {
        self.commit();
        self.records
    }
}

/// 跑一批 fstat，边收边解析。`strict` 为真时非零退出直接失败，而不是只告警。
pub(crate) async fn run_p4_fstat_slice(
    options: &Options,
    work_dir: &str,
    fstat_args: &[&'static str],
    paths: &[String],
    strict: bool,
) -> Result<Vec<DepotFileRecord>> {
    // 路径从 stdin 走：补查传的是 depot 路径，里面同样可能有非 ASCII 字符名，
    // 挂在命令行上会被 Windows 的 ANSI 代码页转换吃掉（见 [`build_p4_command`]）。
    // 定位不到 p4 在这里是硬失败：fstat 是整轮比对的第一步，没有它后面什么都做不了，
    // 与下面 `spawn()?` 对启动失败的态度一致（`strict` 只管 p4 的**退出状态**）。
    let (mut cmd, payload) = build_p4_command(
        crate::locate::p4_exe()?,
        work_dir,
        fstat_args,
        paths,
        options.workspace.as_deref(),
        // fstat 不改状态，不属于任何 changelist。
        None,
    );

    let mut child = cmd.spawn()?;
    let P4Pipes {
        arguments,
        stdout,
        stderr,
    } = take_p4_pipes(&mut child, payload)?;

    let encoding = p4_encoding();
    let verbose = options.verbose;

    let read_stdout = async move {
        let mut reader = futures::io::BufReader::with_capacity(READ_BUFFER_SIZE, stdout);
        let mut parser = FstatParser::new(encoding, 0);
        let mut raw = Vec::new();
        let mut is_first_line = true;

        loop {
            raw.clear();
            if reader.read_until(b'\n', &mut raw).await? == 0 {
                break;
            }

            let line = if is_first_line {
                is_first_line = false;
                strip_bom(&raw, encoding)
            } else {
                &raw
            };
            let line = trim_line_ending(line);

            if verbose {
                sayln!("{}", String::from_utf8_lossy(line));
            }

            parser.push_line(line)?;
        }

        Ok::<_, Error>(parser.finish())
    };

    let read_stderr = read_p4_stderr(stderr);

    // 三条管道必须并发推：只读 stdout 时，塞满 stderr 管道的子进程会永远阻塞；
    // 参数那一侧同理——写不完的大载荷会把 p4 堵在读取上，而它又在等我们读输出。
    let write_args = write_p4_arguments(arguments);
    let (_, records, stderr_bytes) = futures::join!(write_args, read_stdout, read_stderr);
    let records = records?;

    let status = child.status().await?;
    if !status.success() {
        let message = format!(
            "p4 fstat {} exited with {}: {}",
            fstat_args.join(" "),
            status,
            String::from_utf8_lossy(&stderr_bytes).trim()
        );

        // 初次查询沿用旧行为：只提示。补查失败必须终止，否则会拿 head 摘要冒充 have 摘要。
        if strict {
            bail!("{}", message);
        }

        eprintln!("Warning: {}", message);
    }

    Ok(records)
}

/// 分批跑 `p4 fstat`（并发有上限），返回解析好的记录。
pub(crate) async fn run_p4_fstat_batched(
    options: &Options,
    work_dir: &str,
    fstat_args: &[&'static str],
    batched_args: &[String],
    strict: bool,
) -> Result<Vec<DepotFileRecord>> {
    let batches = compute_batches(batched_args);

    sayln!(
        "      Running \"p4 {}\" with {} batches.",
        fstat_args[0],
        batches.len()
    );

    let mut slices = futures::stream::iter(batches)
        .map(|range| {
            run_p4_fstat_slice(options, work_dir, fstat_args, &batched_args[range], strict)
        })
        .buffered(MAX_PARALLEL_P4_COMMANDS);

    let mut records = Vec::new();
    let mut first_error: Option<Error> = None;

    while let Some(slice) = slices.next().await {
        match slice {
            Ok(slice_records) if first_error.is_none() => records.extend(slice_records),
            Ok(_) => {}
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }

    match first_error {
        Some(e) => Err(e),
        None => Ok(records),
    }
}

/// 解析一整组 fstat 行。留作测试的单一入口。
#[cfg(test)]
pub(crate) fn parse_p4_fstat_lines<'a>(
    lines: impl IntoIterator<Item = &'a [u8]>,
    encoding: &'static Encoding,
) -> Result<Vec<DepotFileRecord>> {
    let mut parser = FstatParser::new(encoding, 0);
    for line in lines {
        parser.push_line(line)?;
    }
    Ok(parser.finish())
}

/// fstat 要取的字段，初次查询与补查必须完全一致。
pub(crate) const FSTAT_FIELDS: &str =
    "depotFile clientFile headAction headType headRev haveRev digest fileSize action";

/// 初次查询：一次拿到整个工作区的 depot 状态。
pub(crate) const FSTAT_ARGS: [&str; 5] = [
    "fstat",
    "-Rc", // 只取映射进当前工作区的文件
    "-Ol", // 附上 depot 里文件的大小与摘要
    "-T",
    FSTAT_FIELDS,
];

/// 补查：只查 have 版本。`-L` 让 p4 用内部表整体处理一批 `//depot/file#rev`，比逐个文件查询快得多；
/// 代价是参数语法严格，任何一条出错整条命令都不会执行，所以结果必须严格校验。
pub(crate) const FSTAT_HAVE_ARGS: [&str; 6] = ["fstat", "-Rc", "-Ol", "-L", "-T", FSTAT_FIELDS];

/// 校验补查结果，保证每条 `//depot/file#rev` 都返回了可用的 have 版本记录。
/// 失败或返回不完整时终止 reconcile——回填之后这些字段就是与本地文件比对的基线
/// （见 `model::DepotFileRecord` 的文档），宁可不做，也不能拿 head 版本的摘要顶上。
pub(crate) fn validate_refreshed_records(
    arguments: &[String],
    records: &[DepotFileRecord],
) -> Result<()> {
    let mut expected: HashSet<String> = HashSet::with_capacity(arguments.len());

    for argument in arguments {
        // `-L` 只接受完整 depot 语法加有效版本号，参数形状必须自己先确认。
        let (depot_file, revision) = argument
            .rsplit_once('#')
            .ok_or_else(|| anyhow!("Invalid fstat revision argument \"{}\"", argument))?;
        let revision: u32 = revision
            .parse()
            .map_err(|_| anyhow!("Invalid fstat revision argument \"{}\"", argument))?;

        if !depot_file.starts_with("//") || revision == 0 {
            bail!("Invalid fstat revision argument \"{}\"", argument);
        }

        expected.insert(depot_file.to_ascii_lowercase());
    }

    let mut covered: HashSet<&str> = HashSet::with_capacity(expected.len());

    for record in records {
        let key = record.depot_file_lower.as_str();
        if !expected.contains(key) {
            bail!(
                "p4 fstat returned an unexpected record for \"{}\"",
                record.depot_file
            );
        }

        // 没有 haveRev 说明返回的不是 have 版本，摘要会错位到 head 版本。
        if record.have_rev.is_none() {
            bail!(
                "p4 fstat returned no have revision for \"{}\"",
                record.depot_file
            );
        }

        // 非删除版本必须带类型、大小和摘要，否则后面的比对照样会用到错误数据。
        // 类型名认不出来时 head_type 是空的，但 unsupported_type 有值：这类记录会被
        // 转交 p4 reconcile，后面的比对用不到它的摘要，所以也算「带类型」。
        let has_type = record.head_type.is_some() || record.unsupported_type.is_some();
        if !matches!(
            record.head_action,
            Some(FileAction::Delete | FileAction::MoveDelete)
        ) && (record.head_action.is_none()
            || !has_type
            || record.file_size.is_none()
            || record.digest.is_none())
        {
            bail!(
                "p4 fstat returned an incomplete have record for \"{}\"",
                record.depot_file
            );
        }

        covered.insert(key);
    }

    if let Some(missing) = expected.iter().find(|key| !covered.contains(key.as_str())) {
        bail!("p4 fstat did not return a have record for \"{}\"", missing);
    }

    Ok(())
}

/// 去掉重复的 fstat 记录。
///
/// 重叠的 file spec（例如 `dir/...` 与该目录里的一个文件同时作为范围入口）会让 p4
/// 对同一文件返回多条记录——实测确认过。不去重的话，删除候选会被 push 两次、
/// 命令重复下发。
///
/// 键取**大小写敏感**的 depot 路径：`Snow_Normal.uasset` 与 `Snow_normal.uasset` 是
/// depot 里两个不同的文件，折成小写去重会悄悄吃掉一条，连 `build_mapping` 里那条
/// 「在库的胜过已删除的」决胜规则都跟着失效——而那个场景正是它要处理的。
/// 重叠入口产生的重复记录才是逐字节相同的，用原样路径做键正好只去掉它们。
fn dedupe_records(records: Vec<DepotFileRecord>) -> Vec<DepotFileRecord> {
    let mut seen: HashSet<String> = HashSet::with_capacity(records.len());
    let mut deduped = Vec::with_capacity(records.len());

    for record in records {
        if seen.insert(record.depot_file.clone()) {
            deduped.push(record);
        }
    }

    deduped
}

/// 取走初次查询里的 `(head_rev, head_action)`，作为目标版本的事实。
///
/// 必须在补查**之前**调用：补查会把 `head_type` / `head_action` / `file_size` / `digest`
/// 换成 have 版本的值（见 [`run_p4_fstat_all`] 里的回填），而 sync 要回答的是「拉到哪个
/// 版本」，不是「上次同步的是哪版」。
///
/// 没有 headRev / headAction 的记录不进表。那种形状只出现在「已打开待添加」的文件上
/// （还没进 depot），而 sync 本来就不碰已打开的文件；查不到即视为目标时刻不在库。
pub(crate) fn snapshot_target(records: &[DepotFileRecord]) -> TargetMap {
    records
        .iter()
        .filter_map(|record| {
            Some((
                record.depot_file_lower.clone(),
                TargetRecord {
                    rev: record.head_rev?,
                    action: record.head_action?,
                },
            ))
        })
        .collect()
}

/// 目标为 changelist 时的一次查询：取该 CL 时刻每个文件的修订号与动作。
///
/// 与 [`run_p4_fstat_all`] 的两处不同，都是实测逼出来的：
///
/// - 版本说明符拼在查询载荷里（`./...@<CL>`），与路径一样经 stdin 发放，不经过 Windows 命令行；
/// - **不套用 [`validate_refreshed_records`]**：那套校验要求每条记录都带 `haveRev`，而
///   `@CL` 的返回只在「该文件的 have 恰好就是目标修订」时才带它——套上去会把一次完全
///   正常的查询判死。这里也不补查：要的是目标时刻的事实，不是 have 版本的摘要。
///
/// 查询本身是硬失败（`strict`）：它一旦失败，空结果会被下游读成「目标时刻什么都没有」，
/// 而那个结论会让工具删光本地文件。宁可整轮停下。
pub(crate) async fn run_p4_fstat_at_revision(
    options: &Options,
    work_dir: &str,
    changelist: u32,
    specs: &[String],
) -> Result<TargetMap> {
    sayln!("   Requesting depot state for changelist {changelist}.");
    let start_time = Instant::now();

    // 版本说明符拼在每个入口的 file spec 后面，与路径一样经 stdin 发放。
    let query: Vec<String> = specs
        .iter()
        .map(|spec| format!("{spec}@{changelist}"))
        .collect();
    let records = run_p4_fstat_batched(options, work_dir, &FSTAT_ARGS, &query, true).await?;
    let target = snapshot_target(&records);

    sayln!(
        "      Received {} fstat records for changelist {} in {} seconds.",
        target.len(),
        changelist,
        start_time.elapsed().as_secs_f32()
    );

    Ok(target)
}

/// 查询 depot 状态。第二项是目标版本快照，只有 sync 模式才建（见 [`snapshot_target`]）。
///
/// `specs` 是本轮范围的 file spec 列表（目录已补 `...`）：一个入口一条，一次批量查询。
pub(crate) async fn run_p4_fstat_all(
    options: &Options,
    work_dir: &str,
    specs: &[String],
) -> Result<(DepotState, Option<TargetMap>)> {
    sayln!("   Requesting depot state for all files.");
    let start_time = Instant::now();

    let mut depot_state: DepotState = Default::default();
    // 单入口时每条 file spec 至多回一条记录，不存在重复；而在几十万条记录上建哈希表、
    // 逐条复制路径，是这条路上最贵的恒等变换。
    let records = run_p4_fstat_batched(options, work_dir, &FSTAT_ARGS, specs, false).await?;
    depot_state.file_records = if specs.len() <= 1 {
        records
    } else {
        dedupe_records(records)
    };

    depot_state.build_mapping();

    // 趁 `head_action` 还是初次查询的原值先取走目标版本的事实：下面的补查会把它连同
    // 类型/大小/摘要一起换成 have 版本的值。只有 head 目标的 sync 才建这张表——open /
    // clean 用不上，在几十万条记录上白占几十 MB 内存；`--to` 另走一次查询，这张表它连看都不看。
    let target =
        (options.sync && options.to.is_none()).then(|| snapshot_target(&depot_state.file_records));

    // 找出本地版本落后的记录。
    let mut old_records = Vec::new();
    for record in &depot_state.file_records {
        let (Some(head_rev), Some(have_rev)) = (record.head_rev, record.have_rev) else {
            continue;
        };
        if head_rev == have_rev {
            continue;
        }

        old_records.push([record.depot_file.clone(), have_rev.to_string()].join("#"));

        if options.verbose {
            sayln!(
                "         File \"{}\" is outdated (head rev {}, have rev {}",
                record.depot_file,
                head_rev,
                have_rev
            );
        }
    }

    sayln!(
        "      Received {} fstat records in {} seconds.",
        depot_state.file_records.len(),
        start_time.elapsed().as_secs_f32()
    );

    // 为这些落后的文件补查记录，把摘要换成本地那一版的。
    if !old_records.is_empty() {
        sayln!(
            "   Requesting depot state for {} outdated files.",
            old_records.len()
        );

        let start_time = Instant::now();

        // `-L` 是批量参数一起查的优化，文档要求参数是完整的 depot 语法加版本号；
        // 只有一条记录时没有收益，就不冒语法风险，按原样查询。
        let refresh_args: Vec<&str> = if old_records.len() > 1 {
            FSTAT_HAVE_ARGS.to_vec()
        } else {
            FSTAT_ARGS.to_vec()
        };

        let refreshed_records =
            run_p4_fstat_batched(options, work_dir, &refresh_args, &old_records, true).await?;

        // 补查必须完整，否则不能用它的摘要去比对本地文件。
        validate_refreshed_records(&old_records, &refreshed_records)?;

        for refreshed_record in &refreshed_records {
            match depot_state.get_depot_record_mut(&refreshed_record.depot_file_lower) {
                Some(original_record) => {
                    // 这些字段描述的是 **have 版本**——补查查的就是 have 那一版，回填后
                    // 它们是与本地文件比对的基线（见 `model::DepotFileRecord` 的文档）。
                    // `head_rev` / `have_rev` / `action` 刻意不动：head_rev 仍是真实 head，
                    // 分析层靠它与 have_rev 的差判断本地是否落后。
                    original_record.head_type = refreshed_record.head_type;
                    original_record.unsupported_type = refreshed_record.unsupported_type.clone();
                    original_record.head_action = refreshed_record.head_action;
                    original_record.file_size = refreshed_record.file_size;
                    original_record.digest = refreshed_record.digest;

                    if options.verbose {
                        sayln!(
                            "         Updated record for \"{}\"",
                            original_record.depot_file
                        );
                    }
                }
                None => {
                    bail!(
                        "Failed to find original record for \"{}\"",
                        refreshed_record.depot_file
                    )
                }
            }
        }

        sayln!(
            "      Updated {} fstat records in {} seconds.",
            refreshed_records.len(),
            start_time.elapsed().as_secs_f32()
        );
    }

    Ok((depot_state, target))
}

#[cfg(test)]
mod tests {
    use super::*;

    use encoding_rs::UTF_8;

    use crate::model::FileType;

    #[test]
    fn parses_fstat_records_with_a_trailing_record_lacking_a_blank_line() {
        // 最后一条记录后面没有空行。旧解析器只在空行处提交，于是静默丢掉了那条记录——
        // 结果就是给一个 depot 里已经存在的文件报出假的新增。
        let lines: Vec<&[u8]> = vec![
            b"... depotFile //depot/a.txt",
            b"... clientFile E:\\ws\\a.txt",
            b"... headRev 3",
            b"... haveRev 3",
            b"... fileSize 1234",
            b"",
            b"... depotFile //depot/\xe4\xb8\xad\xe6\x96\x87.txt",
            b"... clientFile E:\\ws\\\xe4\xb8\xad\xe6\x96\x87.txt",
            b"... headRev 1",
        ];

        let records = parse_p4_fstat_lines(lines, UTF_8).unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].depot_file, "//depot/a.txt");
        assert_eq!(records[0].head_rev, Some(3));
        assert_eq!(records[0].have_rev, Some(3));
        assert_eq!(records[0].file_size, Some(1234));
        // 非 ASCII 路径解码后原样保留，查询键照旧走平台那一套路径身份。
        assert_eq!(records[1].client_file, "E:\\ws\\中文.txt");
        assert_eq!(
            records[1].client_file_lower,
            local_path_key("E:\\ws\\中文.txt")
        );
    }

    /// 去重只该去掉「同一条记录被查了两遍」，不能顺手吃掉大小写不同的另一个文件。
    ///
    /// 折成小写去重会连 `build_mapping` 里「在库的胜过已删除的」那条决胜规则一起废掉：
    /// 它要处理的正是这一个小写键下的两个文件。
    #[test]
    fn dedupe_records_keeps_case_variants_and_drops_true_repeats() {
        let lines: Vec<&[u8]> = vec![
            b"... depotFile //depot/Snow_Normal.uasset",
            b"... clientFile E:\\ws\\Snow_Normal.uasset",
            b"... headRev 1",
            b"",
            b"... depotFile //depot/Snow_normal.uasset",
            b"... clientFile E:\\ws\\Snow_normal.uasset",
            b"... headRev 1",
            b"",
            // 重叠入口（`dir/...` 与该目录里的文件）造成的原样重复。
            b"... depotFile //depot/Snow_Normal.uasset",
            b"... clientFile E:\\ws\\Snow_Normal.uasset",
            b"... headRev 1",
            b"",
        ];

        let records = parse_p4_fstat_lines(lines, UTF_8).unwrap();
        let deduped = dedupe_records(records);

        let kept: Vec<&str> = deduped
            .iter()
            .map(|record| record.depot_file.as_str())
            .collect();
        assert_eq!(
            kept,
            vec!["//depot/Snow_Normal.uasset", "//depot/Snow_normal.uasset"]
        );
    }

    /// 目标快照的取值口径：认 `headRev` 与 `headAction`，缺一个就不进表。
    ///
    /// 两条断言各挡一件事：
    ///
    /// - 第一条记录 `haveRev` 是 1、`headRev` 是 3，快照必须是 3——取成 have 的话，sync 会把
    ///   「have 与目标一致」错判成「落后」，白传一轮；
    /// - 第二条只有 `headRev`、没有 `headAction`，不进表。「查不到」在分析层的含义是「目标
    ///   时刻该路径不在库」，落下去就是删本地文件，所以这里不拿别的字段凑数。
    #[test]
    fn snapshot_target_takes_the_head_fields_and_skips_partial_records() {
        let lines: Vec<&[u8]> = vec![
            b"... depotFile //depot/a.txt",
            b"... clientFile E:\\ws\\a.txt",
            b"... headType text",
            b"... headAction edit",
            b"... headRev 3",
            b"... haveRev 1",
            b"",
            b"... depotFile //depot/b.txt",
            b"... clientFile E:\\ws\\b.txt",
            b"... headRev 2",
        ];

        let records = parse_p4_fstat_lines(lines, UTF_8).unwrap();
        let target = snapshot_target(&records);

        assert_eq!(target.len(), 1);
        assert_eq!(
            target.get("//depot/a.txt"),
            Some(&TargetRecord {
                rev: 3,
                action: FileAction::Edit,
            })
        );
    }

    /// 认不出的 `headType` 不能中止整轮 fstat：原串记下来，由分析阶段转交 p4 reconcile。
    #[test]
    fn an_unknown_head_type_is_recorded_instead_of_failing_the_parse() {
        let lines: Vec<&[u8]> = vec![
            b"... depotFile //depot/a.txt",
            b"... clientFile E:\\ws\\a.txt",
            b"... headType tempobj",
            b"... headRev 3",
            b"... haveRev 3",
            b"",
            b"... depotFile //depot/b.txt",
            b"... clientFile E:\\ws\\b.txt",
            b"... headType text+k",
            b"",
        ];

        let records = parse_p4_fstat_lines(lines, UTF_8).unwrap();

        // 一条认不出类型的记录不影响同一批里的其它记录。
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].head_type, None);
        assert_eq!(records[0].unsupported_type.as_deref(), Some("tempobj"));
        assert_eq!(records[1].head_type, Some(FileType::Text));
        assert_eq!(records[1].unsupported_type, None);
    }

    #[test]
    fn blank_lines_never_emit_empty_records() {
        let lines: Vec<&[u8]> = vec![
            b"",
            b"//depot/missing.txt - no such file(s).",
            b"",
            b"... depotFile //depot/a.txt",
            b"... clientFile E:\\ws\\a.txt",
            b"",
            b"",
        ];

        let records = parse_p4_fstat_lines(lines, UTF_8).unwrap();

        // 旧解析器每遇到一个空行就推一条记录，空记录也算。
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].depot_file, "//depot/a.txt");
    }

    #[test]
    fn parses_fstat_digests_and_ignores_unknown_keys() {
        let lines: Vec<&[u8]> = vec![
            b"... depotFile //depot/a.bin",
            b"... somethingUnknown value",
            b"... digest 0123456789ABCDEF0123456789ABCDEF",
        ];

        let records = parse_p4_fstat_lines(lines, UTF_8).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].digest,
            Some([
                0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB,
                0xCD, 0xEF,
            ])
        );
    }

    // ---- fstat 补查 ----

    /// 一条字段齐全的补查记录。
    fn refreshed_record(depot_file: &str) -> DepotFileRecord {
        DepotFileRecord {
            depot_file: depot_file.to_owned(),
            depot_file_lower: depot_file.to_ascii_lowercase(),
            client_file: "C:\\WS\\F.txt".to_owned(),
            client_file_lower: "c:\\ws\\f.txt".to_owned(),
            head_type: Some(FileType::Text),
            unsupported_type: None,
            head_action: Some(FileAction::Edit),
            head_rev: Some(2),
            have_rev: Some(1),
            action: None,
            file_size: Some(10),
            digest: Some([7; 16]),
        }
    }

    #[test]
    fn refresh_validation_accepts_a_complete_response() {
        let arguments = vec!["//depot/a.txt#1".to_owned()];
        let records = vec![refreshed_record("//depot/a.txt")];

        assert!(validate_refreshed_records(&arguments, &records).is_ok());
    }

    /// 类型认不出来时 head_type 是空的，但有 unsupported_type：这类记录会被转交
    /// p4 reconcile，摘要用不到，所以不该判成「字段不完整」。真正缺字段的仍要拒。
    #[test]
    fn refresh_validation_accepts_a_type_it_cannot_digest() {
        let arguments = vec!["//depot/a.txt#1".to_owned()];

        let unknown_type = vec![DepotFileRecord {
            head_type: None,
            unsupported_type: Some("tempobj".to_owned()),
            ..refreshed_record("//depot/a.txt")
        }];
        assert!(validate_refreshed_records(&arguments, &unknown_type).is_ok());

        let missing_type = vec![DepotFileRecord {
            head_type: None,
            ..refreshed_record("//depot/a.txt")
        }];
        assert!(validate_refreshed_records(&arguments, &missing_type).is_err());
    }

    #[test]
    fn refresh_validation_requires_every_requested_record() {
        let arguments = vec!["//depot/a.txt#1".to_owned(), "//depot/b.txt#2".to_owned()];

        // 少一条记录说明返回值不完整，不能用 head 摘要顶上
        assert!(
            validate_refreshed_records(&arguments, &[refreshed_record("//depot/a.txt")]).is_err()
        );

        // 多出来的记录说明参数与结果对不上
        let extra = vec![
            refreshed_record("//depot/a.txt"),
            refreshed_record("//depot/b.txt"),
            refreshed_record("//depot/c.txt"),
        ];
        assert!(validate_refreshed_records(&arguments, &extra).is_err());

        // depot 路径大小写不同仍算同一条
        let case_diff = vec![
            refreshed_record("//depot/A.txt"),
            refreshed_record("//depot/b.txt"),
        ];
        assert!(validate_refreshed_records(&arguments, &case_diff).is_ok());
    }

    #[test]
    fn refresh_validation_rejects_malformed_arguments() {
        // `-L` 只接受完整 depot 语法加有效版本号
        for argument in [
            "//depot/a.txt",
            "//depot/a.txt#0",
            "depot/a.txt#1",
            "//depot/a.txt#x",
        ] {
            assert!(
                validate_refreshed_records(&[argument.to_owned()], &[]).is_err(),
                "{argument} should be rejected"
            );
        }
    }

    #[test]
    fn refresh_validation_requires_the_fields_the_analysis_uses() {
        let arguments = vec!["//depot/a.txt#1".to_owned()];

        for missing in 0..4 {
            let mut record = refreshed_record("//depot/a.txt");
            match missing {
                0 => record.digest = None,
                1 => record.file_size = None,
                2 => record.head_type = None,
                _ => record.have_rev = None,
            }
            assert!(
                validate_refreshed_records(&arguments, &[record]).is_err(),
                "missing field {missing} should be rejected"
            );
        }

        let mut no_action = refreshed_record("//depot/a.txt");
        no_action.head_action = None;
        assert!(validate_refreshed_records(&arguments, &[no_action]).is_err());

        // have 版本本身是删除版本时没有摘要和大小，属于正常情况
        let mut deleted = refreshed_record("//depot/a.txt");
        deleted.head_action = Some(FileAction::Delete);
        deleted.head_type = None;
        deleted.file_size = None;
        deleted.digest = None;
        assert!(validate_refreshed_records(&arguments, &[deleted]).is_ok());
    }

    #[test]
    fn only_the_have_revision_query_uses_the_label_table() {
        // `-L` 只出现在补查里，初次查询保持原样
        assert!(!FSTAT_ARGS.contains(&"-L"));
        assert!(FSTAT_HAVE_ARGS.contains(&"-L"));

        // 两次查询的字段必须完全一致，否则补查会丢掉比对需要的字段
        assert_eq!(FSTAT_ARGS[0], FSTAT_HAVE_ARGS[0]);
        assert_eq!(
            *FSTAT_ARGS.last().unwrap(),
            *FSTAT_HAVE_ARGS.last().unwrap()
        );
        assert_eq!(
            FSTAT_ARGS.iter().filter(|arg| **arg == "-Rc").count(),
            FSTAT_HAVE_ARGS.iter().filter(|arg| **arg == "-Rc").count()
        );
    }
}
