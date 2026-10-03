# p4delta

P4V 里的 `Reconcile Offline Work` 慢得令人抓狂，本程序是它的替代实现。它会从 Helix Core 服务器取回 depot 状态、扫描本地工作区、计算并缓存文件摘要，比对两者后更新指定的待提交 changelist。通常比原工具快 10–100 倍。

加 `--clean` 时它反向工作：不拿工作区去更新 depot，而是**用 depot 修正工作区**，等价于 `p4 clean`（`p4 reconcile -w`）。

加 `--sync` 时它把工作区拉到目标 depot 版本（默认 head，`--to <CL>` 指定 changelist），等价于「只传真正需要传的文件」的 `p4 sync -f`。

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

这些数字背后的机制——哪些是原生没做、哪些是它算得快——见 [docs/why-faster.md](docs/why-faster.md)。

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

默认注册七个。进右键菜单的四条：`p4delta Reconcile`、`p4delta Clean (preview)`、
`p4delta Sync to changelist (preview)`、`p4delta Sync this folder to changelist (preview)`；
三条不可逆的（`... Clean (APPLY - irreversible)`，以及两条 sync 入口各自的
`... (APPLY - irreversible)`）一起放在 `p4delta (irreversible)` 子菜单里，免得和安全的那些挨着
被误点。不想要哪条就在安装时加对应的开关：既不注册，以前注册过的也会被摘掉（只认自己那几个
节点的名字，别人的工具不碰）。

| 参数 | 作用 |
| --- | --- |
| `-WithoutCleanApply` | 不注册不可逆的 clean 实际清理；已经注册过的会被摘掉（默认注册，放在不可逆子菜单里） |
| `-WithoutSyncApply` | 不注册不可逆的 sync 实际执行（两条入口各一条）；同样默认注册、可摘掉 |
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
- `p4delta Sync to changelist (preview)`：把你**指定的那个目录**拉到**右键那一行的 changelist**。
  这一条只在 P4V 的 History 视图里、对着**已提交**的 changelist 出现：changelist 自动带上，
  弹框问目录。
- `p4delta Sync this folder to changelist (preview)`：把**右键选中的那个目录**拉到你**填的
  changelist**。这一条在 Workspace / Depot 树里右键时出现：目录自动带上，弹框问 changelist 号。
- `... Clean (APPLY - irreversible)` 与两条 `... Sync ... (APPLY - irreversible)`：
  上面几条的实际执行版，**不可逆**。它们注册在 `p4delta (irreversible)` 子菜单里；不想要哪条
  就在装的时候加 `-WithoutCleanApply` / `-WithoutSyncApply`（后者管两条）。

为什么两条 sync 入口都得手填一半，见下面「从 History 视图同步到某个 changelist」。

sync 模式的 head 版（不带 `--to`）没有注册进 P4V——它的动作同样会覆盖本地改动。要手工建的话，
Arguments 填 `--sync -w $c -l %D`，先读「sync 模式」一节。

去掉 `-a` 就是预演（dry run）：照常扫描比对并打印结果，但不向 p4 应用任何变更。

### 从 History 视图同步到某个 changelist

在 History 视图里右键一个已提交的 changelist，选 `p4delta Sync to changelist`：那个号直接进了
`--sync --to <CL>`，工作区被拉回那一刻的状态。弹框问的是**目录**——扫描与还原的范围就是它，
不是整个工作区。从 History 的路径栏复制即可，本地路径（`E:\aki\...`）与 depot 路径
（`//aki/...`）都收。

反过来，在 Workspace 或 Depot 树里右键一个目录，选 `p4delta Sync this folder to changelist`：
目录就是右键的那一个，弹框问 changelist 号。

#### 为什么不能两个都自动

P4V 有两类实测撞上的硬限制（多轮探针工具在真实 P4V 上跑出来的结论）：

- **一个工具定义只能有一个 `%` 参数。** 写 `%D ... %S` 保存得了，一运行就弹
  `More than one replaceable file argument of type %X is not allowed`，没有侥幸空间。
