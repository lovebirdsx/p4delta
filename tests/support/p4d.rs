//! p4d 进程的生命周期。

use std::fs;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::paths::InstancePaths;

/// 等 p4d 就绪的上限。测试机上通常几百毫秒。
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// 端口被抢时最多换几次。冲突本身失败得很快（p4d 立刻退出），
/// 所以次数给足不心疼；真到了上限说明有别的问题，错误信息会带上日志。
const PORT_ATTEMPTS: u32 = 8;

/// 等进程退出的上限。干净收尾正常约一秒，强杀是立刻的，
/// 这个值只用来兜住「服务器卡住不动」的情况。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

pub struct P4dServer {
    child: Child,
    port: u16,
    journal: PathBuf,
    pid_file: PathBuf,
    log: PathBuf,
    /// [`P4dServer::keep_running`] 置上：不再收尾，把现场原样留着。
    kept: bool,
}

impl P4dServer {
    /// 起一个 p4d 并等它就绪，端口被抢走时换一个重来。
    ///
    /// 端口是「先 bind 到 0 拿一个空闲端口、立刻放开、再交给 p4d」这样选的，
    /// 从放开到 p4d 真正 bind 之间有一段窗口，并行的另一个实例可能在这期间
    /// 拿到同一个端口——Windows 尤其倾向于把刚释放的端口再发一次，实测并行跑
    /// 整个测试集时能撞上 `WSAEADDRINUSE`。这不是故障，重试就行。
    pub fn launch(exe: &Path, paths: &InstancePaths, p4: &Path) -> io::Result<Self> {
        for attempt in 1..=PORT_ATTEMPTS {
            // p4d 往 `-L` 指定的文件是**追加**写，不是覆盖。上一次尝试留下的
            // `WSAEADDRINUSE` 会一直挂在末尾，让这次真正的失败被误判成端口冲突，
            // 白重试到上限才报出来。每次开新的之前先清掉。
            let _ = fs::remove_file(&paths.log);

            let mut server = Self::start(exe, paths)?;
            let error = match server.wait_ready(p4, &paths.dir) {
                Ok(()) => return Ok(server),
                Err(error) => error,
            };

            // 读日志要赶在收尸之前——虽然 kill 不动日志文件，但少一个先后依赖。
            let conflict = is_port_conflict(&server.log_tail());
            server.kill();
            if !conflict || attempt == PORT_ATTEMPTS {
                return Err(error);
            }
        }
        unreachable!("the loop returns on its last attempt")
    }

