//! `p4 fstat` 查询与流式解析。

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use anyhow::{Error, Result, bail};
use encoding_rs::Encoding;
use futures::StreamExt;
use futures::io::AsyncBufReadExt;
use hex::FromHex;

use crate::READ_BUFFER_SIZE;
use crate::charset::{decode_p4_bytes, p4_encoding, strip_bom, trim_line_ending};
use crate::cli::Options;
use crate::json::{P4Command, count_p4_command, sayln};
use crate::model::{DepotFileRecord, DepotState, FileAction, TargetMap, TargetRecord};
use crate::p4::process::{
    MAX_PARALLEL_P4_COMMANDS, P4Pipes, build_p4_command, compute_batches, read_p4_stderr,
    take_p4_pipes, write_p4_arguments,
};
use crate::path::{local_path_key, normalize_local_path_owned};
use crate::scope::ExcludeSet;

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
    // 起来了才算一次调用，见 [`count_p4_command`]；上面那个 `?` 失败时早退，不记账。
    count_p4_command(P4Command::Fstat);
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

/// 目标快照要取的字段：只要「那一版是几号、什么动作」。
pub(crate) const FSTAT_TARGET_FIELDS: &str = "depotFile headRev headAction";

/// 目标快照的查询参数：与 [`FSTAT_ARGS`] 差在**不取内容字段**。
///
/// `-Ol` 是「附上 depot 里文件的大小与摘要」——往往要 p4 为范围内的文件读内容或查服务端的
/// 摘要数据，在大工作区上是实打实的一笔。而 [`snapshot_target`] 一个字节都不看，取回来纯属白费。
/// `clientFile` 同理不取：目标表按 depot 路径查（`analyze_at_target` 只读 `depot_file`）。
pub(crate) const FSTAT_TARGET_ARGS: [&str; 4] = ["fstat", "-Rc", "-T", FSTAT_TARGET_FIELDS];

/// 一条历史基线补查请求：把 `records` 里某一条记录的 have 版本取回来。
///
/// 带下标是因为**depot 路径不能当键**：库里可以同时存在只差大小写的两个路径
/// （`Snow_caseA.uasset` / `snow_casea.uasset`），它们在大小写不敏感的服务器和客户端上折叠成
/// 同一个键。按折叠键回填，就是把一条记录的 have 版字段写到另一条头上，而这两组字段正是
/// 「与本地文件比对的基线」——写错等于拿错版本、甚至错路径的摘要去比对。
#[derive(Debug)]
pub(crate) struct BaselineRequest {
    /// 记录在 `DepotState::file_records` 里的下标。
    index: usize,
    /// 精确大小写的 depot 路径。
    depot_file: String,
    /// 要取回的修订号，也就是这条记录的 have 版本。
    revision: u32,
}

impl BaselineRequest {
    /// p4 参数形态：完整 depot 语法加版本号（`-L` 只接受这种写法）。
    fn argument(&self) -> String {
        format!("{}#{}", self.depot_file, self.revision)
    }
}

/// 补查的范围。
pub(crate) enum BaselineScope<'a> {
    /// 所有 head 与 have 不一致的记录——open / clean 的既有行为，一个字节都不改。
    Outdated,
    /// sync：只补查目标处活跃、have 恰好停在目标版本的那批。
    ///
    /// 判据是 `analyze_at_target` 里唯一会读 type / size / digest 的分支（`have ==
    /// target_rev` 且目标处该路径还在库，即那里 `check` 一组）的**超集**：多查几条只是
    /// 多花一点，**少查一条就会让那一条带着 head 版本的字段去做 have 版本的比对**。
    /// 所以「本地存在」「类型认不出来」这类只有分析期才知道的条件刻意不搬进来。
    ///
    /// 这是**刻意的保守过滤，不是精准筛选**：选中的记录里会混进一些最终不进 `check` 的
    /// （本地缺失、二进制长度已不符、类型算不出摘要）。多花的那点补查没有正确性代价，
    /// 换来的是判据只依赖这里已有的信息，不会随分析层的分支变化而漏选。
    AtTarget {
        target: &'a TargetMap,
        excludes: &'a ExcludeSet,
    },
}

