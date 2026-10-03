# p4delta 开发指南

面向要改 p4delta 代码、跑 e2e 沙箱或发版的人。安装与使用见 [README](README.md)。

## 开发与验证

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --all-targets --all-features
```

CI 以 `-D warnings` 为硬门禁，clippy 有任何警告都算失败。

测试跑的是 cargo-nextest 而不是 `cargo test`：cargo 逐个测试二进制**串行**执行；nextest 把所有目标的用例放进同一个调度池，并发度默认等于 CPU 数。装一份：

```powershell
cargo install cargo-nextest --locked     # 或者用 nexte.st 上的预编译包，快得多
```

`.config/nextest.toml` 里配了三条，理由写在文件内注释里：不 fail-fast（一次把失败跑全，而不是首个失败就跳过后面所有）、不重试（不靠重试掩盖 flake）、单用例 180 秒硬超时（nextest 默认只把超时的用例标记为「慢」，不终止进程）。

两点与 `cargo test` 的差别要知道：nextest **不跑 doctest**（本项目目前没有真的 doctest，唯一的文档代码块是 text 类型的；将来真加了要另外补 `cargo test --doc`）；nextest 默认**隐藏通过用例的输出**，`skipping:` 那类提示只在你显式 `--no-capture` 或用例失败时才看得见。

测试分三处：单元测试跟着被测代码放在各模块的 `#[cfg(test)] mod tests` 里，跨模块复用的测试基建在 `src/test_util.rs`；`tests/` 下是黑盒用例，通过进程边界观察，不引用 crate 内部符号；`tests/support/` 是 e2e 沙箱框架。

安装脚本另有黑盒测试，不需要构建产物（它用一个人造 exe 复刻「从 release 包解压出来直接跑」的布局）：

```powershell
pwsh -File scripts/test-install.ps1        # 或 powershell -File ...
```

CI 用 Windows PowerShell 5.1 与 PowerShell 7 各跑一遍。两个脚本都**必须带 UTF-8 BOM**：5.1 对没有 BOM 的文件按系统 ANSI 代码页解析，里面的中文会全变乱码（`test-install.ps1` 有一条断言拦着）。

### 预览性能测量（PowerShell 7）

`scripts/benchmark.ps1` 固定执行预览（`-l`，不传 `-a`），先预热一次，再计时 5 轮：

```powershell
cargo build --release --locked
pwsh -File scripts/benchmark.ps1 -Binary ./target/release/p4delta.exe `
    -Workspace <client> -Path <本地目录> -OutDir <扫描范围外的新输出目录>
# 可选：-Mode clean，或 -Mode sync -To <CL> -VerifyAll
# 剪枝对照：相同场景另跑一份 -NoPruneIgnoredDirs
pwsh -File scripts/test-benchmark.ps1
```

每次输出到新目录，保留原始 stdout/stderr 与 `result.json`；预热与计时轮的动作和完整路径按区分
大小写的多重集比较，乱序不算差异，重复数量参与比较。输出目录与测量目录不得互相包含，避免
基准日志污染扫描。遇到转交原生 P4 的不支持文件时，因清单不完整而拒绝报告成功的基准结果。
对照不同构建或剪枝开关时，先核对动作一致，再比较多轮耗时，不能只比最快的一轮。

脚本不清除摘要缓存，也不控制操作系统文件缓存；正常档是自然预热后的重复测量，`-VerifyAll`
仍每轮绕过摘要缓存与时间戳捷径，不是“冷操作系统缓存”测量。峰值工作集只覆盖主进程，
不包含 P4 子进程；退出后无法取得有效峰值时，使用运行期间每 50 ms 刷新采样所得的下界，
始终取不到有效值则记 `null`。目前没有分阶段耗时、服务端负载或进程树峰值统计，也不在 CI
里设耗时硬阈值。脚本测试使用假 CLI 与模拟进程对象，不访问真实 P4。

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

部分用例需要机器上有 `p4`（用来读 `.p4ignore`、跑真实 marshal 输出）。缺失时它们会输出 `skipping: p4 is not available` 后跳过，但通过用例的输出默认被捕获，不能靠 CI 日志里没有这行来判断覆盖完整。CI 设置 `P4_E2E_REQUIRED=1`，让缺少工具直接失败。

### e2e：真实 p4d 沙箱

`tests/e2e_*.rs` 为每个用例起一个独立的 p4d 实例，在真服务器上跑完整流程——八类变更、`--clean` 的三类动作、`--sync` 的四组动作、忽略目录剪枝、client view 排除、字符集、缓存复用。数据库模板只生成一次（`<target>/e2e/template-<指纹>/`），之后每个实例从模板复制，所以单个用例的开销在百毫秒级。

```bash
# 机器上已经有 p4d（比如随 P4V 装的）就能直接跑
cargo nextest run --all-targets --all-features

