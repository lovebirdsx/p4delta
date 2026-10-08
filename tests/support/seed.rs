//! 沙箱的种子：depot 内容、client 与初始提交历史。

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use super::p4d::P4dServer;
use super::paths::InstancePaths;
use super::tools::Tools;

/// 沙箱里唯一的用户名。
pub const USER: &str = "sandbox";
/// 默认 client 名。
pub const CLIENT: &str = "sandbox_main";
/// depot 里所有内容的根。
pub const DEPOT_ROOT: &str = "//depot/main";

/// 默认的 client view：整个 depot 子树都映射进工作区。
pub fn default_view(client: &str) -> Vec<String> {
    vec![format!("{DEPOT_ROOT}/... //{client}/...")]
}

/// client 表单文本。`Root` 与 `view` 由调用方决定：模板里指向 staging，
/// 实例化后指向该实例自己的工作区；view 会被 unmap 用例改掉。
///
/// 只写必要字段，其余交给 p4 补默认值；`Options` 显式写出来是为了不受
/// 服务器默认值变化的影响。
pub fn client_form(client: &str, root: &Path, view: &[String]) -> String {
    client_form_with_alt_roots(client, root, view, &[])
}

/// 同 [`client_form`]，另附 `AltRoots`：同一个 client 的其它工作区根。
///
/// 多根布局用例靠它把 client 变成「同一个 client、多个根」——此时 `p4 info` 报的
/// `clientRoot` 随 cwd 变，而 client spec 里的 `Root` 始终是第一个。
pub fn client_form_with_alt_roots(
    client: &str,
    root: &Path,
    view: &[String],
    alt_roots: &[&Path],
) -> String {
    let view: String = view.iter().map(|line| format!("\t{line}\n")).collect();
    let alt_roots: String = if alt_roots.is_empty() {
        String::new()
    } else {
        let mut field = String::from("AltRoots:\n");
        for alt in alt_roots {
            field.push_str(&format!("\t{}\n", alt.display()));
        }
        field.push('\n');
        field
    };
    format!(
        "Client: {client}\n\
         \n\
         Owner: {USER}\n\
         \n\
         Root: {root}\n\
         \n\
         {alt_roots}\
         Options: noallwrite noclobber nocompress unlocked nomodtime normdir\n\
         \n\
         SubmitOptions: submitunchanged\n\
         \n\
         LineEnd: local\n\
         \n\
         View:\n{view}",
        root = root.display(),
    )
}

/// 用 `p4 client -i` 提交一份 client 表单。
///
/// 这里刻意不指定工作区：client 还不存在，拿它当「当前 client」会让 p4 直接拒绝。
/// `-i` 认的是表单里的 `Client:` 字段，不需要当前 client 能解析出来。
pub fn put_client(p4: &Path, port: u16, cwd: &Path, form: &str) -> io::Result<()> {
    run_p4(p4, port, cwd, None, &["client", "-i"], Some(form))
}

/// 在 `staging` 里生成一份 p4d 数据库模板。
///
/// 只放 depot 与提交历史，**不放 client**：client 由每个实例自己建，
/// 名字唯一、Root 指向自己的工作区——顺带让 digest 缓存的文件名一实例一份。
///
/// 跑完只留下 `server/`：工作区、缓存、日志在实例化时都会重建，
/// 留着只会让模板目录变大，而且日志里的端口号对每个实例都是错的。
pub fn seed_into(staging: &Path, tools: &Tools) -> io::Result<()> {
    let paths = InstancePaths::at(staging.to_path_buf());
    paths.create()?;

    init_unicode_database(&tools.p4d, &paths)?;

    // 用 launch 而不是 start：它等到服务器真的应答才开始填内容。
    // 少了这一步，第一条 p4 命令会撞在没起来的服务器上，靠客户端内部重试
    // 硬扛两秒多——能用，但每次建模板都白等。
    let mut server = P4dServer::launch(&tools.p4d, &paths, &tools.p4)?;
    let result = fill_depot(&tools.p4, &paths, server.port());
    // 这里必须干净收尾，不能强杀：模板要的是刷完盘之后的数据库。
    server.stop_cleanly(&tools.p4, &paths.dir, USER);
    result?;

    for leftover in [&paths.ws_root, &paths.cache, &paths.home, &paths.log] {
        remove_any(leftover);
    }
    Ok(())
}

