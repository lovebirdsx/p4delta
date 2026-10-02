//! e2e 沙箱：每个用例一个独立的 p4d 实例。
//!
//! 框架照搬 `test-p4` 的机制，用 Rust 重写：
//!
//! - **模板复制**：种子只跑一次（[`template::ensure_template`]），之后每个实例
//!   从模板复制数据库，省掉十几次 p4 往返。
//! - **环境隔离**：被测程序拿到的环境里没有任何继承来的 `P4*` 变量
//!   （[`Sandbox::env`]），p4 到底连哪台服务器没有第二种解释。
//! - **独立端口**：让 OS 分配空闲端口，所以同机并行的实例互不干扰。
//! - **探测跳过**：找不到 p4d/p4 时 [`sandbox_or_skip`] 返回 `None`，
//!   与 `src/test_util.rs` 的 `p4_available()` 同一套约定——打印一行说明后跳过，
//!   而不是静默通过。
//!
//! 每个 `tests/*.rs` 是独立的 crate，各自会引入本模块一次，
//! 所以未用到的 helper 会触发 dead_code——CI 的 clippy 是 `-D warnings`，
//! 这个 allow 是必需的，不是偷懒。
#![allow(dead_code)]

mod p4d;
mod paths;
mod seed;
mod template;
mod tools;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};

use p4d::P4dServer;
use paths::InstancePaths;
use tools::Tools;

/// 起一个沙箱；找不到 p4d/p4 时返回 `None`，调用方据此打印一行
/// `skipping:` 后跳过。
///
/// 「没有 p4d」是环境不支持，跳过；「有 p4d 但起不来」是真实错误，
/// 直接 panic——不能把它伪装成环境不支持。
///
/// 设了 `P4_E2E_REQUIRED` 就连「没有 p4d」也不许跳过。CI 用它把
/// 「下载失败导致 e2e 整段没跑」变成一个红色的失败，而不是一次绿色的空跑。
/// 靠 grep 日志做不到这件事：那行 `skipping:` 走的是 stderr，而测试框架默认
/// 会捕获用例的输出（libtest 与 nextest 都是），它压根不会出现在 CI 日志里。
pub fn sandbox_or_skip() -> Option<Sandbox> {
    match Sandbox::start() {
        Some(sandbox) => Some(sandbox),
        None => {
            assert!(
                std::env::var_os("P4_E2E_REQUIRED").is_none(),
                "P4_E2E_REQUIRED is set but p4d/p4 is not available"
            );
            eprintln!("skipping: p4d or p4 is not available");
            None
        }
    }
}

/// 组装 [`Sandbox`] 需要的部件，由 [`Sandbox::start_parts`] 产出。
///
/// 单独拆出来是为了把「起服务器」和「组装 Sandbox」分开：前者的失败发生在
/// Drop 接管之前，得由调用方负责清理实例目录。
struct Parts {
    server: P4dServer,
    client: String,
    client_root: PathBuf,
}

pub struct Sandbox {
    tools: Tools,
    paths: InstancePaths,
    server: P4dServer,
    client: String,
    client_root: PathBuf,
}

impl Sandbox {
    pub fn start() -> Option<Sandbox> {
        let tools = tools::discover()?;
        Some(Self::start_with(tools))
    }

    fn start_with(tools: Tools) -> Sandbox {
        let paths = InstancePaths::new(&paths::unique_instance_name());

        // 这一段失败要自己收尸：`Sandbox` 还没造出来，它的 Drop 还没接管，
        // 而实例目录已经建了。少了这一步，探测到坏 p4d、模板生成失败、
        // 端口起不来这些情况都会在 `target/e2e/instances/` 里留下空壳。
        let parts = match Self::start_parts(&tools, &paths) {
            Ok(parts) => parts,
            Err(error) => {
                paths.remove();
                panic!("could not start the sandbox: {error}");
            }
        };

        let sandbox = Sandbox {
            tools,
            paths,
            server: parts.server,
            client: parts.client,
            client_root: parts.client_root,
        };
        // 到这里 Drop 已经接管，下面的失败由它清理。
        sandbox.put_client(&seed::default_view(&sandbox.client));
        sandbox.p4_ok(&["sync", "-f"]);
        sandbox
    }