# 没有的话，下载一份带校验的到 vendor/（CI 走的就是这条路）
bash scripts/fetch-p4-tools.sh
```

探测顺序是 `vendor/` → `<target>/e2e/tools/` → P4V 的安装目录 → `PATH`；一个都找不到时打印一行 `skipping: p4d or p4 is not available` 后跳过，而不是静默通过。`P4D_EXE` / `P4_EXE` 可以直接指定二进制，指定了就以它为准：指到不存在的路径等于强制跳过（想跳过 e2e 只跑单元测试时好用），指到一个跑不起来的文件则直接失败——「这台机器没有 p4d」和「p4d 起不来」是两回事，后者不该被伪装成前者。

这套探测与生产代码的 `src/locate.rs` 是**两套独立的实现**，刻意不合并：这里要的是配套的一对 p4 + p4d（客户端与服务端主版本必须匹配），顺序也相反（生产的 `PATH` 优先）。沙箱只往 `PATH` 里注入，走的是生产探测的第 2 步；`P4_EXE` 在两边含义不同——测试里指错是强制跳过，生产里是配置错误。

**CI 上不允许跳过**：test job 设了 `P4_E2E_REQUIRED=1`，找不到 p4d/p4 时用例直接失败而不是跳过。光有 `fetch-p4-tools.sh` 的硬失败不够——那只保证下载没出错，保证不了二进制真的能用；靠 grep 日志也不行，`skipping:` 走的是 stderr，libtest 默认连同测试输出一起把它捕获了，根本不会出现在 CI 日志里。

沙箱给被测程序的环境里没有任何继承来的 `P4*` 变量（免得连上你自己的服务器），路径形式的摘要缓存也被圈进实例目录。出问题时用 `P4_KEEP_SANDBOX=1` 保留现场：

```bash
P4_KEEP_SANDBOX=1 cargo nextest run -E 'test(an_edited_file)' --no-capture
```

`-E 'test(...)'` 是按用例名过滤的 filterset（子串匹配，不绑模块路径）；`--no-capture` 让 nextest 串行执行并原样透传输出——保留现场的那几行提示走的是 stderr，不关掉捕获就看不到。

退出时会把实例目录、端口和复核命令一起打出来。**服务器保持运行**——杀掉的话打印出来的端口就是个死端口，现场也就没法看了；代价是复核完要自己停掉它（命令也在提示里）。

Windows 上另有一条：**别把这条命令的输出接进管道**（`| tee`、`| tail` 之类）。保留下来的 p4d 会继承管道的写端句柄，它不退出，管道就永远读不到 EOF，命令一直挂着——看起来像测试卡死，其实进程早就跑完了。要留日志就重定向到文件（`> log.txt`）。

实例目录在 `<target>/e2e/instances/` 下，随 `cargo clean` 一起清掉。数据库模板在 `<target>/e2e/template-<指纹>/`：指纹由 p4d 的身份和种子版本算出，所以模板是**只发布、不修改**的，换了 p4d 或改了种子只会多出一个新目录，旧的留在原地——删掉一个正被别的进程读着的模板，会让它复制到一半就没东西可读，这比多占几十 KB 糟得多。

nextest 是 process-per-test，与「逐个二进制串行」的 cargo 不同：冷缓存时（`cargo clean` 之后，或换了 p4d、改了种子）多个进程会同时走到 `ensure_template` 的「各建各的 staging、抢 rename」那条路径上。正确性由 rename 的原子性保证，模板只可能被发布一次；代价是种子会被重复跑几遍，只影响那一次冷跑。热缓存下是一次毫秒级的 stamp 命中。

clean 模式的三类动作（删未跟踪文件、还原改动、写回缺失文件）在 `tests/e2e_clean.rs` 里对着真实服务器验证过，断言同时落在磁盘内容与 `p4 opened` 上，「已打开的文件不归 clean 管」也有一条专门的用例。另有一条「head 已删除、本地文件重现」的用例，用 `p4 clean -n` 的删除判定交叉验证；三类动作与原生清单的逐文件集合对照尚未自动化，不应把该个案当成完整对照覆盖。

sync 模式的四组动作在 `tests/e2e_sync.rs` 里同样对着真实服务器验证：`--to <CL>` 那条用 `p4 fstat` 的 `haveRev` 独立取证（不看工具自己的 stdout），另有两条把「默认档会漏、`--verify-all` 才抓得住」和「原生 `p4 sync -f -n` 里非 `refreshing` 的文件必须被我们的清单覆盖」都做成断言。删除组那两条腿的保证（一条腿失败不让另一条不跑）由 `both_delete_legs_report_their_own_failure` 守着：它把文件摁住让删除被系统拒绝，断言两条腿的失败都出现在错误信息里——改造前这条会红，因为前一条腿的 `?` 会让后一条腿整个不执行。

`--to` 的两个边界也纳入了沙箱自动化：**目标 CL 早于限定子目录首次提交时拒绝执行**，并断言内容、have 和 opened 不变；**超过 head 的 CL 回落到 head**，通过读回 `haveRev` 与内容确认没有误触发空目标护栏。目标号都从沙箱已提交记录取得，不依赖固定种子 CL。另有 have < 目标 < head 的前向同步用例，确认不会越过指定目标。时间戳捷径的用例在修改内容后恢复实际同步时的 mtime，不再依赖测试执行时的当前时钟。

删除组还有一条实测值得记：`p4 sync -f //depot/f#none` 面对**被别的进程占着的文件**会重试约十秒才放弃，而且它会先把 `deleted as` 打到 stdout、把 `unlink: ...` 写到 stderr，**have 记录原样保留**——「看起来成功、其实什么都没做」的典型。这正是那一组的判据必须是 `ExitCodeOrStderr` 而不能是 `ExitCode` 的原因（后者会让它整个隐形），也是 `both_delete_legs_report_their_own_failure` 那条用例要跑十来秒的原因。

