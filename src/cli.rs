//! 命令行参数。

use clap::Parser;

#[derive(Parser, Debug)]
// 版本号取自 Cargo.toml，不再手写副本，见 docs/dev/release.md 的发布检查清单。
// 简介同理：不带值的 about 让 clap 去取 CARGO_PKG_DESCRIPTION，代码里不写第二份字面量。
#[command(version, about)]
pub struct Options {
    /// 要使用的 workspace（p4 client 名）。没设时读环境变量 P4CLIENT；两个都没有则报错退出。
    #[arg(short, long)]
    pub(crate) workspace: Option<String>,

    /// 变更要加入的 pending changelist。为 0 时加入默认的那一个。
    #[arg(short, long, default_value = "0")]
    pub(crate) changelist: u32,

    /// 是否把文件名清单打到 stdout。`--verbose` 也隐含打开它。
    #[arg(short, long)]
    pub(crate) list: bool,

    /// 是否输出详细日志到 stdout。用这个开关时建议把输出重定向到文件。
    #[arg(short, long)]
    pub(crate) verbose: bool,

    /// 不加这个开关时照样做完全部工作，只是不把变更应用到 p4。
    #[arg(short, long)]
    pub(crate) apply: bool,

    /// 反向模式：用 depot 修正工作区，等价于 `p4 clean`（`p4 reconcile -w`）。
    /// 会删除 depot 里没有的文件、丢弃未打开文件的本地改动，且不可撤销。
    #[arg(long)]
    pub(crate) clean: bool,

    /// 同步模式：把工作区拉到目标版本（默认 head，可用 `--to` 指定 changelist），
    /// 由原生 p4 判定覆盖保护、opened/resolve 与 have 更新——已打开的文件交给 p4 处理，
    /// 可写保护也照 p4 的规则走。不删 depot 里没有的文件。
    ///
    /// 要「无论本地改没改，都把工作区修成目标版本」，加上 `--force`。
    #[arg(long, conflicts_with = "clean")]
    pub(crate) sync: bool,

    /// 强制修复（仅 `--sync`）：等价于「只传真正需要传的文件」的 `p4 sync -f`。
    /// 会恢复缺失文件、丢弃未打开文件的本地改动，且不可撤销；已打开的文件一律不碰。
    /// 这是 `--sync` 在加上本开关之前的旧行为。
    #[arg(long, requires = "sync")]
    pub(crate) force: bool,

    /// 同步的目标 changelist（`--sync`，普通与 `--force` 都支持）。默认是 head。
    ///
    /// 不接受 0：`-c 0` 在本工具里是「默认 changelist」，照搬成 `--to 0` 太容易，而
    /// `@0` 在 p4 语法里是「第一个修订版之前」——目标时刻什么都不存在，同步会据此
    /// 删光本地文件。解析层直接当用法错误挡掉。
    #[arg(long, value_name = "CL", requires = "sync", value_parser = clap::value_parser!(u32).range(1..))]
    pub(crate) to: Option<u32>,

    /// 不信任任何推断（仅 `--sync --force`）：对目标版本没变的文件全部重算摘要，
    /// 摘要缓存也不看。默认档靠 mtime 与缓存跳过它们，快，但「没变化」是上一轮或
    /// 时间戳说的；这一档把推断换成验证，代价是每次运行都要全量读盘。
    ///
    /// 普通同步（不带 `--force`）不读摘要，这个开关没有可放大的东西。
    #[arg(long, requires = "force")]
    pub(crate) verify_all: bool,

    /// 范围目标：每个参数就是**一条**目标，不按 `;` 拆分、不认 `-` 前缀排除（排除项走
    /// `--exclude-dir` / `--exclude-file`）。目录用显式递归后缀 `<dir>/...`，文件用精确路径；
    /// 本地存在的路径按本地类型判。相对路径以启动目录为基准，结果与
    /// **client root 下的** `.p4delta-scope` 取交集。
    pub(crate) paths: Vec<String>,