/// `p4d -xi`：把空库初始化成 Unicode 模式。
///
/// 沙箱用 `P4CHARSET=utf8`，而 UTF-8 的客户端要求服务器处于 Unicode 模式，
/// 否则 p4 会直接报「Unicode clients require a unicode enabled server」。
fn init_unicode_database(p4d: &Path, paths: &InstancePaths) -> io::Result<()> {
    let output = Command::new(p4d)
        .arg("-r")
        .arg(&paths.server)
        .arg("-xi")
        .current_dir(&paths.dir)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "p4d -xi failed ({}): {}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout).trim_end(),
            String::from_utf8_lossy(&output.stderr).trim_end(),
        )));
    }
    Ok(())
}

/// 建 client、写基线文件、提交。`//depot` 由 p4d 初始化时自动创建，不用自己建。
fn fill_depot(p4: &Path, paths: &InstancePaths, port: u16) -> io::Result<()> {
    let root = paths.client_root(CLIENT);
    fs::create_dir_all(&root)?;

    put_client(
        p4,
        port,
        &root,
        &client_form(CLIENT, &root, &default_view(CLIENT)),
    )?;

    write_file(&root, "readme.txt", "hello from the depot\n")?;
    write_file(&root, "src/lib.txt", "library\n")?;
    write_file(&root, "src/deep/a/b/c.txt", "deep\n")?;
    // 非 ASCII 文件名：从写盘到 p4 入库、再到被测程序读回来，整条链路都要能对上。
    write_file(&root, "src/使用说明.txt", "中文内容\n")?;
    fs::write(root.join("logo.bin"), b"BINARY\x00\x01\x02payload")?;

    // 二进制要显式声明类型，否则会按文本入库。
    for args in [
        &["add", "-t", "binary", "logo.bin"][..],
        // `src/...` 而不是 `src`：后者会被 p4 当成一个叫 src 的目录文件。
        &["add", "readme.txt", "src/..."][..],
        &["submit", "-d", "seed"][..],
    ] {
        run_p4(p4, port, &root, Some(CLIENT), args, None)?;
    }

    // 建它的目的只是把文件提交进去；模板里不留 client，
    // 否则每个实例还得先把它改造成自己的，不如各建各的。
    run_p4(
        p4,
        port,
        &root,
        Some(CLIENT),
        &["client", "-d", "-f", CLIENT],
        None,
    )?;
    Ok(())
}

fn write_file(root: &Path, relative: &str, contents: &str) -> io::Result<()> {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}

/// 跑一条 p4 命令，非零退出即失败。
///
/// `client` 为 `None` 时不指定工作区（创建 client 的那一步，见 [`put_client`]）。
/// `stdin` 用于表单类命令。
pub fn run_p4(
    p4: &Path,
    port: u16,
    cwd: &Path,
    client: Option<&str>,
    args: &[&str],
    stdin: Option<&str>,
) -> io::Result<()> {
    let mut command = Command::new(p4);
    // 这台机器上的 P4CLIENT/P4PORT 之类一旦留下来就会盖过下面的设置，
    // 表现为「p4 用了一个不存在的 client」这种莫名其妙的错误。
    super::scrub_p4_vars(&mut command);
    command
        .arg("-p")
        .arg(format!("127.0.0.1:{port}"))
        .arg("-u")
        .arg(USER)
        .args(args)
        .current_dir(cwd)
        // 与生产代码一致：p4 会用继承来的 PWD 找配置，必须清掉。
        .env_remove("PWD")
        .env("P4CONFIG", "noconfig")
        .env("P4CHARSET", "utf8")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    if let Some(client) = client {
        command.env("P4CLIENT", client);
    }

    let mut child = command.spawn()?;
    if let Some(text) = stdin {
        let mut pipe = child.stdin.take().expect("stdin was piped");
        pipe.write_all(text.as_bytes())?;
        // 关掉管道，p4 才读得到 EOF。
        drop(pipe);
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "p4 {} failed ({})\nstdout: {}\nstderr: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout).trim_end(),
            String::from_utf8_lossy(&output.stderr).trim_end(),
        )));
    }
    Ok(())
}

fn remove_any(path: &Path) {
    if path.is_dir() {
        let _ = fs::remove_dir_all(path);
    } else {
        let _ = fs::remove_file(path);
    }
}
