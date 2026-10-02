//! 命令行参数。

use clap::Parser;

#[derive(Parser, Debug)]
// 版本号取自 Cargo.toml，不再手写副本，见 README 的发布检查清单。
#[command(version)]
pub struct Options {
    /// The workspace to use. If not set, will try to use P4CLIENT. If that is also not set, will try the default one.
    #[arg(short, long)]
    pub(crate) workspace: Option<String>,

    /// The pending changelist to add to. If 0, will add to the default pending changelist.
    #[arg(short, long, default_value = "0")]
    pub(crate) changelist: u32,

    /// Whether we should list file names to stdout. Verbose implies this as well.
    #[arg(short, long)]
    pub(crate) list: bool,

    /// Whether we should output verbose logs to stdout. You should redirect the output to a file if you use this.
    #[arg(short, long)]
    pub(crate) verbose: bool,

    /// If not set, we still do all the work, but don't apply the changes to p4.
    #[arg(short, long)]
    pub(crate) apply: bool,

    /// 反向模式：用 depot 修正工作区，等价于 `p4 clean`（`p4 reconcile -w`）。
    /// 会删除 depot 里没有的文件、丢弃未打开文件的本地改动，且不可撤销。
    #[arg(long)]
    pub(crate) clean: bool,

    /// The files and folders to start from.
    pub(crate) paths: Vec<String>,

    /// 关闭忽略目录剪枝，回退到完整扫描（默认自动判断）。仅在结果异常时用来对比。
    #[arg(long)]
    pub(crate) no_prune_ignored_dirs: bool,

    /// The charset p4 output is decoded with, see `p4 help charset` (for example utf8, cp936,
    /// shiftjis). Defaults to P4CHARSET from the environment or from `p4 set`.
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