    /// 关闭忽略目录剪枝，回退到完整扫描（默认自动判断）。仅在结果异常时用来对比。
    #[arg(long)]
    pub(crate) no_prune_ignored_dirs: bool,

    /// 解码 p4 输出所用的字符集，见 `p4 help charset`（例如 utf8、cp936、shiftjis）。
    /// 默认取环境变量或 `p4 set` 里的 P4CHARSET。
    #[arg(long)]
    pub(crate) charset: Option<String>,

    /// 机器可读输出：stdout 只出 JSON Lines，人类可读的报告整体改道 stderr。
    /// 契约（记录形状与硬条款）见 `docs/json-contract.md`。
    #[arg(long)]
    pub(crate) json: bool,

    /// client 根目录。**必须是该 client 的固定 Root**（client spec 里那一个；对不上就
    /// 报错，不是换一份范围配置的入口——`p4 info` 的 `clientRoot` 随 cwd 变，不作数）。
    /// 它同时是 `.p4delta-scope` 的归属、相对路径的基准，以及拼 client 语法（记录里的
    /// `clientFile`）用的根。
    #[arg(long, value_name = "PATH")]
    pub(crate) client_root: Option<String>,

    /// 忽略 client root 下的 `.p4delta-scope`，范围只认命令行给的目标与排除项（普通同步的
    /// 范围本来就是 client view，不受这个开关影响）。给编辑器用的——它按自己的模型给出范围，
    /// 不该在别人的配置上再叠一层。
    #[arg(long)]
    pub(crate) no_scope_file: bool,

    /// 排除一个目录子树（可重复）。相对路径以 client root 为基准，编辑器传绝对路径；
    /// 类型由这个参数自己声明，不去 stat 猜——「本地还不存在的输出目录」正是常见的一条。
    /// 普通同步（`--sync` 不带 `--force`）不接受它：那条路的范围是 client view，一次性的
    /// 排除会换掉要问原生的问题；要持久边界就写进范围配置。
    #[arg(long, value_name = "PATH")]
    pub(crate) exclude_dir: Vec<String>,

    /// 排除一个文件（可重复，精确匹配，不递归）。同 `--exclude-dir`，路径基准是 client root。
    #[arg(long, value_name = "PATH")]
    pub(crate) exclude_file: Vec<String>,

    /// 让 open 模式的三组 revert 让位给 `p4 reconcile -a -e -d` 的语义：`revert_add` /
    /// `revert_edit` 消失，`revert_delete` 并进 `reopen_edit`（原生 `-e` 对「文件还在、
    /// 却被 open for delete」一律改开成 edit，与内容无关）。加上这个开关，输出逐行等于原生
    /// reconcile。
    #[arg(long)]
    pub(crate) no_revert_groups: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    use clap::Parser;

    /// `--clean` 故意不给短名：`-w` 是 `--workspace` 的，P4V 集成配置里写死了
    /// `-w $c -l %D`，把 `-w` 挪走会静默改掉那些配置的含义。
    #[test]
    fn clean_flag_is_long_only() {
        let with_flag = Options::parse_from(["p4delta", "--clean", "-w", "ws"]);
        assert!(with_flag.clean);

        let without_flag = Options::parse_from(["p4delta", "-w", "ws"]);
        assert!(!without_flag.clean);
    }

    /// clean 与 changelist 同时给出时，解析层要放行（告警留给 lib.rs 打），
    /// 否则 `-c` 的 default_value 会让 conflicts_with 无条件触发。
    #[test]
    fn clean_tolerates_a_changelist() {
        let parsed = Options::try_parse_from(["p4delta", "--clean", "-w", "ws", "-c", "5"])
            .expect("clean 与 -c 同时给出不该是用法错误");
        assert!(parsed.clean);
        assert_eq!(parsed.changelist, 5);
    }

