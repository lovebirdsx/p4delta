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
> 见 [CONTRIBUTING.md](CONTRIBUTING.md) 的「开发与验证」。

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

## 开发

想改代码、跑 e2e 沙箱或发版：见 [CONTRIBUTING.md](CONTRIBUTING.md)。
