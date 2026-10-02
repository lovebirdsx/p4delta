# p4delta 开发指南

面向要改 p4delta 代码、跑 e2e 沙箱或发版的人。安装与使用见 [README](README.md)。

## 开发与验证

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

CI 以 `-D warnings` 为硬门禁，clippy 有任何警告都算失败。

测试分三处：单元测试跟着被测代码放在各模块的 `#[cfg(test)] mod tests` 里，跨模块复用的测试基建在 `src/test_util.rs`；`tests/` 下是黑盒用例，通过进程边界观察，不引用 crate 内部符号；`tests/support/` 是 e2e 沙箱框架。

安装脚本另有黑盒测试，不需要构建产物（它用一个人造 exe 复刻「从 release 包解压出来直接跑」的布局）：

```powershell
pwsh -File scripts/test-install.ps1        # 或 powershell -File ...
```

CI 用 Windows PowerShell 5.1 与 PowerShell 7 各跑一遍。两个脚本都**必须带 UTF-8 BOM**：5.1 对没有 BOM 的文件按系统 ANSI 代码页解析，里面的中文会全变乱码（`test-install.ps1` 有一条断言拦着）。

### 装本地构建到 P4V 里验证

改完代码想在 P4V 里点一遍——别手工拷 exe，用 `scripts/install-local.ps1`：它构建 → 跑一次
`--version` 冒烟 → 把安装目录里现有的 exe 备份下来 → 调 `install.ps1` 铺好并注册工具。

```powershell
pwsh -File scripts/install-local.ps1              # cargo build --release 后装上
pwsh -File scripts/install-local.ps1 -DebugBuild  # 装 debug 构建，反复改代码时省编译时间
pwsh -File scripts/install-local.ps1 -NoBuild     # 只装已有的产物
pwsh -File scripts/install-local.ps1 -Restore     # 把本地安装之前的那份 exe 放回去
```

备份只做一次（`p4delta.exe.p4delta-backup-<时间戳>`）：连装几次本地构建，能还原回去的「原件」
不会被自己的中间产物顶掉；`-Restore` 放回去之后把它删掉，下次本地安装重新捕捉。P4V 正开着时
会自动带 `-Force`。

装完**不用重启 P4V**：工具定义没变，而 Command 写的是绝对路径，P4V 每次点菜单都新起一个进程。
脚本会打印本地构建的版本号和 git 修订号（有未提交改动会标出来）——本地构建与发布版版本号相同，
装的是哪份代码只能靠它分辨。

发布脚本也有黑盒测试，它在一次性 git 仓库里真跑一遍改版本号、提交、打附注 tag、推送，只把 `cargo` 换成垫片：

```bash
bash scripts/test-release.sh
```

部分用例需要机器上有 `p4`（用来读 `.p4ignore`、跑真实 marshal 输出）。缺失时它们会打印一行 `skipping: p4 is not available` 后跳过，而不是静默通过——在 CI 日志里看到这一行，说明那次运行并没有覆盖到这些路径。

### e2e：真实 p4d 沙箱

`tests/e2e_*.rs` 为每个用例起一个独立的 p4d 实例，在真服务器上跑完整流程——八类变更、`--clean` 的三类动作、忽略目录剪枝、client view 排除、字符集、缓存复用。数据库模板只生成一次（`<target>/e2e/template/`），之后每个实例从模板复制，所以单个用例的开销在百毫秒级。

```bash
# 机器上已经有 p4d（比如随 P4V 装的）就能直接跑
cargo test --all-targets --all-features

# 没有的话，下载一份带校验的到 vendor/（CI 走的就是这条路）
bash scripts/fetch-p4-tools.sh
```

探测顺序是 `vendor/` → `<target>/e2e/tools/` → P4V 的安装目录 → `PATH`；一个都找不到时打印一行 `skipping: p4d or p4 is not available` 后跳过，而不是静默通过。`P4D_EXE` / `P4_EXE` 可以直接指定二进制，指定了就以它为准：指到不存在的路径等于强制跳过（想跳过 e2e 只跑单元测试时好用），指到一个跑不起来的文件则直接失败——「这台机器没有 p4d」和「p4d 起不来」是两回事，后者不该被伪装成前者。