/// 挑出需要补查基线的记录。
pub(crate) fn select_baseline_requests(
    records: &[DepotFileRecord],
    scope: &BaselineScope<'_>,
) -> Vec<BaselineRequest> {
    let mut requests = Vec::new();

    for (index, record) in records.iter().enumerate() {
        let (Some(head_rev), Some(have_rev)) = (record.head_rev, record.have_rev) else {
            continue;
        };
        // head 与 have 一致时，初次查询给的字段本来就是 have 版本的，没有可补的。
        if head_rev == have_rev {
            continue;
        }

        if let BaselineScope::AtTarget { target, excludes } = scope {
            // 已打开的文件 sync 一律不碰；排除名单里的记录也不处理。
            if record.action.is_some() || excludes.excludes_key(&record.client_file_lower) {
                continue;
            }
            // 目标时刻不在库、只剩删除版本、内容已归档：都不做摘要比对，不需要基线。
            // 按**原样 depot 路径**查（目标表就是这个口径）：折成小写会让只差大小写的
            // 另一条路径的状态顶上来，选错或漏选补查。
            match target.get(&record.depot_file) {
                Some(target_record)
                    if target_record.rev == have_rev
                        && !matches!(
                            target_record.action,
                            FileAction::Delete | FileAction::MoveDelete | FileAction::Archive
                        ) => {}
                _ => continue,
            }
        }

        requests.push(BaselineRequest {
            index,
            depot_file: record.depot_file.clone(),
            revision: have_rev,
        });
    }

    requests
}

