# p4delta

P4V 里的 `Reconcile Offline Work` 慢得令人抓狂，本程序是它的替代实现。它会从 Helix Core 服务器取回 depot 状态、扫描本地工作区、计算并缓存文件摘要，比对两者后更新指定的待提交 changelist。通常比原工具快 10–100 倍。

加 `--clean` 时它反向工作：不拿工作区去更新 depot，而是**用 depot 修正工作区**，等价于 `p4 clean`（`p4 reconcile -w`）。

## 性能测试

比对一个无改动的小项目（14k 文件，50GB）：

- `Reconcile Offline Work` 用 15 秒
- `p4delta` 无摘要缓存用 4.5 秒
- `p4delta` 有摘要缓存用 1 秒

比对一个无改动的大型 Unreal Engine 项目（450k 文件，250GB）：

- `Reconcile Offline Work` 用 360 秒
- `p4delta` 无摘要缓存用 55 秒
- `p4delta` 有摘要缓存用 14 秒

比对二进制 Unreal Engine 构建的一次版本升级（较早的测试，体积未知）：

- `Reconcile Offline Work` 跑约 2 小时后被取消
- `p4delta` 无摘要缓存用约 30 秒

`Reconcile Offline Work` 的性能随改动文件数增加会迅速劣化，`p4delta` 不会——它始终更快、更跟手。

## 安装