这套探测与生产代码的 `src/locate.rs` 是**两套独立的实现**，刻意不合并：这里要的是配套的一对 p4 + p4d（客户端与服务端主版本必须匹配），顺序也相反（生产的 `PATH` 优先）。沙箱只往 `PATH` 里注入，走的是生产探测的第 2 步；`P4_EXE` 在两边含义不同——测试里指错是强制跳过，生产里是配置错误。

**CI 上不允许跳过**：test job 设了 `P4_E2E_REQUIRED=1`，找不到 p4d/p4 时用例直接失败而不是跳过。光有 `fetch-p4-tools.sh` 的硬失败不够——那只保证下载没出错，保证不了二进制真的能用；靠 grep 日志也不行，`skipping:` 走的是 stderr，libtest 默认连同测试输出一起把它捕获了，根本不会出现在 CI 日志里。

沙箱给被测程序的环境里没有任何继承来的 `P4*` 变量（免得连上你自己的服务器），路径形式的摘要缓存也被圈进实例目录。出问题时用 `P4_KEEP_SANDBOX=1` 保留现场：

```bash
P4_KEEP_SANDBOX=1 cargo test --test e2e_open an_edited_file -- --nocapture
```

退出时会把实例目录、端口和复核命令一起打出来。**服务器保持运行**——杀掉的话打印出来的端口就是个死端口，现场也就没法看了；代价是复核完要自己停掉它（命令也在提示里）。

实例目录在 `<target>/e2e/instances/` 下，随 `cargo clean` 一起清掉。数据库模板在 `<target>/e2e/template-<指纹>/`：指纹由 p4d 的身份和种子版本算出，所以模板是**只发布、不修改**的，换了 p4d 或改了种子只会多出一个新目录，旧的留在原地——删掉一个正被别的进程读着的模板，会让它复制到一半就没东西可读，这比多占几十 KB 糟得多。

clean 模式的三类动作（删未跟踪文件、还原改动、写回缺失文件）在 `tests/e2e_clean.rs` 里对着真实服务器验证过，断言同时落在磁盘内容与 `p4 opened` 上，「已打开的文件不归 clean 管」也有一条专门的用例。仍未自动化的是与 `p4 clean -n` 的逐文件对照——拿它的文件集合与 `--clean -l` 的清单比对，三类动作应当一一对应。

## 项目结构

```
src/
  main.rs              bin 薄壳：解析参数、打印错误、返回退出码
  lib.rs               pub fn run()，把各模块串起来
  cli.rs               命令行参数（clap derive）
  charset.rs           p4 输出字符集的解析、缓存与解码
  locate.rs            p4 可执行文件的定位（P4_EXE → PATH → P4V 安装目录）
  model.rs             领域数据模型：depot 记录、工作区文件、摘要缓存
  path.rs              本地路径规范化与路径键
  cache.rs             摘要缓存的落盘（增量保存 + 临时文件改名）
  digest.rs            p4 摘要计算与「自 sync 起未改动」判定
  prune.rs             .p4ignore 分析、预扫描、目录剪枝决策
  workspace.rs         工作区文件收集与忽略过滤
  p4/
    marshal.rs         p4 -G 的 marshal 格式解析
    process.rs         p4 子进程调用与批次的切分、并发
    fstat.rs           p4 fstat 查询与流式解析
  reconcile/
    mod.rs             单个目录的 reconcile 编排
    analyze.rs         两阶段差异分析：每个文件落在哪一类变更（纯逻辑）
    changes.rs         变更分类，以及表驱动的报告与应用
    clean.rs           clean 模式：三类动作的投影、报告与执行
  test_util.rs         跨模块共享的测试基建（仅测试构建）
tests/
  support/             e2e 沙箱框架：p4d 生命周期、数据库模板、环境隔离
  cli.rs               黑盒 CLI 测试（不起服务器，只覆盖参数解析与跳过路径）
  e2e_open.rs          八类变更（11 个用例）
  e2e_clean.rs         --clean 的三类动作（6 个用例）
  e2e_prune.rs         忽略目录剪枝（4 个用例）
  e2e_paths.rs         路径形式 / changelist / 缓存复用 / unmap（4 个用例）
  e2e_charset.rs       非 ASCII 文件名与输出契约（2 个用例）
install.ps1            安装脚本：铺 exe + 注册 P4V 自定义工具
scripts/
  fetch-p4-tools.sh    下载 p4/p4d 到 vendor/（开发与 e2e 用）
  install-local.ps1    构建本地 exe 并装进安装目录，供在 P4V 里验证
  release.sh           发布：改版本号 → 本地门禁 → 提交 → 打 tag → 推送
  test-install.ps1     install.ps1 的黑盒测试
  test-release.sh      release.sh 的黑盒测试
```