    /// 起服务器、建工作区，产出组装 [`Sandbox`] 需要的部件。
    fn start_parts(tools: &Tools, paths: &InstancePaths) -> io::Result<Parts> {
        // 同名残留可能来自上一次被 Ctrl-C 或 CI 超时打断的运行——那种结束方式
        // 不走 Drop。pid 会被系统回收，所以「新」实例名未必真的新，而残留的
        // journal 会被 p4d 重放到刚从模板复制来的干净库上，把初始状态改回去。
        paths.remove();
        paths.create()?;
        let template = template::ensure_template(tools)?;
        template::instantiate(&template, paths)?;
        let server = P4dServer::launch(&tools.p4d, paths, &tools.p4)?;

        // client 名一实例一份：工作区互不重叠，digest 缓存的文件名也就不会撞上。
        let client = format!("e2e_{}", paths.name);
        let client_root = paths.client_root(&client);
        fs::create_dir_all(&client_root)?;

        Ok(Parts {
            server,
            client,
            client_root,
        })
    }

    pub fn port(&self) -> u16 {
        self.server.port()
    }

    /// 工作区目录，也就是 client Root。
    pub fn client_root(&self) -> &Path {
        &self.client_root
    }

    /// 本实例的 client 名。一实例一份，写进 view 与缓存文件名。
    pub fn client(&self) -> &str {
        &self.client
    }

    /// 本实例的目录，所有沙箱数据都在它下面。
    pub fn instance_dir(&self) -> &Path {
        &self.paths.dir
    }

    /// 被测程序的命令行，已注入沙箱环境，工作目录是 client Root。
    pub fn cli(&self) -> assert_cmd::Command {
        let mut command = assert_cmd::Command::cargo_bin("p4delta").expect("binary must build");
        command.env_clear();
        command.envs(self.env());
        command.current_dir(&self.client_root);
        command
    }

    // ---- 独立取证 ----

    /// 在沙箱里跑 p4。判断结论要看这里的输出，不能只看被测程序的 stdout。
    pub fn p4(&self, args: &[&str]) -> Output {
        let mut command = Command::new(&self.tools.p4);
        command
            .arg("-p")
            .arg(self.server.address())
            .arg("-u")
            .arg(seed::USER)
            .arg("-c")
            .arg(&self.client)
            .args(args)
            .current_dir(&self.client_root)
            .stdin(Stdio::null());
        apply_env(&mut command, &self.env());
        command.output().expect("p4 must run")
    }