    #[test]
    fn sync_flags_are_wired_up() {
        let parsed = Options::parse_from([
            "p4delta",
            "--sync",
            "--force",
            "-w",
            "ws",
            "--to",
            "12345",
            "--verify-all",
        ]);
        assert!(parsed.sync);
        assert!(parsed.force);
        assert_eq!(parsed.to, Some(12345));
        assert!(parsed.verify_all);

        // 普通同步：不带 --force，其余开关照旧可用。
        let plain = Options::parse_from(["p4delta", "--sync", "-w", "ws", "--to", "7"]);
        assert!(plain.sync);
        assert!(!plain.force);
        assert!(!plain.verify_all);
    }

    /// `--force` 只对 sync 有意义：没有 `--sync` 时它是用法错误，而不是被静默忽略。
    /// 加上它等于「丢弃未打开文件的本地改动」，放行一个拼错模式的调用代价太大。
    #[test]
    fn force_requires_sync() {
        let error = Options::try_parse_from(["p4delta", "--force", "-w", "ws"])
            .expect_err("没有 --sync 时 --force 必须是用法错误");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );

        // `--clean --force` 是 clap 放行的另一种形状：它补上的 `--sync` 与本已给出的
        // `--clean` 互斥，于是 `sync` 最终并没有出现——clean 照常走，force 一点作用都不起。
        // 立场同 `no_revert_groups_is_tolerated_in_the_other_modes`：模式里无意义的东西
        // 放行。真正的护栏在里面：只有 `--sync --force` 才会分流到强制修复。
        let tolerated = Options::try_parse_from(["p4delta", "--clean", "--force", "-w", "ws"])
            .expect("--clean 下给 --force 不该是用法错误");
        assert!(tolerated.clean);
        assert!(!tolerated.sync, "conflicts_with 已经把 --sync 挡在外面");
    }

    /// `--verify-all` 放大的只有强制路径的摘要候选。普通同步根本不读摘要，放行它等于
    /// 让用户以为「验证过全部文件」，而那一档没有这个含义。
    #[test]
    fn verify_all_requires_force() {
        let error = Options::try_parse_from(["p4delta", "--sync", "--verify-all", "-w", "ws"])
            .expect_err("--verify-all 必须要求 --force");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );

        // 普通同步仍然合法。
        assert!(Options::try_parse_from(["p4delta", "--sync", "-w", "ws"]).is_ok());
    }

    /// `--to 0` 在 p4 语法里是「第一个修订版之前」：目标时刻什么都不存在，sync 会据此
    /// 删光本地文件。而 `-c 0` 在本工具里表示「默认 changelist」，照搬成 `--to 0` 太容易，
    /// 所以要在解析层挡住，不能让它走到分析阶段。
    #[test]
    fn a_zero_target_changelist_is_a_usage_error() {
        let error = Options::try_parse_from(["p4delta", "--sync", "-w", "ws", "--to", "0"])
            .expect_err("--to 0 必须是用法错误");

        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn no_prune_flag_is_wired_up() {
        let with_flag = Options::parse_from(["p4delta", "-w", "ws", "--no-prune-ignored-dirs"]);
        assert!(with_flag.no_prune_ignored_dirs);

        let without_flag = Options::parse_from(["p4delta", "-w", "ws"]);
        assert!(!without_flag.no_prune_ignored_dirs);
    }

    /// 编辑器的开关：布尔、取值的路径，以及两个**可重复**的排除项。可重复是关键——
    /// 一条参数一条路径，不是把多条拼回 `;` 那套文本语法。取值那个要能缺省
    /// （缺省时自己去问 `p4 info`），不能因为没给就变成用法错误。
    #[test]
    fn the_programmatic_flags_are_wired_up() {
        let parsed = Options::parse_from([
            "p4delta",
            "-w",
            "ws",
            "--json",
            "--client-root",
            "/ws",
            "--no-scope-file",
            "--no-revert-groups",
            "--exclude-dir",
            "/ws/gen",
            "--exclude-dir",
            "/ws/build",
            "--exclude-file",
            "/ws/local.txt",
        ]);
        assert!(parsed.json);
        assert_eq!(parsed.client_root.as_deref(), Some("/ws"));
        assert!(parsed.no_scope_file);
        assert!(parsed.no_revert_groups);
        assert_eq!(parsed.exclude_dir, ["/ws/gen", "/ws/build"]);
        assert_eq!(parsed.exclude_file, ["/ws/local.txt"]);

        let bare = Options::parse_from(["p4delta", "-w", "ws"]);
        assert!(!bare.json);
        assert_eq!(bare.client_root, None);
        assert!(!bare.no_scope_file);
        assert!(!bare.no_revert_groups);
        assert!(bare.exclude_dir.is_empty());
        assert!(bare.exclude_file.is_empty());
    }

    /// 被撤掉的那套「机器可读范围」开关不该悄悄回来：它们现在是未知选项（用法错误），
    /// 而不是被容忍的空操作。
    #[test]
    fn the_removed_scope_flags_are_usage_errors() {
        for flag in [
            "--scope-report",
            "--scope-from",
            "--scope-request",
            "--scope-snapshot",
        ] {
            let error = Options::try_parse_from(["p4delta", "-w", "ws", flag, "x"])
                .expect_err("已删除的开关必须是用法错误");
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::UnknownArgument,
                "{flag}"
            );
        }
    }

    /// `--no-revert-groups` 是 open 模式的措辞。clean / sync 下没有那三组，但也不该是
    /// 用法错误——编辑器对三个模式共用同一份参数拼装（照 `clean_tolerates_a_changelist`
    /// 的立场：模式里无意义的东西放行，有意义的东西才拦）。
    #[test]
    fn no_revert_groups_is_tolerated_in_the_other_modes() {
        for mode in ["--clean", "--sync"] {
            let parsed =
                Options::try_parse_from(["p4delta", mode, "-w", "ws", "--no-revert-groups"])
                    .unwrap_or_else(|error| {
                        panic!("{mode} 下给 --no-revert-groups 不该是用法错误：{error}")
                    });
            assert!(parsed.no_revert_groups);
        }
    }

    /// `p4delta.exe.manifest` 的程序集版本是仓库里唯一没法自动生成的一处版本号
    /// （VERSIONINFO 走 build.rs 注入，XML 属性不行）。这条用例把它变成门禁：
    /// 漏改即失败，而不是等到有人翻开 exe 属性页才发现那是旧版本。
    #[test]
    fn manifest_assembly_version_matches_the_crate_version() {
        let manifest = include_str!("../p4delta.exe.manifest");
        let expected = format!("version=\"{}.0\"", env!("CARGO_PKG_VERSION"));

        assert!(
            manifest.contains(&expected),
            "p4delta.exe.manifest 里需要 {expected}"
        );
    }

    /// `p4delta.rc` 里这几个名字是任务管理器「名称」列和 exe 属性页显示的东西，不该各写各的。
    /// 这条用例把它们钉在包名上：改名时漏改 `.rc` 会当场失败，而不是等到翻开属性页才发现
    /// 进程列表里顶着上一代的定位句。
    #[test]
    fn version_resource_names_match_the_crate() {
        let rc = include_str!("../p4delta.rc");
        let name = env!("CARGO_PKG_NAME");

        for (field, expected) in [
            ("FileDescription", name.to_owned()),
            ("InternalName", name.to_owned()),
            ("ProductName", name.to_owned()),
            ("OriginalFilename", format!("{name}.exe")),
        ] {
            // 模式里必须是字面量的 `\0`（两个字符）：.rc 的字符串值以它结尾，rc.exe 不替你补。
            let expected = format!(r#"VALUE "{field}", "{expected}\0""#);
            assert!(rc.contains(&expected), "p4delta.rc 里需要 {expected}");
        }
    }
}