依赖方向自下而上、无环：`cli` / `model` / `path` / `charset` / `locate` 不依赖 crate 内其他模块；`p4/*` 依赖它们；`prune` / `workspace` / `cache` / `digest` 再往上一层；`reconcile/*` 在最上面。`lib.rs` 只做模块声明，不反向依赖任何模块。

## 发布检查清单

1. 跑 `bash scripts/release.sh`。**不写版本号就自动升**：默认补丁号 +1（0.1.3 → 0.1.4），
   升次版本号用 `--minor`（0.1.3 → 0.2.0），主版本号用 `--major`（0.1.3 → 1.0.0）；也可以
   直接写死一个，`bash scripts/release.sh 0.2.0`。想先看一遍加 `--dry-run`。

   它把版本号写进三处——`Cargo.toml` 的 `version`、`p4delta.exe.manifest` 的四段程序集版本、
   `Cargo.lock` 里 `p4delta` 的条目——再跑一遍本地门禁（fmt / clippy / test），然后提交、
   打附注 tag、推送。exe 里的 VERSIONINFO 由 `build.rs` 从 `Cargo.toml` 现算，不用管。门禁想
   跳过用 `--skip-check`，只想在本地备好、暂不推送用 `--no-push`。

   推之前它会先检查：工作区干净、当前在 `main` 上、新版本确实比当前大、tag 本地与远端都
   不存在、本地不落后远端。任何一条不满足都当场拒绝——这些正是「推出去才发现」的坑，
   而 tag 推出去就收不回来了（同一版本号不能发两次，自动升号也一样受这条约束）。

   Git Bash 里直接跑就行；PowerShell 与 cmd 里也可以，只要 PATH 里的 `bash` 是 Git 自带的
   那个（`where bash` 的第一条应当是 `...\Git\usr\bin\bash.exe`）。若第一条是
   `C:\Windows\System32\bash.exe`，那是 WSL，脚本会在另一个文件系统视图里跑，不是你要的——
   这种情况用绝对路径调 Git 自带的那个（路径随你的 Git 安装位置，默认在
   `C:\Program Files\Git\bin\bash.exe`）：`& "<Git>\bin\bash.exe" scripts/release.sh`。
2. 剩下的交给 `.github/workflows/release.yml`：复用 CI 当门禁，构建，断言产物里的版本与 tag
   一致、CRT 是静态链接的，打包 zip 与 `SHA256SUMS`，建 release。盯进度用 `gh run watch`。
3. release 建好后，在一台装了 P4V 的机器上核对一遍：从 zip 跑 `install.ps1` → 重启 P4V →
   `Tools > Manage Tools` 里应出现对应条目 → 右键一个无关紧要的目录跑预演确认输出正常。
   生成 XML 与 P4V 自己 `Export tools...` 的差异也在这里对（见 README「已知问题」里两条未经实测的选项）。

