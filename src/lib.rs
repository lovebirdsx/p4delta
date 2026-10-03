//! p4delta：P4V 的 Reconcile Offline Work 的快速替代实现。
//!
//! 从服务器取 depot 状态、扫描本地工作区、计算并缓存文件摘要，再比对两者，
//! 更新指定的待提交 changelist。

use std::env;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use async_global_executor as task;
use directories::ProjectDirs;

pub use cli::Options;

mod cache;
mod charset;
mod cli;
mod digest;
mod locate;
mod model;
mod p4;
mod path;
mod prune;
mod reconcile;
mod workspace;

#[cfg(test)]
mod test_util;

use crate::cache::{CacheWriter, save_cache};
use crate::charset::init_p4_encoding;
use crate::locate::check_p4_exe_env;
use crate::model::WorkspaceCache;
use crate::p4::process::{FailureMode, run_p4_command_slice};
use crate::path::{absolute_local_path, normalize_local_path_owned, strip_depot_wildcard_suffix};
use crate::reconcile::reconcile_dir;

// Seemingly optimal buffer size for reading large data on a PCIe 4.0 SSD.
// Need non-blocking queued IO for small files, but is not available in rust.
pub(crate) const READ_BUFFER_SIZE: usize = 128 * 1024;

/// 按给定的参数跑一次 reconcile。
///
/// 参数由调用方（二进制入口）解析后传入，这样库本身不依赖进程级的参数解析。
pub fn run(mut options: Options) -> Result<()> {
    let start_time = Instant::now();

    // 配置错误要在任何输出之前报出来。只校验 P4_EXE 设了的那种情况：
    // 「这台机器没有 p4」由各个调用点的 strict / lenient 策略分别处理。
    check_p4_exe_env()?;

    // Resolve the charset p4 writes its output in before anything reads that output.
    // Probing from inside the workspace lets a P4CHARSET defined in its .p4config take effect.
    let probe_dir = match options.paths.first().map(PathBuf::from) {
        Some(path) if path.is_dir() => path,
        Some(path) => path.parent().map(Path::to_path_buf).unwrap_or(path),
        None => env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    init_p4_encoding(options.charset.as_deref(), &probe_dir);

    // Workspace input
    if options.workspace.is_none() {
        println!("No workspace passed, trying P4CLIENT.");
        options.workspace = env::var("P4CLIENT").ok();
    }

    let workspace_name = match &options.workspace {
        None => bail!("No workspace found, use -w or set P4CLIENT."),
        Some(name) => {
            println!("Using workspace \"{}\".", name);
            name.as_str()
        }
    };

    // 没有路径就无事可做，而「什么事都没做」不该报告成功——P4V 那边的 prompt 留空正好落到
    // 这里（`-l $D` 展开成空）。空串也算没给（`p4delta -l ""`）：`is_empty()` 时 `all()` 为真，
    // 一个判断同时覆盖「没给」与「给了空串」。
    if options.paths.iter().all(|path| path.trim().is_empty()) {
        bail!("No path given; pass the folder to work on.");
    }

    if options.clean {
        println!("Clean mode: updating the workspace to match the depot.");
    }
    if options.sync {
        // 说清目标是哪个版本：head 与指定 changelist 是两种不同的结果，
        // 而用户未必记得自己没写 --to。
        match options.to {
            Some(changelist) => {
                println!("Sync mode: updating the workspace to changelist {changelist}.")
            }
            None => println!("Sync mode: updating the workspace to the head revision."),
        }
    }

    // Changelist input
    match options.changelist {
        0 => println!("Using default pending changelist."),
        // clean 不打开任何文件，也就没有 changelist 可进；`p4 clean` 本身也不接受 -c。
        // 静默忽略会让人以为改动进了指定的 pending changelist，所以要说一声。
        n if options.clean => {
            eprintln!("Warning: --changelist {n} is ignored, clean never opens files.")
        }
        // sync 同理，但这里多一句：想指定目标版本的用法是 `--to`，
        // 而那正是 `-c` 在别的模式下长得最像的东西。
        n if options.sync => eprintln!(
            "Warning: --changelist {n} is ignored, sync never opens files (use --to to pick a target changelist)."
        ),
        n => println!("Using pending changelist {}.", n),
    }

    // Verbose input
    if options.verbose {
        options.list = true;
    }

    let mut cache: WorkspaceCache = Default::default();

    let cache_path = ProjectDirs::from("com", "", "p4delta").map(|proj_dirs| {
        proj_dirs
            .cache_dir()
            .join("digests_".to_owned() + workspace_name + ".bin")
    });

    // Load digest cache
    if let Some(cache_path) = &cache_path
        && cache_path.exists()
    {
        println!("Loading cache from {}.", cache_path.display());
        // Stream the cache in: it reaches hundreds of MB on a large workspace, and reading
        // it whole would double the memory needed to load it.
        match (|| -> Result<WorkspaceCache> {
            let config = bincode::config::standard();
            let mut cache_file =
                BufReader::with_capacity(READ_BUFFER_SIZE, File::open(cache_path)?);
            let decoded: WorkspaceCache = bincode::decode_from_std_read(&mut cache_file, config)?;
            Ok(decoded)
        })() {
            Ok(decoded) => {
                cache = decoded;
                cache.out_of_date = false;
                println!("    Loaded {} cached digests.", cache.file_map.len());
            }
            Err(e) => {
                eprintln!(
                    "Warning: Failed to load cache ({}), will rebuild from scratch",
                    e
                );
                // Cache remains empty, will be rebuilt
            }
        }
    }

    // 摘要阶段成功后按阈值保存，保留此前阶段的成果；不是阶段计算中的周期 checkpoint。
    let mut cache_writer = cache_path.map(CacheWriter::new);

    // 逐个处理输入里的路径，串行执行，输出才好读。不可用的路径先记下原因：一个都用不上时
    // 整轮失败（见循环之后），只是其中一部分时最后汇总成一行告警。
    let mut usable = 0usize;
    let mut unusable: Vec<String> = Vec::new();

    for original_path in &options.paths {
        let mut path: String = original_path.to_owned();

        // Convert depot paths to workspace paths
        if original_path.starts_with("//") {
            let args = ["-Mj", "-Ztag", "where"];
            let paths = [original_path.to_owned()];
            let current_dir = env::current_dir()
                .map_err(|e| anyhow!("Failed to get current directory: {}", e))?;
            let result = task::block_on(run_p4_command_slice(
                &options,
                &current_dir.to_string_lossy(),
                &args,
                &paths,
                false,
                FailureMode::Warn,
            ))?;
            for record_result in result
                .into_iter()
                .map(|line| serde_json::from_str::<serde_json::Value>(&line))
            {
                let record = record_result?;
                if record["depotFile"].as_str() == Some(original_path) {
                    path = record["path"]
                        .as_str()
                        .ok_or_else(|| {
                            anyhow!(
                                "Missing 'path' field in p4 where response for {}",
                                original_path
                            )
                        })?
                        .to_owned();
                    break;
                }
            }
        }
        // 统一本地路径：P4V 等工具会传来正斜杠，本地键必须与 p4 返回的 clientFile 一致。
        path = normalize_local_path_owned(path);

        // Correct the path since P4V tends to give us a bad one
        path = strip_depot_wildcard_suffix(&path).to_owned();

        // 相对路径转绝对：扫描结果的路径前缀必须与 p4 返回的 clientFile 相同。
        path = absolute_local_path(&path);

        if let Some(first_letter) = path.get_mut(0..1) {
            first_letter.make_ascii_uppercase();
        }
        // Use the path
        let check_path = PathBuf::from(&path);
        if check_path.exists() {
            if check_path.is_dir() {
                task::block_on(reconcile_dir(
                    &options,
                    &path,
                    &mut cache,
                    &mut cache_writer,
                ))?;
                usable += 1;
            } else {
                unusable.push(format!("\"{original_path}\" is not a directory"));
            }
        } else {
            unusable.push(format!("\"{original_path}\" does not exist"));
        }
    }

    // 一个用得上的路径都没有，等于什么都没做，而「什么都没做」不该报成功。原因一律用
    // `original_path`：`path` 在上面被归一化、剥通配后缀、绝对化、首字母大写，depot 路径
    // 翻译失败时还会变成 `\\depot\...` 的样子，拿它报错用户对不上自己输入的东西。
    if usable == 0 {
        bail!(
            "Nothing to work on; p4delta works on folders:\n  {}",
            unusable.join("\n  ")
        );
    }
    if !unusable.is_empty() {
        // 告警走 stderr：`-l` 的清单在 stdout 上，别混在一起。
        eprintln!(
            "Warning: skipped {} path(s) that cannot be worked on:",
            unusable.len()
        );
        for reason in &unusable {
            eprintln!("  {reason}");
        }
    }

    // Save digest cache
    save_cache(&mut cache_writer, &mut cache, true)?;

    // We are done!
    println!(
        "Operation completed in {} seconds.",
        start_time.elapsed().as_secs_f32()
    );
    Ok(())
}