/// 把补查结果回填到请求指定的记录上。
///
/// 定位靠**精确的 depot 路径**（外加「回显的大小写与请求不同」这一档回退），不靠折叠键：
/// depot 里可以同时存在只差大小写的两个路径，而它们折叠成同一个键——参见
/// [`BaselineRequest`]。返回的还必须是**被请求的那一版**：p4 对带版本号的参数应当只回
/// 那一条，但它若回了别的版本，回填的字段会静默描述另一个版本（head、或大小写冲突的
/// 另一条路径）的下游比对基线。
///
/// 失败或不完整一律中止整轮：回填之后这些字段就是与本地文件比对的基线（见
/// `model::DepotFileRecord` 的文档），宁可不做，也不能拿 head 版本的摘要顶上。
fn apply_refreshed_baselines(
    records: &mut [DepotFileRecord],
    requests: &[BaselineRequest],
    refreshed: &[DepotFileRecord],
) -> Result<()> {
    // 精确路径 -> 请求下标。同一个路径可能被多条记录请求（同一个 depot 路径落在两个
    // 本地根上，或输入的 scope 入口重叠），它们要的是同一份答案。
    let mut exact: HashMap<&str, Vec<usize>> = HashMap::with_capacity(requests.len());
    for (index, request) in requests.iter().enumerate() {
        exact
            .entry(request.depot_file.as_str())
            .or_default()
            .push(index);
    }

    let mut satisfied = vec![false; requests.len()];

    for refreshed_record in refreshed {
        let candidates: Vec<usize> = match exact.get(refreshed_record.depot_file.as_str()) {
            Some(indices) => indices.clone(),
            None => {
                // 大小写不敏感的服务器可能按库里的写法回显（与我们请求的写法不同）。
                // 这一档只有在**唯一命中**时才敢认：两条只差大小写的记录互认，正是这个
                // 函数要挡掉的事。
                let mut matches = requests.iter().enumerate().filter(|(_, request)| {
                    request
                        .depot_file
                        .eq_ignore_ascii_case(&refreshed_record.depot_file)
                });
                match (matches.next(), matches.next()) {
                    (Some((index, _)), None) => vec![index],
                    _ => bail!(
                        "p4 fstat returned an unexpected record for \"{}\"",
                        refreshed_record.depot_file
                    ),
                }
            }
        };

        // 同一条 depot 路径可能被请求了不同的修订（两个本地根各自停在不同的版本上）：
        // 这一条回答只交代它带回的那个版本，没有一条请求要它就算错。
        let answered: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&index| refreshed_record.head_rev == Some(requests[index].revision))
            .collect();
        let request = &requests[candidates[0]];
        if answered.is_empty() {
            bail!(
                "p4 fstat returned revision {} for \"{}\", which asked for revision {}",
                refreshed_record
                    .head_rev
                    .map_or("none".to_owned(), |rev| rev.to_string()),
                request.depot_file,
                request.revision
            );
        }

        // 没有 haveRev 说明返回的记录不是这个客户端 have 列表里的那条。
        if refreshed_record.have_rev.is_none() {
            bail!(
                "p4 fstat returned no have revision for \"{}\"",
                request.depot_file
            );
        }

        // 非删除版本必须带类型、大小和摘要，否则后面的比对照样会用到错误数据。
        // 类型名认不出来时 head_type 是空的，但 unsupported_type 有值：这类记录会被
        // 转交原生 sync，后面的比对用不到它的摘要，所以也算「带类型」。
        let has_type =
            refreshed_record.head_type.is_some() || refreshed_record.unsupported_type.is_some();
        if !matches!(
            refreshed_record.head_action,
            Some(FileAction::Delete | FileAction::MoveDelete)
        ) && (refreshed_record.head_action.is_none()
            || !has_type
            || refreshed_record.file_size.is_none()
            || refreshed_record.digest.is_none())
        {
            bail!(
                "p4 fstat returned an incomplete record for \"{}\" (revision {})",
                request.depot_file,
                request.revision
            );
        }

        for &request_index in &answered {
            // 一条请求只能被一条回答填：第二次返回的会被静默丢掉，而它们本该是同一条。
            if std::mem::replace(&mut satisfied[request_index], true) {
                bail!(
                    "p4 fstat returned more than one record for \"{}\"#{}",
                    request.depot_file,
                    requests[request_index].revision
                );
            }

            // 回填按下标定位，而请求是**查询之前**按下标挑出来的。下标一旦对不上（记录
            // 被重排过），字段就落到了别的文件上——比回填错版本更隐蔽。写入前对一次
            // 原样路径与 have 版本，对不上就停下。
            let request = &requests[request_index];
            let original_record = &mut records[request.index];
            if original_record.depot_file != request.depot_file
                || original_record.have_rev != Some(request.revision)
            {
                bail!(
                    "Refusing to apply the refreshed record for \"{}\"#{}: the record it was \
                     selected from no longer matches",
                    request.depot_file,
                    request.revision
                );
            }

            // 这些字段描述的是 **have 版本**——补查查的就是 have 那一版，回填后它们是与本地
            // 文件比对的基线（见 `model::DepotFileRecord` 的文档）。`head_rev` / `have_rev` /
            // `action` 刻意不动：head_rev 仍是真实 head，分析层靠它与 have_rev 的差判断本地
            // 是否落后。
            original_record.head_type = refreshed_record.head_type;
            original_record.unsupported_type = refreshed_record.unsupported_type.clone();
            original_record.head_action = refreshed_record.head_action;
            original_record.file_size = refreshed_record.file_size;
            original_record.digest = refreshed_record.digest;
        }
    }

    if let Some(missing) = requests
        .iter()
        .zip(&satisfied)
        .find(|(_, satisfied)| !**satisfied)
    {
        bail!(
            "p4 fstat did not return a record for \"{}\"#{}, which the content comparison needs",
            missing.0.depot_file,
            missing.0.revision
        );
    }

    Ok(())
}