- **History 视图里拿不到目录。** 在那里右键时选中的是一行 changelist 而不是文件，`%D` 与
  `%f`（选中文件所在的目录）**都**是空的，只有 `%S` 有值。P4V 只在变量取得到值的上下文显示
  工具，所以带 `%D`/`%f` 的工具在文件夹历史里根本不出现——手写的定义如此，让 P4V 自己从 GUI
  建一个同形状的也一样。反过来，只有 `%S` 的工具在普通目录右键里也不出现（那里没有
  changelist）。

四个右键位置实测下来的可见性：

| 右键的位置 | `%D` | `%f` | 会出现的工具 |
| --- | --- | --- | --- |
| History · 一行 changelist | 空 | 空 | 只有带 `%S` 的 |
| 树 · 目录 | 本地路径 | 空 | 带 `%D` 的 |
| 树 · 文件 | 本地路径 | 文件所在目录 | `%D`/`%f` 的都会出现 |
| 文件历史 · 一行 | 该文件的 depot 路径 | 空 | 带 `%D` 或 `%S` 的 |

于是一个目录、一个 changelist，两个入口各自永远要你手给一样：**History 那条自动 changelist、
问目录；树里那条自动目录、问 changelist。** 看着 changelist 点就用 History 那条，看着目录点
就用树里那条。**这也意味着没有哪条工具会去动整个工作区**——范围永远是你明确给的那个目录。

树里那条问 changelist 号时**建议粘贴、不要手打**：打错的号（尤其是打成比目标小的号）会让
`--to` 静默地多做删除，粘贴那一行的号最省事也最不容易错。

两个选择都不该被"统一"成别的写法：

- **`%S` 而不是 `%c`**。`%S` 是 P4V 的 "Selected submitted changelists"，只对**已提交**的
  changelist 有值，所以这条工具在 Pending 视图里不触发。`%c`（"Selected changelists"）在那边
  同样有值，会把一个 pending changelist 的号喂给 `--to`——而 changelist 号是**创建**时分配的，
  pending 号完全可能小于当前 head，那一下就是「退回历史版本并删掉之后新建的文件」，与点它的
  人的意图正好相反。
- **两条入口分开而不是合成一条**。在**文件**的历史里右键时 `%D` 是那个文件的 depot 路径，
  p4delta 只认目录，`Sync this folder` 那条会明确报 `is not a directory` 并以退出码 1 结束
  （不会"顺手"用父目录把范围放大）。文件历史里请用 `Sync to changelist` 那条，目录填该文件
  所在的文件夹。

一次只选一个 changelist。多选时 `%S` 会把所有号一起传进来，而 `--to` 只吃第一个，其余那些号
会被当成路径参数，输出里出现一行 `Warning: skipped N path(s) that cannot be worked on`——
也就意味着只有第一个生效。

> **这一条会删本地文件**：目标 changelist 之后新建的那些都在删除之列，且删掉的内容无法用
> `sync @CL` 找回。动手前先用预演那条看 `-l` 列出的清单。判定细节见「sync 模式」一节。

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

想从 History 视图同步到某个 changelist 的话，同样再建两条：Arguments 填
`--sync -w $c -l $D --to %S` 做预演，填 `-a --sync -w $c -l $D --to %S` 做实际执行，并且
**勾上 "Prompt for arguments"**、Prompt Text 填「要同步哪个目录？……」——`$D` 就是那个输入框
里填的内容。想从工作区树同步某个目录的话，再建两条：`--sync -w $c -l %D --to $D` 与
`-a --sync -w $c -l %D --to $D`，同样勾 Prompt、Prompt Text 填「要同步到哪个 changelist？……」。

每条 sync 工具都得勾 Prompt 才有用：目录与 changelist 只能自动一个，另一个不勾就是空的。
为什么不能两个都自动，见上面「从 History 视图同步到某个 changelist」一节。