    /// 起一个 p4d。`paths.dir` 会被用作它的工作目录。
    ///
    /// 工作目录必须是沙箱目录：p4d 会读 cwd 下名为 `license` 的文件，
    /// 而工程根正好有一份 `LICENSE`，会让它报 license 语法错误。
    fn start(exe: &Path, paths: &InstancePaths) -> io::Result<Self> {
        let port = free_port()?;
        let child = Command::new(exe)
            .arg("-r")
            .arg(&paths.server)
            .arg("-p")
            .arg(format!("127.0.0.1:{port}"))
            .arg("-J")
            .arg(&paths.journal)
            .arg("-L")
            .arg(&paths.log)
            .arg("-q")
            .arg(format!("--pid-file={}", paths.pid_file.display()))
            .current_dir(&paths.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(P4dServer {
            child,
            port,
            journal: paths.journal.clone(),
            pid_file: paths.pid_file.clone(),
            log: paths.log.clone(),
            kept: false,
        })
    }

    /// 保留现场：不再管这个进程，让它继续跑。
    ///
    /// 见 `Sandbox` 的 `Drop`：保留现场的意义是一个还能连上去的现场。
    pub fn keep_running(&mut self) {
        self.kept = true;
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `127.0.0.1:<port>`，可直接用作 P4PORT。
    pub fn address(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// 轮询直到服务器应答。只由 [`P4dServer::launch`] 调用——外面看到的
    /// 应该是一个已经能用 `p4` 连上的服务器，而不是一个刚 spawn 出来的进程。
    ///
    /// 分两步：先等端口开始监听，再用 `p4 info` 确认协议层就绪。
    /// 这一步省不掉——p4 客户端连不上端口时会在内部重试两秒多，
    /// 拿失败的 `p4 info` 当轮询器会让每次启动白白慢十几秒。
    fn wait_ready(&mut self, p4: &Path, cwd: &Path) -> io::Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        let address = SocketAddr::from(([127, 0, 0, 1], self.port));

        while TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_err() {
            self.check_alive()?;
            if Instant::now() >= deadline {
                return Err(io::Error::other(format!(
                    "p4d never started listening within {READY_TIMEOUT:?}; log:\n{}",
                    self.log_tail()
                )));
            }
            thread::sleep(Duration::from_millis(10));
        }

        // 端口已经在听了，这时候的探测失败都是快的。
        let mut interval = Duration::from_millis(20);
        loop {
            self.check_alive()?;
            let probe = Command::new(p4)
                .arg("-p")
                .arg(self.address())
                .args(["-u", "sandbox", "info"])
                .env("P4CONFIG", "noconfig")
                .env("P4CHARSET", "utf8")
                // p4 用继承来的 PWD 判断当前目录，会盖过真实 cwd。
                .env_remove("PWD")
                .current_dir(cwd)
                .stdin(Stdio::null())
                .output();
            if let Ok(output) = probe
                && output.status.success()
            {
                return Ok(());
            }

            if Instant::now() >= deadline {
                return Err(io::Error::other(format!(
                    "p4d did not answer p4 info within {READY_TIMEOUT:?}; log:\n{}",
                    self.log_tail()
                )));
            }
            thread::sleep(interval);
            interval = (interval * 3 / 2).min(Duration::from_millis(500));
        }
    }

    /// p4d 起不来时（端口被占、数据库损坏）要立刻失败，而不是等满超时。
    fn check_alive(&mut self) -> io::Result<()> {
        if let Some(status) = self.child.try_wait()? {
            return Err(io::Error::other(format!(
                "p4d exited before becoming ready ({status}); log:\n{}",
                self.log_tail()
            )));
        }
        Ok(())
    }

    /// 强杀并清掉 journal。幂等。
    ///
    /// 不走 p4d 的干净收尾：那要近一秒，而这里不需要保留任何数据。
    /// 强杀之后必须删 journal——它在 `server/` 之外，模板复制覆盖不到，
    /// 残留会被下次启动重放，把刚要来的干净状态改回去。
    pub fn kill(&mut self) {
        if self.kept {
            return;
        }
        let _ = self.child.kill();
        // 信号已经发了，这里只是回收进程对象，但也不无限等。
        let _ = self.wait_until(SHUTDOWN_TIMEOUT);
        let _ = fs::remove_file(&self.journal);
        let _ = fs::remove_file(&self.pid_file);
    }

    /// 等到子进程退出，超时返回 false。
    ///
    /// 不用 `Child::wait`：它没有超时，而 libtest 也不给用例设超时，
    /// 服务器真卡住时会把 CI 一路挂到 job 上限——那是个「没有结果」，
    /// 远不如一个失败来得有用。
    fn wait_until(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                _ => return false,
            }
        }
    }

    /// 干净收尾：让 p4d 把数据库刷完再退出。**模板快照前必须用它。**
    ///
    /// p4d 是「先写 journal、再改数据库」，强杀会让最近的事务只留在 journal 里；
    /// 而 journal 刻意放在 `server/` 之外、不会被模板复制带走——等于把刚 seed
    /// 的内容连同 Unicode 初始化标记一起丢掉，症状是实例起来后
    /// 「Unicode clients require a unicode enabled server」。
    ///
    /// 慢（约一秒），但模板只建一次。
    pub fn stop_cleanly(&mut self, p4: &Path, cwd: &Path, user: &str) {
        let _ = Command::new(p4)
            .arg("-p")
            .arg(self.address())
            .arg("-u")
            .arg(user)
            .args(["admin", "stop"])
            .current_dir(cwd)
            .env_remove("PWD")
            .env("P4CONFIG", "noconfig")
            .env("P4CHARSET", "utf8")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        // 等它真的退出，否则接下来的复制会读到写了一半的数据库。
        // 停不下来就强杀——宁可放弃这个模板，也不能把测试挂死在这里。
        if !self.wait_until(SHUTDOWN_TIMEOUT) {
            self.kill();
            return;
        }
        let _ = fs::remove_file(&self.journal);
        let _ = fs::remove_file(&self.pid_file);
    }

    /// 日志末尾若干行，附在失败信息里。
    fn log_tail(&self) -> String {
        let Ok(text) = fs::read_to_string(&self.log) else {
            return format!("<no log at {}>", self.log.display());
        };
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(20);
        lines[start..].join("\n")
    }
}

impl Drop for P4dServer {
    fn drop(&mut self) {
        self.kill();
    }
}

/// p4d 报端口被占用的特征串：Windows 给 `WSAEADDRINUSE`，Unix 给这个短语。
fn is_port_conflict(log: &str) -> bool {
    log.contains("WSAEADDRINUSE") || log.contains("Address already in use")
}

/// 让 OS 分配一个空闲端口再立刻放开。
///
/// 拿回端口到 p4d 绑定之间有极小的窗口，并行测试偶尔会撞上（见
/// [`P4dServer::launch`] 的重试）。这仍然比固定端口段稳得多：固定段要么
/// 和机器上别的东西撞，要么得靠人工分配、并行度就锁死了。
fn free_port() -> io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}