/// 补查 `requests` 并回填。查询参数与 `-L` 的选择保持原样：多条才走批量表，单条退回普通查询。
pub(crate) async fn refresh_baselines(
    options: &Options,
    work_dir: &str,
    records: &mut [DepotFileRecord],
    requests: &[BaselineRequest],
) -> Result<()> {
    if requests.is_empty() {
        return Ok(());
    }

    sayln!(
        "   Requesting depot state for {} file(s) whose content needs comparing.",
        requests.len()
    );

    let start_time = Instant::now();
    let arguments: Vec<String> = requests.iter().map(BaselineRequest::argument).collect();

    let refresh_args: Vec<&str> = if arguments.len() > 1 {
        FSTAT_HAVE_ARGS.to_vec()
    } else {
        FSTAT_ARGS.to_vec()
    };

    let refreshed_records =
        run_p4_fstat_batched(options, work_dir, &refresh_args, &arguments, true).await?;
    apply_refreshed_baselines(records, requests, &refreshed_records)?;

    if options.verbose {
        for request in requests {
            sayln!("         Updated record for \"{}\"", request.depot_file);
        }
    }

    sayln!(
        "      Updated {} fstat record(s) in {} seconds.",
        requests.len(),
        start_time.elapsed().as_secs_f32()
    );

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
/// 换成 have 版本的值（见 [`refresh_baselines`] 里的回填），而 sync 要回答的是「拉到哪个
/// 版本」，不是「上次同步的是哪版」。
///
/// 没有 headRev / headAction 的记录不进表。那种形状只出现在「已打开待添加」的文件上
/// （还没进 depot），而 sync 本来就不碰已打开的文件；查不到即视为目标时刻不在库。
///
/// 键是**原样 depot 路径**：只差大小写的两个路径在本地折叠成同一个文件，但在库里是两条
/// 独立的历史，目标时刻的状态可以一头在库、一头是删除版本。折成小写会让后插入的那条盖掉
/// 先来的（见 [`crate::model::TargetMap`]）。
pub(crate) fn snapshot_target(records: &[DepotFileRecord]) -> TargetMap {
    records
        .iter()
        .filter_map(|record| {
            Some((
                record.depot_file.clone(),
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
/// 与 [`query_depot_state`] 的三处不同，都是实测逼出来的：
///
/// - 版本说明符拼在查询载荷里（`./...@<CL>`），与路径一样经 stdin 发放，不经过 Windows 命令行；
/// - **只取修订号与动作**（[`FSTAT_TARGET_ARGS`]）：目标表用不到任何内容字段；
/// - **不套用补查与那套完整性校验**：`@CL` 的返回只在「该文件的 have 恰好就是目标修订」时
///   才带 `haveRev`（校验要求它每条都有），套上去会把一次完全正常的查询判死。这里也不补查：
///   要的是目标时刻的事实，不是 have 版本的摘要。
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
    let records = run_p4_fstat_batched(options, work_dir, &FSTAT_TARGET_ARGS, &query, true).await?;
    let target = snapshot_target(&records);

    sayln!(
        "      Received {} fstat records for changelist {} in {} seconds.",
        target.len(),
        changelist,
        start_time.elapsed().as_secs_f32()
    );

    Ok(target)
}

/// 查询 depot 状态。第二项是 head 目标快照，只有 sync 且没有 `--to` 时才建
/// （见 [`snapshot_target`]）——`--to` 的目标另走一次查询（[`run_p4_fstat_at_revision`]）。
///
/// `specs` 是本轮范围的 file spec 列表（目录已补 `...`）：一个入口一条，一次批量查询。
///
/// 历史基线补查刻意**不**在这里做：`--sync --to <CL>` 的目标快照决定了哪些记录真的需要
/// 内容比对（[`BaselineScope::AtTarget`]），而它是另一路并发查询的结果；调用方拿到目标后
/// 再调 [`refresh_baselines`]。
pub(crate) async fn query_depot_state(
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

    if options.verbose {
        for record in &depot_state.file_records {
            if let (Some(head_rev), Some(have_rev)) = (record.head_rev, record.have_rev)
                && head_rev != have_rev
            {
                sayln!(
                    "         File \"{}\" is outdated (head rev {}, have rev {}",
                    record.depot_file,
                    head_rev,
                    have_rev
                );
            }
        }
    }

    sayln!(
        "      Received {} fstat records in {} seconds.",
        depot_state.file_records.len(),
        start_time.elapsed().as_secs_f32()
    );

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

    /// 目标表按**原样 depot 路径**建键。depot 里可以同时存在只差大小写的两个路径，它们在
    /// 本地折叠成同一个文件，但在库里是两条独立的历史——折成小写会让后插入的那条盖掉先来的
    /// （现场就是「在库的 A 被已删除的 B 盖掉」，分析层据此把本地文件删了）。
    #[test]
    fn snapshot_target_keeps_case_variant_paths_apart() {
        let lines: Vec<&[u8]> = vec![
            b"... depotFile //depot/main/Snow.uasset",
            b"... headAction edit",
            b"... headRev 5",
            b"",
            b"... depotFile //depot/main/snow.uasset",
            b"... headAction delete",
            b"... headRev 3",
            b"",
        ];

        let records = parse_p4_fstat_lines(lines, UTF_8).unwrap();
        let target = snapshot_target(&records);

        assert_eq!(target.len(), 2, "两条大小写变体各自入表，不许互相盖掉");
        assert_eq!(
            target.get("//depot/main/Snow.uasset"),
            Some(&TargetRecord {
                rev: 5,
                action: FileAction::Edit,
            })
        );
        assert_eq!(
            target.get("//depot/main/snow.uasset"),
            Some(&TargetRecord {
                rev: 3,
                action: FileAction::Delete,
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

    // ---- fstat 历史基线补查 ----

    use crate::path::local_path_key;

    /// 一条 head 落后于 have 的记录（head rev2 / have rev1）。
    fn outdated_record(depot_file: &str) -> DepotFileRecord {
        DepotFileRecord {
            depot_file: depot_file.to_owned(),
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

    /// 同上，但落在指定的本地路径上（排除集合按本地路径键匹配）。
    fn outdated_record_at(depot_file: &str, client_file: &str) -> DepotFileRecord {
        DepotFileRecord {
            client_file: client_file.to_owned(),
            client_file_lower: local_path_key(client_file),
            ..outdated_record(depot_file)
        }
    }

    /// 补查的返回形状：`p4 fstat //depot/f#1` 回的是**被请求的那一版**。
    fn have_answer(depot_file: &str, revision: u32) -> DepotFileRecord {
        DepotFileRecord {
            head_rev: Some(revision),
            ..outdated_record(depot_file)
        }
    }

    fn target_map(entries: &[(&str, u32, FileAction)]) -> TargetMap {
        entries
            .iter()
            .map(|(depot_file, rev, action)| {
                (
                    (*depot_file).to_owned(),
                    TargetRecord {
                        rev: *rev,
                        action: *action,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn baseline_requests_pick_the_outdated_records_and_their_have_revision() {
        let records = vec![
            outdated_record("//depot/a.txt"),
            // head 与 have 一致：初次查询给的字段就是 have 版本的，没有可补的。
            DepotFileRecord {
                head_rev: Some(1),
                ..outdated_record("//depot/b.txt")
            },
            // 没同步过：没有 have 版本可查。
            DepotFileRecord {
                have_rev: None,
                ..outdated_record("//depot/c.txt")
            },
        ];

        let requests = select_baseline_requests(&records, &BaselineScope::Outdated);

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].index, 0);
        assert_eq!(requests[0].depot_file, "//depot/a.txt");
        assert_eq!(requests[0].revision, 1);
        assert_eq!(requests[0].argument(), "//depot/a.txt#1");
    }

    /// depot 里可以同时存在只差大小写的两个路径，它们在本地折叠成同一个文件。补查必须把
    /// 每条记录**自己**的 have 版字段回填给它自己：按折叠键定位会让一条记录的字段落到另一条
    /// 头上，而分析层随后拿这组字段当「本地文件该是什么样」的基线。
    #[test]
    fn case_variant_requests_keep_their_own_baseline() {
        let mut records = vec![
            // 旧大小写：head 已删除。
            DepotFileRecord {
                head_action: Some(FileAction::Delete),
                file_size: None,
                digest: None,
                ..outdated_record("//depot/main/DLTask_ABC.uasset")
            },
            // 新大小写：head 还在库。
            outdated_record("//depot/main/DLTask_abc.uasset"),
        ];

        let requests = select_baseline_requests(&records, &BaselineScope::Outdated);
        assert_eq!(requests.len(), 2, "两条记录各自都要补查");

        let refreshed = vec![
            DepotFileRecord {
                digest: Some([1; 16]),
                ..have_answer("//depot/main/DLTask_ABC.uasset", 1)
            },
            DepotFileRecord {
                digest: Some([2; 16]),
                ..have_answer("//depot/main/DLTask_abc.uasset", 1)
            },
        ];

        apply_refreshed_baselines(&mut records, &requests, &refreshed).unwrap();

        // 每条记录拿到的是它自己被请求的那一版。
        assert_eq!(records[0].digest, Some([1; 16]));
        assert_eq!(records[1].digest, Some([2; 16]));
        // head 已删除的那条记录，回填后描述的是它的 have 版本（rev1 这一版在库、有内容）。
        assert_eq!(records[0].head_action, Some(FileAction::Edit));
        assert_eq!(records[0].file_size, Some(10));
        // 两个版本号仍是各自真实的值，回填不碰。
        assert_eq!(records[0].head_rev, Some(2));
        assert_eq!(records[0].have_rev, Some(1));
    }

    /// 同一条 depot 路径被两条记录请求（两个本地根各持一份，或 scope 入口重叠）：
    /// 一条回答就够，两条都要拿到基线——按折叠键定位时只会有一条被填上。
    #[test]
    fn one_answer_fills_every_request_for_the_same_path() {
        let mut records = vec![
            outdated_record_at("//depot/a.txt", "C:/ws/one/a.txt"),
            outdated_record_at("//depot/a.txt", "C:/ws/two/a.txt"),
        ];
        let requests = select_baseline_requests(&records, &BaselineScope::Outdated);
        assert_eq!(requests.len(), 2);

        let refreshed = vec![have_answer("//depot/a.txt", 1)];
        apply_refreshed_baselines(&mut records, &requests, &refreshed).unwrap();
        for record in &records {
            assert_eq!(record.digest, Some([7; 16]));
            assert_eq!(record.head_action, Some(FileAction::Edit));
        }

        // 同一个路径但请求的是不同修订时，回答各归各的；一条回答不够填两条。
        let mut records = vec![
            outdated_record_at("//depot/a.txt", "C:/ws/one/a.txt"),
            DepotFileRecord {
                head_rev: Some(3),
                have_rev: Some(2),
                ..outdated_record_at("//depot/a.txt", "C:/ws/two/a.txt")
            },
        ];
        let requests = select_baseline_requests(&records, &BaselineScope::Outdated);
        assert_eq!(requests.len(), 2);
        assert!(
            apply_refreshed_baselines(&mut records, &requests, &[have_answer("//depot/a.txt", 1)])
                .is_err()
        );
    }

    #[test]
    fn baseline_apply_rejects_an_answer_about_another_revision() {
        // 参数要的是 `#1`，返回的却是 head（rev2）那一版：字段会静默描述另一个版本。
        let mut records = vec![outdated_record("//depot/a.txt")];
        let requests = select_baseline_requests(&records, &BaselineScope::Outdated);
        let refreshed = vec![have_answer("//depot/a.txt", 2)];

        let error = apply_refreshed_baselines(&mut records, &requests, &refreshed)
            .unwrap_err()
            .to_string();

        assert!(error.contains("//depot/a.txt"), "{error}");
        assert!(error.contains("revision"), "{error}");
    }

    /// 大小写不敏感的服务器可能按库里的写法回显；这一档只有在**唯一命中**时才认——
    /// 同一个折叠键上还有第二条请求时，互认正是这个函数要挡掉的事。
    #[test]
    fn baseline_apply_accepts_a_unique_case_insensitive_echo() {
        let mut records = vec![outdated_record("//depot/a.txt")];
        let requests = select_baseline_requests(&records, &BaselineScope::Outdated);

        let echoed = vec![have_answer("//depot/A.TXT", 1)];
        assert!(apply_refreshed_baselines(&mut records, &requests, &echoed).is_ok());

        // 两条只差大小写的请求同时存在：回显的写法对他们都不精确，只能靠折叠比，
        // 而折叠比命中两条——无从判断这是哪一条的答案，必须拒绝。
        let mut ambiguous = vec![
            outdated_record("//depot/a.txt"),
            outdated_record("//depot/A.txt"),
        ];
        let requests = select_baseline_requests(&ambiguous, &BaselineScope::Outdated);
        assert_eq!(requests.len(), 2);
        let answer = vec![have_answer("//depot/A.TXT", 1)];
        assert!(apply_refreshed_baselines(&mut ambiguous, &requests, &answer).is_err());
    }

    #[test]
    fn baseline_apply_requires_every_requested_record() {
        let mut records = vec![
            outdated_record("//depot/a.txt"),
            outdated_record("//depot/b.txt"),
        ];
        let requests = select_baseline_requests(&records, &BaselineScope::Outdated);

        // 少一条：不能拿剩下的凑合，缺的那条会带着 head 版本的字段去比对。
        let short = vec![have_answer("//depot/a.txt", 1)];
        assert!(apply_refreshed_baselines(&mut records, &requests, &short).is_err());

        // 多一条：参数与结果对不上。
        let extra = vec![
            have_answer("//depot/a.txt", 1),
            have_answer("//depot/b.txt", 1),
            have_answer("//depot/c.txt", 1),
        ];
        assert!(apply_refreshed_baselines(&mut records, &requests, &extra).is_err());
    }

    #[test]
    fn baseline_apply_rejects_a_malformed_answer() {
        let requests = vec![BaselineRequest {
            index: 0,
            depot_file: "//depot/a.txt".to_owned(),
            revision: 1,
        }];

        for answer in [
            // 非删除版本缺内容字段：比对会用到错误的基线。
            DepotFileRecord {
                digest: None,
                ..have_answer("//depot/a.txt", 1)
            },
            DepotFileRecord {
                file_size: None,
                ..have_answer("//depot/a.txt", 1)
            },
            // 类型与「认不出的类型」都没有：摘要类型无从确定。
            DepotFileRecord {
                head_type: None,
                ..have_answer("//depot/a.txt", 1)
            },
            DepotFileRecord {
                head_action: None,
                ..have_answer("//depot/a.txt", 1)
            },
        ] {
            let mut records = vec![outdated_record("//depot/a.txt")];
            assert!(
                apply_refreshed_baselines(&mut records, &requests, &[answer]).is_err(),
                "an incomplete answer must be rejected"
            );
        }

        // 类型认不出来但有 unsupported_type 是合法的：这类记录会被转交原生 sync。
        let unknown_type = DepotFileRecord {
            head_type: None,
            unsupported_type: Some("tempobj".to_owned()),
            ..have_answer("//depot/a.txt", 1)
        };
        let mut records = vec![outdated_record("//depot/a.txt")];
        assert!(apply_refreshed_baselines(&mut records, &requests, &[unknown_type]).is_ok());

        // have 版本本身是删除版本时没有大小与摘要，那是它真实的形状。
        let deleted = DepotFileRecord {
            head_action: Some(FileAction::Delete),
            head_type: None,
            file_size: None,
            digest: None,
            ..have_answer("//depot/a.txt", 1)
        };
        let mut records = vec![outdated_record("//depot/a.txt")];
        assert!(apply_refreshed_baselines(&mut records, &requests, &[deleted]).is_ok());
    }

    /// 回填按下标定位，而请求是查询**之前**按下标挑出来的。写下标之前必须确认那条记录还是
    /// 当时那一条——重排会让字段落到别的文件上，比回填错版本更隐蔽。
    #[test]
    fn baseline_apply_refuses_when_the_record_moved() {
        let requests = vec![BaselineRequest {
            index: 0,
            depot_file: "//depot/a.txt".to_owned(),
            revision: 1,
        }];
        let answer = vec![have_answer("//depot/a.txt", 1)];

        // 同一份请求对着当时那条记录是好的。
        let mut records = vec![outdated_record("//depot/a.txt")];
        assert!(apply_refreshed_baselines(&mut records, &requests, &answer).is_ok());

        // 下标 0 上换成了另一条记录：不再认。
        let mut moved = vec![outdated_record("//depot/other.txt")];
        assert!(apply_refreshed_baselines(&mut moved, &requests, &answer).is_err());
    }

    /// sync 的补查只为「摘要当真要比对的那批」发查询。这条判据必须是
    /// `analyze_at_target` 里读 type / size / digest 那个分支的**超集**：多查几条只是多花
    /// 一点，少查一条就会让那条记录带着 head 版本的字段去做 have 版本的比对。
    #[test]
    fn at_target_scope_narrows_to_the_records_that_compare_content() {
        let records = vec![
            outdated_record_at("//depot/a.txt", "C:/ws/a.txt"),
            // 已打开：sync 不碰。
            DepotFileRecord {
                action: Some(FileAction::Edit),
                ..outdated_record_at("//depot/b.txt", "C:/ws/b.txt")
            },
            // 排除名单里的。
            outdated_record_at("//depot/skipped/c.txt", "C:/ws/skipped/c.txt"),
            // 目标时刻不在库（目标表里没有）。
            outdated_record_at("//depot/absent.txt", "C:/ws/absent.txt"),
            // 目标时刻只剩删除版本。
            outdated_record_at("//depot/deleted.txt", "C:/ws/deleted.txt"),
            // 目标是归档版本。
            outdated_record_at("//depot/archived.txt", "C:/ws/archived.txt"),
            // 目标版本比 have 新：直接拉，不需要摘要。
            outdated_record_at("//depot/behind.txt", "C:/ws/behind.txt"),
            // have 恰好停在目标版本、目标处活跃：唯一会读内容的形状。
            outdated_record_at("//depot/compare.txt", "C:/ws/compare.txt"),
        ];

        let target = target_map(&[
            ("//depot/a.txt", 1, FileAction::Edit),
            ("//depot/b.txt", 1, FileAction::Edit),
            ("//depot/deleted.txt", 3, FileAction::Delete),
            ("//depot/archived.txt", 1, FileAction::Archive),
            ("//depot/behind.txt", 5, FileAction::Edit),
            ("//depot/compare.txt", 1, FileAction::Edit),
        ]);
        let excludes = ExcludeSet::from_dir_keys(&["C:\\ws\\skipped"]);

        let requests = select_baseline_requests(
            &records,
            &BaselineScope::AtTarget {
                target: &target,
                excludes: &excludes,
            },
        );

        let selected: Vec<&str> = requests
            .iter()
            .map(|request| request.depot_file.as_str())
            .collect();
        assert_eq!(selected, ["//depot/a.txt", "//depot/compare.txt"]);
    }

    #[test]
    fn the_target_query_only_takes_the_revision_and_action() {
        // 目标表一个内容字段都不读：`-Ol` 会让 p4 为整个范围现算大小与摘要。
        assert!(!FSTAT_TARGET_ARGS.contains(&"-Ol"));
        assert_eq!(FSTAT_TARGET_ARGS.last(), Some(&FSTAT_TARGET_FIELDS));
        for field in ["depotFile", "headRev", "headAction"] {
            assert!(FSTAT_TARGET_FIELDS.contains(field), "{field}");
        }
        for field in ["digest", "fileSize", "clientFile"] {
            assert!(!FSTAT_TARGET_FIELDS.contains(field), "{field}");
        }
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
