//! 跨模块共享的测试基建。只在测试构建下编译，不进 release 产物。

use std::env;
use std::path::{Path, PathBuf};

use crate::charset::query_p4_variable;
use crate::model::DepotFileRecord;
use crate::p4::marshal::{TYPE_DICT, TYPE_NULL, TYPE_STRING};
use crate::path::local_path_key;
use crate::prune::P4IGNORE_FILE_NAME;

pub(crate) const CHINESE_NAME_UTF8: &[u8] = b"\xe4\xbd\xbf\xe7\x94\xa8\xe8\xaf\xb4\xe6\x98\x8e.txt";

/// 构造一条 marshal 字符串：`'s'` + 小端长度 + 数据。
pub(crate) fn marshal_string(value: &str) -> Vec<u8> {
    let mut out = vec![TYPE_STRING];
    out.extend_from_slice(&(value.len() as i32).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
    out
}

/// 构造一个 marshal 字典：`'{'` + 键值对 + `'0'`。
pub(crate) fn marshal_dict(fields: &[(&str, &str)]) -> Vec<u8> {
    let mut out = vec![TYPE_DICT];
    for (key, value) in fields {
        out.extend_from_slice(&marshal_string(key));
        out.extend_from_slice(&marshal_string(value));
    }
    out.push(TYPE_NULL);
    out
}

// ---- 预扫描与真实 p4 fixture ----

/// 测试用的临时目录，析构时删除。
pub(crate) struct TempTree {
    pub(crate) root: PathBuf,
}

impl TempTree {
    pub(crate) fn new(name: &str) -> Self {
        let root = env::temp_dir().join(format!("p4delta-test-{}", name));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        TempTree { root }
    }

    pub(crate) fn dir(&self, relative: &str) -> PathBuf {
        let path = self.root.join(relative);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    pub(crate) fn file(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub(crate) fn depot_record(client_file: &str) -> DepotFileRecord {
    DepotFileRecord {
        client_file: client_file.to_owned(),
        client_file_lower: local_path_key(client_file),
        ..Default::default()
    }
}

/// 测试用 p4 子进程的固定环境：只影响子进程，不动这台机器的全局配置，
/// 也不需要 P4 服务器（`ignores` 完全是客户端行为）。
pub(crate) const TEST_P4_ENV: [(&str, &str); 2] = [
    ("P4CONFIG", "p4delta-test-no-config"),
    ("P4IGNORE", P4IGNORE_FILE_NAME),
];

/// 「这台机器上没有 p4」的统一出口。调用方据此打印一行 `skipping:` 后跳过；
/// 设了 `P4_E2E_REQUIRED` 则是失败——与 `tests/support/mod.rs` 的 `sandbox_or_skip`
/// 同一套约定：CI 靠它把「p4 没到位、这些用例整段没跑」变成红色，而不是一场绿色的
/// 空跑。靠日志抓不到这件事：`skipping:` 走 stderr，而测试框架默认捕获用例的输出
/// （libtest 与 nextest 都是），它压根不会出现在 CI 日志里。
fn no_p4() {
    assert!(
        env::var_os("P4_E2E_REQUIRED").is_none(),
        "P4_E2E_REQUIRED is set but p4 is not available"
    );
}

/// 运行 p4 子进程。只有 p4 不存在才返回 None 让调用方跳过；
/// 其他启动错误一律失败，不能把真实的 p4 问题伪装成「环境不支持」。
///
/// 定位走生产代码那一套（`P4_EXE` → `PATH` → P4V 安装目录）：装了 P4V 但 p4 没进
/// `PATH` 的机器上这些用例照样能真跑，而不是无声地跳过。CI 上没有 P4V 安装目录，
/// 由 ci.yml 把 `P4_EXE` 指向 vendor/ 里那一份。
pub(crate) fn p4_output(
    root: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> Option<std::process::Output> {
    let Ok(program) = crate::locate::p4_exe() else {
        no_p4();
        return None;
    };

    match std::process::Command::new(program)
        .args(args)
        .current_dir(root)
        // 与生产代码一致：p4 会用继承来的 PWD 找配置，必须清掉。
        .env_remove("PWD")
        .envs(env.iter().copied())
        .output()
    {
        Ok(output) => Some(output),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            no_p4();
            None
        }
        Err(error) => panic!("p4 {} could not be started: {error}", args.join(" ")),
    }
}

/// p4 是否可用。不可用时测试显式跳过并打印一行说明，而不是静默通过；
/// 设了 `P4_E2E_REQUIRED` 时跳过本身即失败，见 [`no_p4`]。
pub(crate) fn p4_available() -> bool {
    let Ok(program) = crate::locate::p4_exe() else {
        no_p4();
        return false;
    };

    match std::process::Command::new(program).arg("-V").output() {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            no_p4();
            false
        }
        Err(error) => panic!("p4 -V could not be started: {error}"),
    }
}

/// 目标目录里的 `.p4config` 是否真的被 p4 采用（只在设置了 P4CONFIG 的子进程里调用）。
/// p4 的优先级是 .p4config > 环境变量/注册表，所以生效值来自 fixture 才说明配置生效。
pub(crate) fn fixture_config_is_effective(root: &Path) -> bool {
    query_p4_variable(root, "P4IGNORE").as_deref() == Some(P4IGNORE_FILE_NAME)
}

/// 创建指向文件的符号链接；系统不允许时返回错误，由调用方决定是否跳过。
pub(crate) fn symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link)
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (target, link);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlinks are not supported on this platform",
        ))
    }
}

/// 子进程用例的环境变量名：父测试用它把 fixture 目录交给重新执行的自己。
pub(crate) const CHILD_CASE_VAR: &str = "P4DELTA_TEST_CASE";
pub(crate) const CHILD_DIR_VAR: &str = "P4DELTA_TEST_DIR";
/// 子用例的测试函数名，作为子进程的测试过滤器（子串匹配，不绑定模块路径）。
pub(crate) const CHILD_CASE_TEST_NAME: &str = "p4_environment_child_case";
/// fixture 里的配置文件名，只给子进程设置，不动这台机器的全局配置。
pub(crate) const TEST_P4CONFIG_NAME: &str = ".p4config";

/// 用受控环境重新运行本测试二进制里的子用例（`p4_environment_child_case`）。
/// 需要读 `.p4config` 的用例必须这么跑：进程内的环境改不了，
/// 而机器的注册表里未必有 P4CONFIG，只有子进程能拿到确定的环境。
///
/// 过滤器用子串而不是 `--exact` 加完整模块路径：测试换模块后路径会变，
/// 而 `--exact` 匹配不到任何用例时 libtest 依然返回成功，子用例会静默失效。
pub(crate) fn run_child_case(case: &str, root: &Path) {
    let exe = env::current_exe().expect("test binary path");
    let output = std::process::Command::new(exe)
        .args(["--nocapture", CHILD_CASE_TEST_NAME])
        .env_remove("PWD")
        .env_remove("P4IGNORE")
        .env("P4CONFIG", TEST_P4CONFIG_NAME)
        .env(CHILD_CASE_VAR, case)
        .env(CHILD_DIR_VAR, root.display().to_string())
        .output()
        .expect("child test binary must run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "child case {case} failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // 匹配到 0 个用例时 libtest 也是成功退出，必须显式确认子用例真的跑了。
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "child case {case} did not run exactly one test:\n{stdout}"
    );
}