**不要**顺手把 "Add file browser to prompt dialog" 也勾上：实测那个浏览按钮只能选**文件**，
而这四条问的要么是目录、要么是 changelist 号——勾了只会把人往「选个文件、工具报不是目录」
上引。（安装脚本写出来的定义里没有这个元素，就是为这个。）

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

下列八类是**默认（open）模式**的行为。`--clean` 只取其中三类，见「clean 模式」。`--sync` 是
另一套判据——它对着目标 depot 版本分类，与这八类不重合，见「sync 模式」。

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

## sync 模式（`p4 sync -f` 对等）

`--sync` 把工作区拉到**目标 depot 版本**——默认 head，`--to <CL>` 指定某个 changelist——
语义等价于 `p4 sync -f`，但只下发真正需要传的文件，而不是把整个工作区重传一遍。
几十万文件的工作区上，这是小时级与秒级的差别。

四组动作：

| 组 | 判据 | 动作 |
| --- | --- | --- |
| `Update` | 本地有一份，但不是目标版本 | 拉到目标版本，覆盖本地那份 |
| `Revert` | 本地就是目标版本，内容却已经不是它 | 还原成目标版本 |
| `Restore` | 本地缺失（或被忽略规则盖着） | 从 depot 写回 |
| `Delete` | 目标时刻该路径不在库，本地却有 | 删掉本地文件，并清掉 have 记录 |

四组按**本地状态**分，不按 have 状态分。「本地从没同步过、但已有一份同名文件」落在 `Update`
里——会被 depot 内容整份覆盖，而且没有旧版本可退回，所以工具会单独打一行汇总提醒；「本地
缺失」不论 have 停在哪一版都归 `Restore`，因为对它来说动作就是「从 depot 写回」。

`Update` 与 `Revert` 分开报是有意的：前者是「本地落后了，正常拉取」，后者是「你在本地改了一个
没有打开编辑的文件，改动会被丢弃」。风险等级不同，混在一组就是在误导。

与 `--clean` 的两处分水岭：

- **不碰 depot 里没有的本地文件**——那是用户自己的东西。同一批文件交给 `--clean` 会被删掉。
- **已打开的文件一律不碰**，与 `p4 sync -f` 的官方口径一致（"does not affect open files"）。

与 `p4 sync -f` 的差别只有「传多少」：目标版本、对已打开文件的处理、以及「未打开文件上的本地
改动会被覆盖」三者都一样。少数文件没法在这里比对，会转交原生的 `p4 sync -f`（带 `-a` 时真正
下发，否则下发 `p4 sync -f -n`）：Apple/Resource 旧格式，以及 `headType` 认不出来的文件。

不带 `-a` 仍是预演。只是预演也会打印那句覆盖警告——代价得在授权之前就看得见。

### 快档与 `--verify-all`

默认档沿用摘要缓存与时间戳捷径：与 have 的 syncTime 相差不超过一秒的文件不再重算摘要。
**所以默认档是近似而不是保证**——改过内容却保住了 mtime 的文件（编辑器保留时间戳、从备份
恢复、脚本改写）会被静默漏掉。这是它相对 `p4 sync -f` 唯一的新增失败模式，`p4 sync -f` 没有
这个问题，因为它一律重传。

`--verify-all` 把推断换成验证：目标版本没变的文件全部重算摘要，**摘要缓存也不看**——缓存里
放的是上一轮算出的值，拿它下结论就还是推断。代价是每次运行都要全量读盘（算出的摘要仍写回缓存，
默认档接着受益）。成功语也分档——只有这一档会打印 `The synced files match the target depot
revision.`，默认档只说 `Synced N files in ... seconds.`。

### 回到某个 changelist（`--to`）

`--to 12345` 拉到那个 changelist 时刻的状态，而不是 head：目标之后提交的新版本会被**退回旧版**，
目标之后才创建的路径会被**删掉**（`Delete` 组）。这与 `p4 sync -f ./...@12345` 的判定一致——
那边把这类文件一并报成 `deleted as`。

