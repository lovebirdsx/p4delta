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

## p4 先连服务器，再跑子命令

p4 在跑任何子命令之前**先建立到 `P4PORT` 的连接**：连不上就报
`Connect to server failed; check $P4PORT.`、退出码 1——**连非法 flag（`p4 reconcile --bogus`）也不例外**，命令行的本地解析排在连接之后。这一点造成一处稳定的本地/CI 差异：

- **开发机**上 p4 有配置（注册表或 `P4*` 环境变量，`p4 set` 能看到），连接瞬间完成；
- **CI runner** 上什么都没有，p4 兜底去连 `perforce:1666`，DNS 解析加连接超时**每次约 4.7 秒**。

代价落在「拿一个必然失败的 p4 查询当夹具」的单元测试上（它们要的只是「退出码非零 + stderr 有内容」，并不需要连上谁）。2026-10-15 实测，`p4::process::tests::strict_p4_queries_reject_failed_output` 与 `prune::tests::pruning_plan_falls_back_when_a_p4_query_fails` 各调两次 p4，在 windows-latest 上是 **11.0 / 10.9 秒**，本机 0.12 / 0.19 秒，两条合计占该 job 全部用例耗时之和的 14%。

所以 CI 的 `Run tests` 给测试进程设了 `P4PORT=nonexistent.invalid:1666`：

```bash
# 本机复现 CI 条件（没有 P4 配置的机器天然就是这个条件）
P4_E2E_REQUIRED=1 P4PORT=nonexistent.invalid:1666 cargo nextest run --all-targets --all-features
```

`.invalid` 是 RFC 2606 保留 TLD，保证解析不了，p4 在 **150 毫秒**内报同一个
`Connect to server failed`——失败语义一字不变，只是不再等网络。别图省事换
`127.0.0.1:1`：本地端口被拒后 p4 仍要 **2.1 秒**才返回（实测），比 `.invalid` 慢一个量级。

两处不受影响：**e2e 沙箱**给被测程序的环境里不继承任何 `P4*` 变量、自己设指向实例 p4d 的 `P4PORT`；**测试发现**走的是 `p4_available()` 的 `p4 -V`，它不需要服务器。本机 491 条在这种环境变量下全绿、整轮墙钟与有服务器时持平（2026-10-15）。

## WSL：验 Linux 侧行为

装了多个发行版时要显式 `-d` 指定（默认发行版未必与 CI 钉的版本相同），挑与 CI 同版本的，比如 `Ubuntu-24.04`：

```bash
wsl -d Ubuntu-24.04 -e bash -lc 'cd ~/git_project/p4delta && \
  export PATH=$HOME/.cargo/bin:$PATH && \
  CARGO_TARGET_DIR=$HOME/p4delta-target cargo test --lib --locked <过滤词>'
```

里面的 `CARGO_TARGET_DIR` 只对**仓库在 Windows 盘上**（`/mnt/*`）的情形有意义：把 target dir 指到家目录的 ext4，绕开 drvfs 的慢速。仓库本身就在 ext4 上时不必为速度设它，留着的作用只剩「与 Windows 侧那份构建分开」——两边各自的 `target/` 不互相踩。`scripts/install-local.sh` 认这个变量：设了就去那里取产物，不设就用仓库里的 `target/`。

别并发跑两个 rustup（同时写同一个 toolchain 目录会 `recovering from a partially installed toolchain`、报 `bin/rust-gdb` 冲突而装坏）；撞上时再跑一次 `rustup toolchain list` 触发同步自愈。

要在这边跑 e2e（而不只是 `--lib`）就先下一份 Linux 版 p4：`bash scripts/fetch-p4-tools.sh`，再把 `P4_EXE` 指过去。它按 `uname` 挑包，往 `vendor/` 放的是无后缀的 `p4`/`p4d`，与 Windows 侧那两个 `.exe` 并存，也不进版本库。

构建出来的那份想在 WSL 里**当命令用**，跑 `bash scripts/install-local.sh`：cargo build → 冒烟跑一次 `--version` → 原子替换到 `~/.local/bin/p4delta`。它不碰 P4V，也不碰 PATH（`~/.local/bin` 由发行版的 `~/.profile` 默认加进去）——往 P4V 里装是 Windows 侧 `scripts/install-local.ps1` 的事（见 [release.md](release.md)）。

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

这条事实现在有一处**依赖**它：强制修复的删除保护只能走命令行（`p4 ignores` 不认 stdin），它因此不按平台一刀切，而是运行时逐字核对 p4 回显的路径（`src/workspace.rs::query_ignores_verbose`）——能表示这些名字的机器上照常跑完，表示不了的机器上 fail-closed 停下。对应的两条 e2e 成功用例按同一条事实分档（`Sandbox::argv_carries_non_ascii`），第三条成功用例改由 shim 注入逐字裁决、因此不依赖它；见 [e2e.md](e2e.md)。