    /// 同上，但非零退出即 panic，返回 stdout。
    pub fn p4_ok(&self, args: &[&str]) -> String {
        let output = self.p4(args);
        assert!(
            output.status.success(),
            "p4 {} failed ({})\nstdout: {}\nstderr: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout).trim_end(),
            String::from_utf8_lossy(&output.stderr).trim_end(),
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// stdout 按行返回，去掉空行。
    pub fn p4_lines(&self, args: &[&str]) -> Vec<String> {
        self.p4_ok(args)
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// 同 [`Sandbox::p4`]，但往 stdin 喂一份表单。
    pub fn p4_input(&self, args: &[&str], form: &str) -> String {
        let mut command = Command::new(&self.tools.p4);
        command
            .arg("-p")
            .arg(self.server.address())
            .arg("-u")
            .arg(seed::USER)
            .arg("-c")
            .arg(&self.client)
            .args(args)
            .current_dir(&self.client_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_env(&mut command, &self.env());

        let mut child = command.spawn().expect("p4 must run");
        {
            let mut pipe = child.stdin.take().expect("stdin was piped");
            pipe.write_all(form.as_bytes()).expect("write the form");
            // 关掉管道，p4 才读得到 EOF。
        }
        let output = child.wait_with_output().expect("p4 must finish");
        assert!(
            output.status.success(),
            "p4 {} failed ({})\nstdout: {}\nstderr: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout).trim_end(),
            String::from_utf8_lossy(&output.stderr).trim_end(),
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// 建一个 pending changelist，返回它的编号。
    ///
    /// 编号直接从 `p4 change -i` 的 `Change 3 created.` 里取，
    /// 比事后翻 `p4 changes` 找最新的那条可靠。
    pub fn new_changelist(&self, description: &str) -> u32 {
        let form = self.p4_ok(&["change", "-o"]);
        let form = form.replace("<enter description here>", description);
        let output = self.p4_input(&["change", "-i"], &form);
        output
            .split_whitespace()
            .skip_while(|token| *token != "Change")
            .nth(1)
            .and_then(|number| number.parse().ok())
            .unwrap_or_else(|| panic!("no changelist number in {output:?}"))
    }

    /// 当前打开的文件，形如 `//depot/main/readme.txt#1 - edit change 0 (text)`。
    /// 没有打开的文件时是空列表（p4 的那句提示会被滤掉）。
    pub fn opened(&self) -> Vec<String> {
        self.p4_lines(&["opened"])
            .into_iter()
            .filter(|line| !line.starts_with("File(s) not opened"))
            .collect()
    }

    // ---- 场景构造 ----

    pub fn write(&self, relative: &str, contents: &str) -> PathBuf {
        self.write_bytes(relative, contents.as_bytes())
    }

    /// 写工作区文件。两件事不能省：
    ///
    /// 1. **先清只读位**——client 的 `noclobber` 让 sync 下来的文件是只读的，
    ///    直接覆写会被系统拒绝。
    /// 2. **把 mtime 回拨**——`is_unchanged_since_sync` 把「与 have 的 syncTime
    ///    相差不超过一秒」当成未改动并跳过摘要计算，而测试往往在 `sync -f`
    ///    之后毫秒级就动手改文件，正好落进这个窗口，会静默漏报成「没有变更」。
    ///    这是确定性的，不是竞态：两个值在 sync 那一刻就定死了。
    pub fn write_bytes(&self, relative: &str, contents: &[u8]) -> PathBuf {
        let path = self.client_root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        clear_readonly(&path);
        fs::write(&path, contents).expect("write workspace file");
        backdate(&path);
        path
    }

    /// 读工作区文件，CRLF 归一成 LF——client 是 `LineEnd: local`，
    /// Windows 上 sync 下来的文本文件是 CRLF。
    pub fn read(&self, relative: &str) -> String {
        let path = self.client_root.join(relative);
        fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
            .replace("\r\n", "\n")
    }

    pub fn read_bytes(&self, relative: &str) -> Vec<u8> {
        let path = self.client_root.join(relative);
        fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
    }

    pub fn remove(&self, relative: &str) {
        let path = self.client_root.join(relative);
        // 只读文件在 Windows 上删不掉，先松开。
        clear_readonly(&path);
        fs::remove_file(&path).unwrap_or_else(|error| panic!("remove {}: {error}", path.display()));
    }

    pub fn exists(&self, relative: &str) -> bool {
        self.client_root.join(relative).is_file()
    }

    /// 把一个新文件加进 depot 并提交，改掉基线。
    pub fn commit(&self, relative: &str, contents: &str) {
        self.write(relative, contents);
        self.p4_ok(&["add", relative]);
        self.p4_ok(&["submit", "-d", "e2e fixture"]);
    }

    /// `p4 sync -f`：把工作区强制拉回 have 状态。
    pub fn sync(&self) {
        self.p4_ok(&["sync", "-f"]);
    }

    /// 重写 client view。unmap 用例会在这里加排除行。
    pub fn set_client_view(&self, view: &[String]) {
        self.put_client(view);
    }

    fn put_client(&self, view: &[String]) {
        let form = seed::client_form(&self.client, &self.client_root, view);
        seed::put_client(&self.tools.p4, self.server.port(), &self.client_root, &form)
            .expect("update the client spec");
    }

    // ---- 环境 ----

    /// 被测程序与沙箱内 p4 的环境。
    ///
    /// 从父进程复制后删掉所有 `P4*`（大小写不敏感）与 `PWD`/`OLDPWD`：
    /// p4 的优先级是「命令行 > .p4config 文件 > 环境变量 > 注册表」，
    /// 光是覆盖环境变量压不住机器上的 `.p4config`，所以还要把
    /// `P4CONFIG` 指到一个不存在的文件名，把「配置文件」这条来源整个关掉。
    ///
    /// 刻意不用 `env_clear()`：Windows 上缺 `SystemRoot` 会让子进程直接起不来，
    /// 而 `P4*` 正是唯一会串味的来源。
    pub fn env(&self) -> Vec<(OsString, OsString)> {
        let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(key, _)| {
                // p4 用 $PWD 判断当前目录，会盖过子进程的真实 cwd；
                // 另外三个是缓存目录的入口（`directories` 在三个平台分别读它们），
                // 也得先清掉父进程的值，下面再设成沙箱内的路径。
                !is_p4_var(key)
                    && !matches!(
                        key.to_string_lossy().to_uppercase().as_str(),
                        "PWD" | "OLDPWD" | "LOCALAPPDATA" | "XDG_CACHE_HOME" | "HOME"
                    )
            })
            .collect();

        let mut set = |key: &str, value: String| env.push((OsString::from(key), value.into()));
        set("P4PORT", self.server.address());
        set("P4USER", seed::USER.to_owned());
        set("P4CLIENT", self.client.clone());
        set("P4CONFIG", "noconfig".to_owned());
        // 剪枝只在生效的 P4IGNORE 恰好是 .p4ignore 时才敢用目录级判断。
        set("P4IGNORE", ".p4ignore".to_owned());
        // 设了就不必跑 `p4 set` 去探测，也躲开这台机器的注册表。
        set("P4CHARSET", "utf8".to_owned());
        set("P4TICKETS", self.paths.tickets.display().to_string());
        set("P4ENVIRO", self.paths.enviro.display().to_string());
        set("P4TRUST", self.paths.trust.display().to_string());
        // 任何交互式工具都会挂住测试，一律换成立即返回的命令。
        set("P4EDITOR", ok_command());
        set("P4MERGE", fail_command());
        set("P4DIFF", fail_command());
        set("PATH", self.path_with_tools());

        // 把被测程序的摘要缓存圈进沙箱，否则它会写进真实用户的缓存目录，
        // 并在多次运行之间互相污染。directories 在三个平台分别读这三个变量。
        let cache = self.paths.cache.display().to_string();
        set("LOCALAPPDATA", cache.clone());
        set("XDG_CACHE_HOME", cache);
        set("HOME", self.paths.home.display().to_string());

        env
    }

    /// p4 的目录插到 PATH 最前面。
    ///
    /// 生产代码的定位顺序是 `P4_EXE` → `PATH` → P4V 安装目录，沙箱刻意只走 `PATH`
    /// 这一段：tools 目录排在 `PATH` 最前，第 2 步必定命中沙箱这一份，**跑测试的机器上
    /// 就算装了 P4V 也不会串味**。（`env()` 会把父进程的 `P4_EXE` 一并剥掉，见 `is_p4_var`。）
    fn path_with_tools(&self) -> String {
        let existing = std::env::var_os("PATH").unwrap_or_default();
        let mut entries = vec![self.tools.bin_dir()];
        entries.extend(std::env::split_paths(&existing));
        std::env::join_paths(entries)
            .unwrap_or(existing)
            .to_string_lossy()
            .into_owned()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if std::env::var_os("P4_KEEP_SANDBOX").is_some() {
            // 保留现场的意思是一个还能连上去的现场，所以服务器不能杀：
            // 杀了的话下面打印的端口就是个死端口，按 CONTRIBUTING.md 的命令连过去
            // 只会得到 "Connect to server failed"。也顺带保住了 journal——
            // p4d 先写 journal 再改库，强杀会把最后几条事务留在 journal 里。
            self.server.keep_running();
            // 缓存照清不误：它落在**真实的**用户缓存目录里，跟要保留的现场
            // 没有关系，留下只是垃圾。
            self.remove_predicted_cache();
            let port = self.server.address();
            eprintln!(
                "P4_KEEP_SANDBOX is set, keeping {}\n  \
                 服务器还在跑，可以直接连：\n    \
                 p4 -p {port} -u {} -c {} opened\n  \
                 用完记得停掉：p4 -p {port} -u {} admin stop",
                self.paths.dir.display(),
                seed::USER,
                self.client,
                seed::USER,
            );
            return;
        }

        self.server.kill();
        self.remove_predicted_cache();
        self.paths.remove();
    }
}

impl Sandbox {
    /// 尽力删掉被测程序写下的 digest 缓存。
    ///
    /// Linux/macOS 上 [`Sandbox::env`] 已经把它圈进了实例目录，随实例目录一起删掉；
    /// Windows 上 `directories` 走 `SHGetKnownFolderPath`，环境变量改不动它，
    /// 缓存只会落在真实用户的缓存目录里，所以只能按标准位置预测、退出时清理。
    /// 文件名里带着本实例唯一的 client 名，即便这次没删掉，也不会和别的实例
    /// 或者用户自己的缓存撞上。
    fn remove_predicted_cache(&self) {
        let Some(path) = self.predicted_cache_path() else {
            return;
        };
        let _ = fs::remove_file(&path);
        // 写缓存是「先写 .tmp 再改名」，中途失败会留下它。
        let _ = fs::remove_file(path.with_extension("bin.tmp"));
    }

    fn predicted_cache_path(&self) -> Option<PathBuf> {
        let name = format!("digests_{}.bin", self.client);
        if cfg!(windows) {
            // 预测值取自环境变量，而实际值来自 SHGetKnownFolderPath：
            // 标准配置下两者一致，重定向过用户目录的机器上可能不同。
            let local = std::env::var_os("LOCALAPPDATA")?;
            Some(
                PathBuf::from(local)
                    .join("p4delta")
                    .join("cache")
                    .join(name),
            )
        } else if cfg!(target_os = "macos") {
            Some(
                self.paths
                    .home
                    .join("Library/Caches/com.p4delta")
                    .join(name),
            )
        } else {
            Some(self.paths.cache.join("p4delta").join(name))
        }
    }
}

/// `-l` 清单里可能出现的动作标签：前八个来自 `src/reconcile/changes.rs` 的
/// `GROUPS`，后两个来自 `src/reconcile/clean.rs` 的 `CLEAN_GROUPS`。
const CHANGE_LABELS: [&str; 10] = [
    "Add",
    "Edit",
    "Reopen Edit",
    "Delete",
    "Reopen Delete",
    "Revert Add",
    "Revert Edit",
    "Revert Delete",
    // clean 模式
    "Revert",
    "Restore",
];

/// 从 `-l` 的输出里取出 (动作标签, 文件路径)。
///
/// 清单行形如 `         Add "C:\ws\fresh.txt".`，标签本身可能含空格
/// （`Reopen Edit`），所以按引号切分，而不是按第一个空格。
/// 只认已知标签，免得把 `Using workspace "x".` 这类别的行也吃进来。
pub fn listed_changes(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim_end().strip_suffix("\".")?;
            let (label, file) = line.split_once(" \"")?;
            let label = label.trim();
            CHANGE_LABELS
                .contains(&label)
                .then(|| (label.to_owned(), file.to_owned()))
        })
        .collect()
}

/// 把沙箱环境装到命令上。
///
/// 必须先 `env_clear()` 再装：`envs()` 只**覆盖**它提到的键，没提到的仍然
/// 从父进程继承——被 [`Sandbox::env`] 过滤掉的 `P4PORT`、`PWD` 会原样漏进来，
/// 于是 p4 拿着这台机器的 `PWD` 去找工作区，相对路径全解析到了别处。
/// `env()` 返回的是一份完整的、已经过滤过的环境（连 `SystemRoot` 这些
/// Windows 必需的变量都在里面），所以清空重装是安全的。
fn apply_env(command: &mut Command, env: &[(OsString, OsString)]) {
    command.env_clear();
    command.envs(env.iter().cloned());
}

/// 键是否落在 p4 的配置空间里。Windows 的环境变量名不区分大小写，一律折成大写比。
fn is_p4_var(key: &OsStr) -> bool {
    key.to_string_lossy().to_uppercase().starts_with("P4")
}

/// 把父进程环境里的 `P4*` 与 `PWD`/`OLDPWD` 从子进程环境里剔掉。
///
/// 不剔干净的话，这台机器上的默认 client、端口、票据会盖过沙箱的设置——
/// 症状是「p4 用了一个不存在的 client」，或者更糟：连到了别人的服务器。
pub(crate) fn scrub_p4_vars(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if is_p4_var(&key) {
            command.env_remove(key);
        }
    }
    command.env_remove("PWD");
    command.env_remove("OLDPWD");
}

/// 清掉只读位。没有这个位、或者文件不存在时都是空操作。
///
/// `noclobber` 是 p4 在客户端侧实现的，两个平台都会让 sync 下来的文件不可写，
/// 所以构造场景前必须先松开——否则覆写会被系统直接拒绝。
#[cfg(unix)]
fn clear_readonly(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    let mode = metadata.permissions().mode();
    // 只补 owner 的写位。`set_readonly(false)` 在这里是把权限设成
    // `0o666 & !umask`，对本来更严格的文件等于顺手放宽了它。
    if mode & 0o200 == 0 {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o200));
    }
}

/// 清掉只读位，见 Unix 侧的说明。
///
/// Windows 只有「只读属性」这一个位，清掉它就是全部——没有 Unix 那套
/// owner/group/other 的语义，`set_readonly(false)` 正是这里要的。
/// clippy 的 `permissions_set_readonly_false` 讲的是 Unix 的坑，在这个
/// `cfg` 下不适用。
#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)]
fn clear_readonly(path: &Path) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    let mut permissions = metadata.permissions();
    if permissions.readonly() {
        permissions.set_readonly(false);
        let _ = fs::set_permissions(path, permissions);
    }
}

/// 把 mtime 回拨一小时，让它远离 have 的 syncTime。
fn backdate(path: &Path) {
    let Ok(file) = fs::File::options().write(true).open(path) else {
        return;
    };
    if let Some(old) = SystemTime::now().checked_sub(Duration::from_secs(3600)) {
        let _ = file.set_modified(old);
    }
}

fn ok_command() -> String {
    if cfg!(windows) {
        "cmd.exe /c exit 0".to_owned()
    } else {
        "true".to_owned()
    }
}

fn fail_command() -> String {
    if cfg!(windows) {
        "cmd.exe /c exit 1".to_owned()
    } else {
        "false".to_owned()
    }
}