> **这一组会删本地文件，而且没有反悔余地。** 目标 CL 之后新增的文件都在此列，哪怕内容是你刚
> 写的；删除版本没有内容，`sync @CL` 也找不回来。先跑预演，`-l` 会把它们逐个列出来。

`--to 0` 是用法错误：`@0` 在 p4 语法里是「第一个修订版之前」，那等于说目标时刻什么都不存在。
在 P4V 里，这条路径就是从 History 视图右键一个已提交的 changelist——见「从 History 视图同步到
某个 changelist」。

### 与 clean 模式的其余差异

- `--sync` 与 `--clean` 互斥：同时给出是用法错误（退出码 `2`）。一个会删掉 depot 里没有的文件、
  一个不删，静默择一就是误操作。
- `-c` / `--changelist` 没有意义（sync 不打开任何文件），给出时会告警并忽略。
  想指定目标版本用的是 `--to`——那正是 `-c` 在别的模式下长得最像的东西，所以告警里会点名。
- 需要的是 `read` 权限，而不是 `open` 权限（与 clean 相同）。
- `Delete` 组的文件如果正被忽略规则覆盖（`.p4ignore` 命中），工具不动它；其余三组的这类文件
  照常同步，与 `p4 sync -f` 的行为一致。

## 退出码

| 码 | 含义 |
| --- | --- |
| `0` | 成功。包括预演，以及「有差异但没带 `-a`」的情况。 |
| `1` | 运行错误。stderr 以 `error: ` 开头接原因链——多数情况是一行；多个动作组都失败、或给的路径一个都用不上时逐条另起一行（见下）。 |
| `2` | 用法错误，例如未知参数。由参数解析阶段给出。 |

**路径参数必须是存在的目录。** 不存在的路径、或指向文件而不是目录的路径都会被报出来，并且
不计入工作量；**一个可用的都没有时整轮以退出码 1 结束**，不会打一句 `Operation completed`
假装成功。给出多个路径、只有一部分可用时会照常处理可用的那些，其余的在 stderr 上汇总成一行
告警。P4V 那边两种拼错都会落到这条：prompt 留空（命令行上没有路径）、在文件的历史里点了
`Sync this folder`（传进来的是文件）。

`--clean -a` 或 `--sync -a` 下如果有文件删不掉（被别的进程占着、权限不对）或拉不下来，同样是
退出码 `1`。各动作组按顺序做，一组失败**不拦下**其余的组——一个文件删不掉，不该让另外几百个
留在原地；全部做完后再把失败的组逐个列在错误信息里。有失败时工具不会打印「与 depot 一致」
这类结论，那是它唯一的安全承诺。

> 退出码 `1` 的错误格式与 0.1.2 及更早版本不同：旧版是 anyhow 的多行
> `Error: ...` / `Caused by: ...`，现在压成一行。写脚本解析 stderr 时需要留意。

## 已知问题

它只在我们的仓库上验证过，如果你的配置与我们不同，未必能正常工作。
请先用预演模式确认行为符合预期。你对仓库所做的改动由你自己负责。

- `p4delta` 不实现 move/add 与 move/delete 的配对识别。
- `--clean` 不还原 head revision 是归档版本的文件，只跳过并汇报；Apple/Resource 与认不出的 `headType` 则转交 `p4 clean`。
- `--sync` 的默认档是**近似而不是保证**：它信任时间戳与摘要缓存，改过内容却保住 mtime 的文件会被
  静默漏掉（`p4 sync -f` 没有这个失败模式，它一律重传）。要保证就用 `--verify-all`。归档版本的文件
  同样只跳过并汇报；Apple/Resource 与认不出的 `headType` 转交 `p4 sync -f`。
- `--sync --to <CL>` 会**删掉目标 changelist 之后创建的文件**（它们在那时还不存在），且无法用
  `sync @CL` 找回内容。先跑预演，`-l` 会列出来。
