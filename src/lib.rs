//! p4delta：Perforce Helix Core 工作区工具。
//!
//! reconcile 模式等价 P4V 的 `Reconcile Offline Work`（快 10–100 倍），`--clean` 等价
//! `p4 clean`，`--sync` 是普通同步（判定权交给原生 p4），`--sync --force` 才是
//! 「只传真正需要传的文件」的 `p4 sync -f`；后三者共用同一条扫描与摘要缓存管线，
//! 普通同步不在这条管线上——它既不扫工作区也不读摘要缓存。

use std::env;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::time::Instant;

use anyhow::{Result, bail};
use async_global_executor as task;
use directories::ProjectDirs;

pub use cli::Options;

mod cache;
mod charset;
mod cli;
mod digest;
mod json;
mod locate;
mod model;
mod normal_sync;
mod p4;
mod path;
mod prune;
mod reconcile;
mod scope;
mod scope_config;
mod workspace;

#[cfg(test)]
mod test_util;

use crate::cache::{CacheWriter, save_cache};
use crate::charset::init_p4_encoding;
use crate::json::{Mode, sayln};
use crate::locate::check_p4_exe_env;
use crate::model::WorkspaceCache;
use crate::normal_sync::run_normal_sync;
use crate::reconcile::reconcile_scope;
use crate::scope::{Scope, ScopePolicy, combine_scope};

/// 读写文件的缓冲区大小：缓存读写、摘要计算、p4 输出流三处共用。
///
/// 128 KiB 是经验值——原注释说它「在 PCIe 4.0 SSD 上实测最优」，那个结论只在一块盘上量过，
/// 没有可复现的基准，别把它当调优结论看。
pub(crate) const READ_BUFFER_SIZE: usize = 128 * 1024;

/// 按给定的参数跑一次 reconcile。
///
/// 参数由调用方（二进制入口）解析后传入，这样库本身不依赖进程级的参数解析。
///
/// 入口与 `run_once` 之间夹着 JSON 模式的 summary：**任何**返回路径（成功、`bail!`、
/// 早退）都要发一条 `kind:"summary"`，消费方靠它在「跑完了」与「跑挂了」之间划线。
/// 写成包一层而不是散在各个 `return` 上，是因为后者迟早会漏一处，而漏掉的那处会被读成
/// 「没有结论」——那还算好的；真正糟的是漏在成功路径上，整轮工作白做。
pub fn run(mut options: Options) -> Result<()> {
    json::set_json_mode(options.json);
    json::set_force(options.sync && options.force);
    let start_time = Instant::now();

    let result = run_once(&mut options);
    let mode = mode_of(&options);

    if let Err(error) = &result {
        // 人读的版本仍走 stderr（`main` 那边照着既有格式再打一遍），记录里那一份是给
        // 消费方做诊断用的：它未必去翻 stderr。
        json::emit_error(None, &format!("{error:#}"));
    }
    json::emit_summary(
        mode,
        result.is_ok(),
        options.apply,
        start_time.elapsed().as_millis(),
    );
    // 退化的 clientFile 在记录流里看不出来（就是一个不以 `//` 开头的值），整轮结束时
    // 汇总一句，免得消费方拿到一份「两种拼法混排」的记录流却毫无察觉。
    json::report_degraded_client_files();

    result
}

/// 本轮是哪个模式。三选一，`--clean` 与 `--sync` 互斥由 clap 保证。
fn mode_of(options: &Options) -> Mode {
    if options.clean {
        Mode::Clean
    } else if options.sync {
        Mode::Sync
    } else {
        Mode::Open
    }
}

/// `--exclude-*` 是**本次操作**的降噪，普通同步没有它的位置。
///
/// 普通同步的范围就是 client view，每个动作由原生判定：把一次性的排除塞进去，δ 问的问题
/// 与原生回答的问题就不是同一个了，而报告出来的是「原生在更小范围上会做什么」。持久边界
/// 应该写进 client root 下的 `.p4delta-scope`（那份排除对普通同步同样生效），要一次性降噪
/// 就走 `--clean` 或 `--sync --force`——那两条路本就按本地算出来的范围下发。
fn reject_noise_arguments_in_a_normal_sync(options: &Options) -> Result<()> {
    if !options.sync || options.force {
        return Ok(());
    }
    if options.exclude_dir.is_empty() && options.exclude_file.is_empty() {
        return Ok(());
    }

    bail!(
        "--exclude-dir / --exclude-file conflict with a normal sync (--sync without --force). \
         Its range is the client view and p4 decides every action, so a per-operation exclusion \
         would replace the question native answers with a narrower one. Put a lasting boundary \
         in the client root's .p4delta-scope (its exclusions do apply here), or use --clean / \
         --sync --force, which work from a locally computed range."
    )
}

