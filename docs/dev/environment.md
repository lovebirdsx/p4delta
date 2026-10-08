# 本地与 CI 的环境差异

几条容易造成「本地绿、CI 红」的环境差异，排查时先排掉它们。

## 复现「没有 p4」的环境

想在没有 p4 的机器上验证「跳过路径」，或复现「runner 上什么都没有」的环境，可以把 p4 目录从 `PATH` 里剔掉再跑（PowerShell）：

```powershell
$env:PATH = ($env:PATH -split ';' | Where-Object { $_ -and $_ -notmatch 'erforce' }) -join ';'
where.exe p4   # 用子进程视角验证，别用 Get-Command（它有缓存，会给出假的 True）
cargo test --all-targets --all-features --locked
```

别按精确路径比较来剔除：`PATH` 条目常带尾部反斜杠（如 `...\Perforce\`），与 `Split-Path (Get-Command p4).Source` 的无斜杠写法用 `-ne` 比不相等，剔不掉——用模糊匹配。这份复现用的是 `cargo test`，注意它的 fail-fast 会让后面的测试二进制整个不跑（见 [testing.md](testing.md)）；日常测试用 nextest。

关于 e2e 在缺 p4d/p4 时的跳过行为与 CI 的 `P4_E2E_REQUIRED` 门禁，见 [e2e.md](e2e.md)。

## WSL：验 Linux 侧行为

装了多个发行版时要显式 `-d` 指定（默认发行版未必与 CI 钉的版本相同），挑与 CI 同版本的，比如 `Ubuntu-24.04`。把 target dir 指到家目录的 ext4，绕开 `/mnt/*` 的 drvfs 慢速，也不污染 Windows 侧的 `target/`：

```bash
wsl -d Ubuntu-24.04 -e bash -lc 'cd /mnt/e/git_project/p4delta && \
  export PATH=$HOME/.cargo/bin:$PATH && \
  CARGO_TARGET_DIR=$HOME/p4delta-target cargo test --lib --locked <过滤词>'
```

别并发跑两个 rustup（同时写同一个 toolchain 目录会 `recovering from a partially installed toolchain`、报 `bin/rust-gdb` 冲突而装坏）；撞上时再跑一次 `rustup toolchain list` 触发同步自愈。

要在这边跑 e2e（而不只是 `--lib`）就先下一份 Linux 版 p4：`bash scripts/fetch-p4-tools.sh`，再把 `P4_EXE` 指过去。它按 `uname` 挑包，往 `vendor/` 放的是无后缀的 `p4`/`p4d`，与 Windows 侧那两个 `.exe` 并存，也不进版本库。

这个仓库反复踩过「只在 Windows 上过」的坑——用例硬编码 `C:\ws\...`，而 `local_path_key` 按 `MAIN_SEPARATOR` 切组件、假造路径还未必是本地意义上的绝对路径，于是 ubuntu 与 macos 上失败、Windows 上照过。推 CI 前先在 WSL 里复现一遍 Linux 侧。形态有三种，写用例时对着查：

- **写死的路径键**（`records.contains_key("e:\\ws\\kept.txt")`）：期望值该用 `local_path_key(...)` 表达，那才是产线口径，折叠平台上它自然折成小写。
- **写死的盘符根**（`const ROOT: &str = "C:\\ws"`）：按平台定义，Unix 侧从 `/` 起。`C:\ws` 在 Unix 上是个**相对**路径，`canonical_local_path` 会把它拼到 cwd 上。
- **只在大小写上不同的两条路径**（`Snow_Normal` / `Snow_normal`）：它们只在不区分大小写的平台上折成同一个键，依赖这个前提的用例得按 `path_identity_ignores_case()` 分支。

## ACP：非 ASCII 命令行参数

**本机 ANSI 代码页（ACP）会让「非 ASCII 命令行参数」类 bug 永远复现不了**。Windows 上开了「Beta: Use Unicode UTF-8 for worldwide language support」的机器 ACP 是 **65001**（中文 Windows 默认是 936），而 p4 在 Windows 上会把命令行解析两遍（宽字符一遍、ANSI 一遍）——ACP=65001 时 ANSI 那一遍不丢字符，本地怎么跑都绿；CI 的 `windows-latest` 是 en-US（ACP=1252），中文文件名必然被吃成 `????`。遇到「只有 Windows CI 红」时**先查 ACP**：

```powershell
Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Nls\CodePage' | Select ACP
```

返回值是 65001 就说明本地没有复现条件，别再花时间在本地试非 ASCII 文件名——直接按「非 ASCII 命令行参数」这一假设去读代码（相关实现：`src/p4/process.rs` 的 `command_line_safe`），或推一次 CI 取数据。