- `--sync -a` 的删除组里若有文件被别的进程占着（编辑器开着、杀毒软件正在扫），`p4 sync -f
  #none` 会在删除上重试约十秒才放弃，那一组会明显卡一下。这是 p4 自身的行为，不是 p4delta
  加的重试；失败会被报出来（见退出码一节），只是要等。
- `p4 clean -K`（抑制 ktext 关键字展开）没有暴露，clean 始终按 p4 的默认行为展开关键字。
- `--clean` 或 `--sync` 与 `-c` 同时给出时 `-c` 被忽略（各自会打印一行告警）。
- depot 路径里含 `#`、`%`、`@`、`*`，或以 `...` 结尾的文件名会被 p4 当成通配符解析，转交路径、
  clean 的还原规格与 sync 的文件规格都受影响（`--to` 下要拼 `@<CL>`，这类名字的路径尤其容易歧义）。
- p4 对**逐文件**错误（protections 拒绝 open、路径不在 client view）返回的退出码是 **0**，错误只出现在 stderr 上。
  工具因此把改状态命令（add / edit / delete / revert / sync）的 stderr 非空也判为失败，p4 的原话会进错误信息——
  「p4 一个文件都没接受、工具却报 `Inconsistencies fixed.`」不会再发生。反过来说，如果你的 p4 在**成功**时
  也往 stderr 写提示，那一轮会被判失败（`Failed to apply N change group(s)`），把那条提示加进 `FailureMode`
  的例外即可。转交 `p4 reconcile` / `p4 clean` 的兜底路径不受这条影响：p4 对「无事可做」也写 stderr。
- **团队自己同步 `customtools.xml` 的脚本会把这里的注册整份覆盖掉。** 我们这边
  `Package/Script/Perforce/RunTaskAndSyncFiles.bat` 的结尾有一段：把 depot 里的 `tools.xml` 与
  `%USERPROFILE%\.p4qt\customtools.xml` 逐字节比对，不一致就用 depot 那份覆盖——而那个 bat 是
  团队其它工具的入口，**每次**调用都会走到这段。装过 p4delta 之后两份必然不一致（多出七个节点），
  所以下次跑它的任何工具，p4delta 的条目就全没了，右键菜单里再也看不到。要么把 p4delta 那七个
  节点加进 depot 里的 `tools.xml` 源文件，要么跑完同步再跑一次 `install.ps1`。
- 安装脚本写出的工具定义里，三个复选框与元素的对应是实测出来的，各有取舍：
  「Run tool in terminal window」对应的元素名没有在 GUI 里逐项核对过（`<Console>` 按 P4V 自己
  导出的样例写；实测本工具跑起来**不**弹 cmd 终端窗，输出直接进 P4V 自己的输出窗，预演清单
  看得见）；「Ignore P4CONFIG files」的元素名（`<IgnoreP4Config>`，`CustomToolDef` 的直接
  子元素）已知但**故意不写**——写了等于关掉 `.p4config` 的发现，而工具要靠它拿 `P4IGNORE` 与
  `P4CHARSET`；「Add file browser to prompt dialog」（`<ShowBrowse>`，`<Prompt>` 内、
  `<PromptText>` 之后）实测那个浏览按钮只能选**文件**、选不了目录，对问目录的 prompt 没用，
  所以也不写。
- sync 那几条**已在真实 P4V 里点过**（2026-10-03）：两条入口都出现在预期的菜单里、prompt 正常
  弹出，留空点 OK 以退出码 1 报 `No path given`，输出进 P4V 自己的输出窗。剩余未知见下一条。
- prompt 里粘的目录若**含空格**，P4V 如何把它拼进命令行没有实测（我们这边的路径都不含空格）。
  含空格的路径建议改用命令行调用。
- 安装路径里含空格时（用户名带空格就会），`<Command>` 能否被 P4V 正确解析尚未实测。安装脚本会在
  这时打印警告；真出问题就用 `-InstallDir` 换一个不含空格的目录。

## 开发

想改代码、跑 e2e 沙箱或发版：见 [CONTRIBUTING.md](CONTRIBUTING.md)。
