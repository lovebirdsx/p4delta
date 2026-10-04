//! 命令行参数。

use clap::Parser;

#[derive(Parser, Debug)]
// 版本号取自 Cargo.toml，不再手写副本，见 CONTRIBUTING.md 的发布检查清单。
#[command(version)]
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

    /// 同步模式：把工作区拉到目标版本（默认 head），等价于「只传真正需要传的文件」的
    /// `p4 sync -f`。不删 depot 里没有的文件；已打开的文件一律不碰。
    #[arg(long, conflicts_with = "clean")]
    pub(crate) sync: bool,

    /// 同步的目标 changelist（仅 `--sync`）。默认是 head。
    ///
    /// 不接受 0：`-c 0` 在本工具里是「默认 changelist」，照搬成 `--to 0` 太容易，而
    /// `@0` 在 p4 语法里是「第一个修订版之前」——目标时刻什么都不存在，`--sync` 会据此
    /// 删光本地文件。解析层直接当用法错误挡掉。
    #[arg(long, value_name = "CL", requires = "sync", value_parser = clap::value_parser!(u32).range(1..))]
    pub(crate) to: Option<u32>,

    /// 不信任任何推断（仅 `--sync`）：对目标版本没变的文件全部重算摘要，摘要缓存也不看。
    /// 默认档靠 mtime 与缓存跳过它们，快，但「没变化」是上一轮或时间戳说的；这一档把推断
    /// 换成验证，代价是每次运行都要全量读盘。
    #[arg(long, requires = "sync")]
    pub(crate) verify_all: bool,

    /// 范围入口：目录（整棵子树）或文件（单文件；本地不存在也可以，用于 open for
    /// delete）。`-` 前缀表示排除，一条里可用 `;` 分隔多个。结果与工作区的
    /// `.p4delta-scope` 配置取交集——配置在上一层目录时也能找到；不给路径时
    /// 直接用配置的范围。
    pub(crate) paths: Vec<String>,

    /// 关闭忽略目录剪枝，回退到完整扫描（默认自动判断）。仅在结果异常时用来对比。
    #[arg(long)]
    pub(crate) no_prune_ignored_dirs: bool,

    /// 解码 p4 输出所用的字符集，见 `p4 help charset`（例如 utf8、cp936、shiftjis）。
    /// 默认取环境变量或 `p4 set` 里的 P4CHARSET。
    #[arg(long)]
    pub(crate) charset: Option<String>,
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
            "-w",
            "ws",
            "--to",
            "12345",
            "--verify-all",
        ]);
        assert!(parsed.sync);
        assert_eq!(parsed.to, Some(12345));
        assert!(parsed.verify_all);
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
}