从 [Releases](https://github.com/lovebirdsx/p4delta/releases) 下载最新的
`p4delta-<版本>-x86_64-pc-windows-msvc.zip`，解压后在这个目录里跑：

```powershell
.\install.ps1
```

它做三件事：把 `p4delta.exe` 铺到 `%LOCALAPPDATA%\Programs\p4delta\`、把工具定义写进
`%USERPROFILE%\.p4qt\customtools.xml`（P4V 的自定义工具文件）、检查机器上有没有 `p4`。
**装完要重启 P4V**——它只在启动时读那个文件。

写入是幂等的：只动它自己那几个节点，你已有的其它自定义工具原样保留；内容没变时整个文件都不
重写。写之前会备份一份（`customtools.xml.p4delta-backup-<时间戳>`）。P4V 正在运行时会拒绝
写入（`-Force` 可以强行继续）——P4V 退出时可能用它内存里的列表覆盖这次改动。

默认注册三个：`p4delta Reconcile` 与 `p4delta Clean (preview)` 都进右键菜单，不可逆的
`p4delta Clean (APPLY - irreversible)` 单独放在 `p4delta (irreversible)` 子菜单里，免得和安全的
那个挨着被误点。不想要它就在安装时加 `-WithoutCleanApply`：既不注册，以前注册过的也会被
摘掉（只认自己那几个节点的名字，别人的工具不碰）。

| 参数 | 作用 |
| --- | --- |
| `-WithoutCleanApply` | 不注册不可逆的 clean 实际清理；已经注册过的会被摘掉（默认注册，放在单独的子菜单里） |
| `-Uninstall` | 卸载：摘掉工具定义、删安装目录；摘要缓存留着 |
| `-AddToPath` | 把安装目录加进用户级 PATH，方便在命令行里直接敲 `p4delta` |
| `-InstallDir`、`-CustomToolsPath` | 换安装位置 / 换 P4V 配置文件位置 |
| `-ExePath` | 装指定路径的那份 exe（默认取与本脚本同目录的），自编译的产物走这里 |
| `-WhatIf` | 只说要做什么，不落盘 |

### 未签名的 exe

发布产物没有代码签名，首次运行可能出现 SmartScreen 的「Windows 已保护你的电脑」，点「更多信息」
→「仍要运行」即可。想先核对再运行的话，下载目录里有 `SHA256SUMS`：

```powershell
Get-FileHash .\p4delta-<版本>-x86_64-pc-windows-msvc.zip -Algorithm SHA256
```

## 用法

程序作为 P4V 的自定义工具运行。安装脚本已经把它注册好了——重启 P4V，在工作区里右键一个目录，
选菜单里的条目：

- `p4delta Reconcile`：默认模式，等价于依次执行 "Reconcile Offline Work" 和 "Revert Unchanged"。
- `p4delta Clean (preview)`：clean 模式的预演，只打印不动作。
- `p4delta Clean (APPLY - irreversible)`：clean 模式的实际清理，**不可逆**，先看「clean 模式」
  一节。默认注册在 `p4delta (irreversible)` 子菜单里；不想要就在装的时候加 `-WithoutCleanApply`。

去掉 `-a` 就是预演（dry run）：照常扫描比对并打印结果，但不向 p4 应用任何变更。

### 手工安装（备选）

不用安装脚本的话，在 P4V 里打开 `Tools > Manage Custom Tools`，新建一个 Local Tool，按下表填写：

| 字段 | 值 |
| --- | --- |
| Name | `p4delta`（只是菜单里的显示名，取什么都行） |
| Placement | `Custom Tools` |
| Application | `p4delta.exe` 的完整路径（安装脚本用的是 `%LOCALAPPDATA%\Programs\p4delta\p4delta.exe`） |
| Arguments | `-a -w $c -l %D` |
| Start in | `$r` |
| Add to applicable context menus | 勾选 |
| Run tool in terminal window | 勾选 |
| Refresh Helix P4V upon completion | 勾选 |
| Prompt for arguments、Close window upon completion、Add file browser to prompt dialog、Ignore P4CONFIG files | 不勾 |

Arguments 里的 `$c` 是 P4V 展开的当前 workspace，`%D` 是右键选中的目录，`$r` 是 client 根目录，原样照抄即可。

想从 P4V 里清理工作区的话，再建一个自定义工具：Arguments 填 `--clean -w $c -l %D` 做预演，
填 `-a --clean -w $c -l %D` 做实际清理。它的动作**不可逆**，先看「clean 模式」一节。
安装脚本会把实际清理那条放进单独的 `p4delta (irreversible)` 文件夹，手工建的话也建议这么摆。

配置完成后，在工作区里右键一个目录，选最下面的这个自定义工具。

### p4 从哪来

工具要调用 `p4`（命令行客户端），按这个顺序找：

1. `P4_EXE` 环境变量。设了就以它为准；指向不存在的文件是配置错误，直接报错，不会回退到别的 p4。
2. `PATH`。
3. P4V 的安装目录（`%ProgramFiles%\Perforce`、`%ProgramFiles%\Perforce\DVCS`，x86 同理）。

第 3 条是必要的兜底：P4V 的安装器把命令行客户端列为**可选组件**，没勾那一项的机器上 `PATH`
里没有 p4。找不到时的报错会列出找过哪些地方。

> `P4_EXE` 在 e2e 测试里还有另一层含义（指到不存在的路径等于强制跳过），与生产代码不同，
> 见「开发与验证」。

### 非 ASCII 文件名

p4 返回的文件名使用客户端配置的字符集：unicode 模式的服务器上是 `utf8`，旧式服务器上则是 `cp936`、`shiftjis` 之类。工具必须用同一个字符集解码这些输出，否则含非 ASCII 字符的名字无法与本地文件系统报告的路径对上——每个这样的文件都会被报告两次，一次当作新增、一次当作删除。若真的应用这些变更，就会对一个已经存在的文件执行 `p4 add`，并对一个并不存在的路径执行 `p4 delete`。

字符集按以下顺序自动探测：

1. `--charset` 参数，也接受 p4 自己的写法（`utf8`、`cp936`、`shiftjis`……）。
2. `P4CHARSET` 环境变量。
3. `p4 set` 里的 `P4CHARSET`，其次是端口级的 `P4_<port>_CHARSET`。
4. UTF-8。

如果工作区里有非 ASCII 名字而探测失败，请显式设置 `--charset`——在 P4V 里把它追加到 Arguments 字段，例如 `-w $c -l %D --charset cp936`。设错是可见的而不是静默的：日志开头会打印 `Using p4 charset ...`，字符集不匹配会表现为 `-l` 输出里的乱码文件名。

判断一个文件是否被忽略要走 `p4 ignores`，而这个命令只认挂在命令行上的路径。Windows 会把命令行按系统 ANSI 代码页转一次字节，代码页表示不了的名字在那一步变成 `?`，p4 会因此报 `Argument parsing ambiguity.` 并让**整批**查询一起作废。工具于是把这类名字排除在忽略查询之外：它们不会被当成被忽略的文件，会照常出现在变更清单里（同一批里的 ASCII 名字不受影响）。真要应用时，`p4 add` 会对这些名字报「拒绝忽略文件」。

### 路径与忽略目录优化

Windows 本地目录参数支持正斜杠、反斜杠和混合分隔符，匹配时统一为本地路径键；depot 路径仍使用 P4 的 `//depot/...` 语法。

工具会尝试提前剪枝被 P4 忽略的目录，以减少文件 metadata 查询和 `p4 ignores` 子进程数。当前仅在目标目录的有效配置为 `P4IGNORE=.p4ignore`、没有嵌套 `.p4ignore`、生效规则不含 `!` 重新包含规则且查询结果可可靠识别时启用。其他情况回退到完整扫描和逐文件过滤，日志会说明原因。目录是否被忽略仍由 P4 判断，不硬编码忽略 `node_modules` 或 `.git`；被剪目录内若有 depot 已跟踪文件，会重新扫描，避免误报删除。

可添加 `--no-prune-ignored-dirs` 禁用目录剪枝，进行结果和性能对照：

```powershell
p4delta -w "your-workspace" -l --no-prune-ignored-dirs "E:\project\src"
```

旧版本文件的元数据补查使用 `p4 fstat -L`；补查失败或记录不完整时会停止，不会用最新版本的摘要替代本地 have revision 的摘要。

client view 里的排除行（例如 `-//aki/....tmp`）会让 p4 完全看不见那些路径：`p4 reconcile`、`p4 clean`、`p4 add` 对它们一律报 `not in client view`。工具是自己扫盘的，所以对判定为新增的文件会再查一次 `p4 where`，把被排除的路径剔除——否则 open 模式会去 add 一个 p4 拒绝的文件，clean 模式更糟：会删掉一个 p4 根本不管的文件。

不加 `--apply`（`-a`）时不会应用 P4 变更，但仍可能更新工具自身的本地 digest 缓存。首次路径规范化后，部分旧缓存可能无法命中；请以预览结果一致性及多轮计时评估性能，目录结构不同，收益也会不同。

## 会修正的不一致

下列八类是**默认（open）模式**的行为。`--clean` 只取其中三类，见「clean 模式」。

- 工作区中存在、depot 中没有或 have revision 已删除，但没有签出为 add 的文件 → 新增
- 工作区中相对 have revision 有改动，但没有签出为 edit 的文件 → 编辑
- 工作区中相对 have revision 有改动，却签出为 delete 的文件 → revert 后编辑
- 不在工作区中，但没有签出为 delete 的文件 → 删除
- 不在工作区中，却签出为 edit 的文件 → revert 后删除
- 不在工作区中，却签出为 add 的文件 → revert
- 在工作区中、相对 have revision 无改动，却签出为 edit 的文件 → revert
- 在工作区中、相对 have revision 无改动，却签出为 delete 的文件 → revert

等价于依次执行 "Reconcile Offline Work" 和 "Revert Unchanged"。

少数文件没法在这里比对，会转交原生的 `p4 reconcile` 处理（带 `-a` 时真正下发，否则下发
`p4 reconcile -n` 预演）：Apple/Resource 旧格式，以及 `headType` 认不出来的文件。`--clean`
下这两类转交的是 `p4 clean`（预演用 `p4 clean -n`）——方向相反的两条命令对同一批文件给出的
预告也是相反的，用错方向比没有预告更糟。head revision 是归档版本（archived）的文件则跳过并
汇报——它们的内容在 archive depot 里，本地没有可比的副本。

## clean 模式（`p4 clean` 对等）

`--clean` 让工具反向工作，等价于 `p4 clean`（即 `p4 reconcile -w`）：不把工作区的改动报告给
depot，而是把工作区修正到与 depot 一致。它对三类**未打开**的文件动手：

- 工作区中存在、depot 中没有的文件 → **从工作区删除**
- 相对 have revision 有改动、没有签出为 edit 的文件 → **还原成上次 sync 的版本**
- 不在工作区中、没有签出为 delete 的文件 → **从 depot 写回上次 sync 的版本**

其余五类都是已打开的文件。`p4 clean` 完全不碰它们，工具同样既不动作也不汇报。

第一类里的「head revision 已被删除、本地文件又冒出来」是同一个动作。这个状态通常得手工造：
`p4 sync` 到删除版本会把本地文件一并删掉。工具把它归为**从工作区删除**，与 `p4 reconcile`
会 open for add 正好相反；这一点与 `p4 clean` 自己的判定一致——两边都认成 `#none`，
都报告要 `deleted as` 本地那一份。

> **clean 不可逆。** 它会删掉 depot 里没有的文件，并丢弃未打开文件上的本地改动，两者都没法
> 用 p4 找回来。别在没跑过预演的情况下直接加 `-a`。

和默认模式一样，不带 `-a` 时只统计、不动作。其余差异：

- `-c` / `--changelist` 没有意义（clean 不打开任何文件），给出时会告警并忽略。
- `p4 clean -K`（抑制 ktext 关键字展开）没有对应开关，工具始终按 p4 的默认行为展开关键字。
- 需要的是 `read` 权限，而不是 `open` 权限。

## 退出码

| 码 | 含义 |
| --- | --- |
| `0` | 成功。包括预演，以及「有差异但没带 `-a`」的情况。 |
| `1` | 运行错误。stderr 输出单行 `error: <原因链>`。 |
| `2` | 用法错误，例如未知参数。由参数解析阶段给出。 |

`--clean -a` 下如果有文件删不掉（被别的进程占着、权限不对），同样是退出码 `1`。删除组内部会
把该删的都试一遍，失败的文件逐个列在错误信息里；但**后面两组不会执行**——三类动作按「先删、
再还原、最后写回」的顺序做，某一组报错就中止整轮，剩下的留给下次运行。

> 退出码 `1` 的错误格式与 0.1.2 及更早版本不同：旧版是 anyhow 的多行
> `Error: ...` / `Caused by: ...`，现在压成一行。写脚本解析 stderr 时需要留意。

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
   生成 XML 与 P4V 自己 `Export tools...` 的差异也在这里对（见「已知问题」里两条未经实测的选项）。

## 已知问题

它只在我们的仓库上验证过，如果你的配置与我们不同，未必能正常工作。
请先用预演模式确认行为符合预期。你对仓库所做的改动由你自己负责。

- `p4delta` 不实现 move/add 与 move/delete 的配对识别。
- `--clean` 不还原 head revision 是归档版本的文件，只跳过并汇报；Apple/Resource 与认不出的 `headType` 则转交 `p4 clean`。
- `p4 clean -K`（抑制 ktext 关键字展开）没有暴露，clean 始终按 p4 的默认行为展开关键字。
- `--clean` 与 `-c` 同时给出时 `-c` 被忽略（会打印一行告警）。
- depot 路径里含 `#`、`%`、`@`、`*`，或以 `...` 结尾的文件名会被 p4 当成通配符解析，既有的转交路径和 clean 的还原规格都受影响。
- p4 对**逐文件**错误（protections 拒绝 open、路径不在 client view）返回的退出码是 **0**，错误只出现在 stderr 上。
  工具因此把改状态命令（add / edit / delete / revert / sync）的 stderr 非空也判为失败，p4 的原话会进错误信息——
  「p4 一个文件都没接受、工具却报 `Inconsistencies fixed.`」不会再发生。反过来说，如果你的 p4 在**成功**时
  也往 stderr 写提示，那一轮会被判失败（`Failed to apply N change group(s)`），把那条提示加进 `FailureMode`
  的例外即可。转交 `p4 reconcile` / `p4 clean` 的兜底路径不受这条影响：p4 对「无事可做」也写 stderr。
- 安装脚本写出的工具定义里，**「Run tool in terminal window」与「Ignore P4CONFIG files」两项没有对应元素**——
  它们在 P4V 自定义工具 XML 里的元素名没查到官方文档，也没实测过，所以没有臆造。不勾「终端窗口」时
  输出会进 P4V 自己的输出窗格，工具照常可用；确实需要这两项的话，在 `Manage Tools` 里手工补勾一次并
  `Export tools...`，对照生成的文件把元素名补进 `install.ps1`。
- 安装路径里含空格时（用户名带空格就会），`<Command>` 能否被 P4V 正确解析尚未实测。安装脚本会在
  这时打印警告；真出问题就用 `-InstallDir` 换一个不含空格的目录。