fn run_once(options: &mut Options) -> Result<()> {
    let start_time = Instant::now();

    // 配置错误要在任何输出之前报出来。只校验 P4_EXE 设了的那种情况：
    // 「这台机器没有 p4」由各个调用点的 strict / lenient 策略分别处理。
    check_p4_exe_env()?;
    reject_noise_arguments_in_a_normal_sync(options)?;

    // 命令行一侧的起点只有一个：**固定的启动 cwd**。字符集探测、连接解析与 depot 路径翻译
    // 都在它下面，不再借第一个目标改变——「同一批参数换个顺序」不该换来另一个连接。
    let startup_dir = crate::scope::startup_dir();

    // 在任何人读 p4 输出之前，先把它的输出字符集定下来。从启动目录里探测，是为了让那里的
    // `.p4config` 里定义的 P4CHARSET 生效。
    init_p4_encoding(options.charset.as_deref(), Path::new(&startup_dir));

    if options.workspace.is_none() {
        sayln!("No workspace passed, trying P4CLIENT.");
        options.workspace = env::var("P4CLIENT").ok();
    }

    let workspace_name = match options.workspace.clone() {
        None => bail!("No workspace found, use -w or set P4CLIENT."),
        Some(name) => {
            sayln!("Using workspace \"{}\".", name);
            name
        }
    };

    // client root 是本轮的归属：`.p4delta-scope` 从它下面读、相对路径以它为基准、p4 子进程
    // 也以它为 cwd。它必须来自 p4 自己的账，`--client-root` 只是一个需要被核对的声明；
    // 对不上就失败关闭，而不是换一份范围配置继续跑。
    let client_root = task::block_on(crate::scope::resolve_client_root(options, &workspace_name))?;
    json::set_client_spec(&client_root, &workspace_name);

    // `.p4delta-scope`：只有 ENOENT 才是「没有配置」。读解析失败一律在这里报出来，
    // 早于任何 p4 写入。
    let scope_file = crate::scope::read_scope_file(&client_root, options.no_scope_file)?;

    // 求值本轮范围：配置与命令行目标取交集、去重；两边都没有就是错误。
    //
    // 普通同步用严格求值：范围是它交给 p4 的问题边界，少一条目标等于换了个问题去问，
    // 而答案会被当成完整结论报出去（见 [`ScopePolicy`]）。「入口匹配了几个」这个结论
    // 普通同步拿不到，要在**求值之前**就声明：求值本身失败时 summary 照样要发，
    // 而那时它已经没机会声明了（见 `json::set_scope_matched_unknown`）。
    let strict_scope = options.sync && !options.force;
    if strict_scope {
        json::set_scope_matched_unknown();
    }
    let policy = if strict_scope {
        ScopePolicy::Strict
    } else {
        ScopePolicy::Lenient
    };

    let targets = task::block_on(crate::scope::resolve_targets(options, policy))?;
    let cli_excludes = crate::scope::resolve_cli_excludes(options, &client_root);
    let (includes, excludes) = combine_scope(
        &client_root,
        scope_file.as_ref(),
        &targets.entries,
        targets.declared,
        &cli_excludes,
    )?;

    if excludes.declared_len() > 0 {
        sayln!(
            "Scope has {} include entr{} and {} exclusion(s).",
            includes.len(),
            if includes.len() == 1 { "y" } else { "ies" },
            excludes.declared_len()
        );
    }

    let scope = Scope {
        includes,
        excludes,
        work_dir: crate::scope::work_dir_for(&client_root),
    };

    json::emit_progress("start", None);

    if options.clean {
        sayln!("Clean mode: updating the workspace to match the depot.");
    }
    if options.sync {
        // 说清目标是哪个版本，以及这一轮是「普通同步」还是「强制修复」：后者会丢弃本地
        // 改动，两者共用 `--sync`，光看模式名分不出来。
        let mode = if options.force {
            "Sync (force) mode: repairing"
        } else {
            "Sync mode: updating"
        };
        match options.to {
            Some(changelist) => sayln!("{mode} the workspace to changelist {changelist}."),
            None => sayln!("{mode} the workspace to the head revision."),
        }
    }

    match options.changelist {
        0 => sayln!("Using default pending changelist."),
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
        n => sayln!("Using pending changelist {}.", n),
    }

    if options.verbose {
        options.list = true;
    }

    // 普通同步在这里分叉，**在加载摘要缓存之前**：它不扫工作区、不算摘要、不查 have
    // 时间戳，也就没有任何理由去碰那块缓存（不读、不写、不存在时更不该凭空创建）。
    // 强制修复（`--sync --force`）与 open / clean 照旧走下面整条 reconcile 管线。
    if options.sync && !options.force {
        task::block_on(run_normal_sync(options, &scope))?;
        json::emit_progress("done", None);
        sayln!(
            "Operation completed in {} seconds.",
            start_time.elapsed().as_secs_f32()
        );
        return Ok(());
    }

    let mut cache: WorkspaceCache = Default::default();

    let cache_path = ProjectDirs::from("com", "", "p4delta").map(|proj_dirs| {
        proj_dirs
            .cache_dir()
            .join("digests_".to_owned() + &workspace_name + ".bin")
    });

    if let Some(cache_path) = &cache_path
        && cache_path.exists()
    {
        sayln!("Loading cache from {}.", cache_path.display());
        // 流式加载：大工作区的缓存有几百 MB，整份读进内存会让加载峰值翻倍。
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
                sayln!("    Loaded {} cached digests.", cache.file_map.len());
            }
            Err(e) => {
                eprintln!(
                    "Warning: Failed to load cache ({}), will rebuild from scratch",
                    e
                );
            }
        }
    }

    // 摘要阶段成功后按阈值保存，保留此前阶段的成果；不是阶段计算中的周期 checkpoint。
    let mut cache_writer = cache_path.map(CacheWriter::new);

    // 一次操作、一个视图：所有入口合并为一轮（exclude 的两侧过滤在 reconcile_scope 内完成）。
    task::block_on(reconcile_scope(
        options,
        &scope,
        &mut cache,
        &mut cache_writer,
    ))?;

    save_cache(&mut cache_writer, &mut cache, true)?;

    json::emit_progress("done", None);
    sayln!(
        "Operation completed in {} seconds.",
        start_time.elapsed().as_secs_f32()
    );
    Ok(())
}