真实大工作区上的 A/B 也跑过一轮（客户端的 `Source\Client\Config` 589 文件、`Source\Script\QAScript` 44,554 文件 5.09 GiB）：前者原生 10 个 `updating`、我们 10 个 `Update`，文件集合逐条相同。`--to <CL>` 换成一个能真正区分 head 与目标的目标 CL 后（8984916，那里 `DefaultEngine.ini` 是 `#34`、head 已是 `#35`），两边仍是同样的那 10 个文件。后者整轮 dry run 2.54 s（摘要缓存全命中）。**那里出现过一次分歧**：原生打了 22 行 `deleted as` 而我们说「无事可做」——逐条核实后那 22 个既不在磁盘上也没有 have 记录，原生那几行只是描述目标状态、p4 自己也无事可做，结论钉在 `a_deleted_target_with_nothing_local_is_a_no_op` 里。

这一轮是**一次性的人工核对**，没做成自动化（要连真实的 `songxiao_aki_branch_3.8_2`，CI 上跑不了），而且全程 dry run——`-a` 会动到那份真实工作区。所以它验证的是「取数、分类、集合」三者与原生一致；**「钉住的确实是目标那一版（`#34` 而非 `#35`）」只有沙箱用例有独立取证**（`p4 fstat` 读回的 `haveRev`），工具自己的输出里不打印目标版本号。

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
  cache.rs             摘要缓存的阶段间保存（全量序列化 + 临时文件改名）
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
    sync.rs            sync 模式：四类动作的投影、报告与执行（目标版本，默认 head）
  test_util.rs         跨模块共享的测试基建（仅测试构建）
tests/
  support/             e2e 沙箱框架：p4d 生命周期、数据库模板、环境隔离
  cli.rs               黑盒 CLI 测试（不起服务器，只覆盖参数解析与路径参数的校验）
  e2e_open.rs          八类变更
  e2e_clean.rs         --clean 的三类动作与已打开文件的保护
  e2e_sync.rs          --sync 的四组动作、--to <CL> 与 --verify-all
  e2e_prune.rs         忽略目录剪枝
  e2e_paths.rs         路径形式 / changelist / 缓存复用 / unmap
  e2e_charset.rs       非 ASCII 文件名与输出契约
install.ps1            安装脚本：铺 exe + 注册 P4V 自定义工具
scripts/
  benchmark.ps1        仅预览的多轮计时、动作多重集比对与日志留存（PowerShell 7）
  test-benchmark.ps1   测量脚本的假 CLI 黑盒测试
  fetch-p4-tools.sh    下载 p4/p4d 到 vendor/（开发与 e2e 用）
  install-local.ps1    构建本地 exe 并装进安装目录，供在 P4V 里验证
  release.sh           发布：改版本号 → 本地门禁 → 提交 → 打 tag → 推送
  test-install.ps1     install.ps1 的黑盒测试
  test-release.sh      release.sh 的黑盒测试
```

阅读时可从 `model` / `path` 等基础模块入手，再看 `p4/*` 的查询和进程管理、`workspace` / `cache` / `digest` 的扫描与摘要，最后看 `reconcile/*` 的分类和动作。`lib.rs` 不仅声明模块，还负责编排参数、缓存生命周期与逐目录执行；`main.rs` 才是薄入口。

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

   还要核对 sync 那两条入口各自出现在哪个菜单、命令行拼出来对不对：在 **History 视图**右键
   一个已提交的 changelist，`p4delta Sync to changelist` 应当出现（预演输出里的 `--to` 就是
   那一行的号），点它应当弹出问目录的输入框；在 **Workspace / Depot 树**里右键一个目录，
   `p4delta Sync this folder to changelist` 应当出现，点它问 changelist 号。

   这一段 **2026-10-03 已在本机完整对过**：我们手写的节点在 P4V 重写 `customtools.xml` 时原样
   保留（与 P4V 自己 `Export tools...` 的输出比对差异为零），两条 sync 入口都出现在预期位置、
   prompt 留空以退出码 1 报 `No path given`、输出进 P4V 自己的输出窗。之后凡是动过
   `install.ps1` 里元素形状或 Arguments 的改动，这一段就再走一遍。同样要留意 README 里那条：
   团队那个 `RunTaskAndSyncFiles.bat` 会拿 depot 的 `tools.xml` 覆盖 `customtools.xml`，跑过
   一次它之后 p4delta 的七条就全没了，别把"菜单里没有"误判成工具定义写错了。

