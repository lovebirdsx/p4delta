# `--json` 输出契约

`--json` 把 p4delta 从「给人看的工具」变成「给程序调用的工具」。本文是这个契约的**单一真相**：
消费方（编辑器扩展）按这里写的东西解析，实现按这里写的东西发射；改任何一条都要同时改两处，
并更新本文件。

## 通道与编码

| 通道 | `--json` 关闭（默认） | `--json` 打开 |
|---|---|---|
| stdout | 人类可读报告（标题、清单、统计） | **只出 JSON Lines**（UTF-8，紧凑，一行一条记录，无 BOM） |
| stderr | 警告 / 意外 | 人类可读报告**全部改道到这里**，外加 `kind:"progress"` 记录 |

- 打开 `--json` 后 stdout **一个非 JSON 字节都不许出现**，包括转交给原生 p4 的子进程输出
  （那些调用一律捕获，需要时翻译成记录）。
- `-l`（列清单）在 `--json` 下同样改道 stderr；给不给都不影响 stdout 的记录流——清单的信息
  本来就在文件记录里。
- 退出码沿用文本模式的语义：`0` 成功、`1` 失败、`2` 用法错误。

## 硬条款（消费方可以依赖的东西）

1. **`kind:"summary"` 是「这一轮有结论」的唯一凭据**。任何经由 `run()` 返回的路径（成功、
   `bail!`、用法之外的任何失败）都要发一条。进程崩溃 / 被 `kill` 天然没有 summary —— 所以
   **没有 summary 就等于没有结论**，消费方绝不许把「读到一半就断了」当成完整答案。
2. **`ok:false` 的记录流永远是残缺的**：它可能已经发了一部分文件记录。消费方只允许在
   `ok:true` 时把记录流当成全集。
3. **`clientFile` 是 client 语法**（`//<client>/<相对路径>`）而不是本地路径，好让消费方
   沿用已有的 `clientToLocalPath` 翻译（那套翻译已经在原生引擎的路径上跑熟了，两条引擎
   给出同一种拼法，比较逻辑只有一份）。**原生 `p4 reconcile` 那条路径回的是本地路径**，
   δ 自己翻（见「`class:"handoff"` 变体」）。
4. **`depotFile` 必有**（含新增文件，按 client view 映射出来）。`action` / `rev` 的口径与
   `p4 -Mj reconcile` 对齐，但**字段名与取值集合不必逐字相同**：原生 2024.1 那边 `rev`
   叫 `workRev`（add 也带它，但那是「将要成为的版本」而不是 have），原生 `action` 里多出
   的几种一律收敛（见「`class:"handoff"` 变体」）。
5. **转交给原生 p4 的文件必须出现在记录流里**（`class:"handoff"`），绝不静默丢。
6. **预演与应用给出同一批文件**：`-a` 不改文件集合，只把记录标成 `applied:true`。转交给
   原生 p4 的那批是唯一的例外——open 预演拿得到逐条动作（正常 file 记录），open 应用只能
   整批报 `class:"handoff"`；两边点的是同一批文件。
7. scope 位置参数接受 `<path>/...`（`...` 展开按 depot/have 列表做，与磁盘无关）；**每个参数
   就是一条目标**，不按 `;` 拆分、也不认 `-` 前缀那套排除写法。路径里的 `;`、空格、`#`、
   `@`、`%` 都是文件名的合法字符：**本地原始路径**与交给 p4 的 **file spec** 是两种东西，
   `%xx` 转义只在 p4 边界上做一次（`path::escape_file_spec`，范围查询、动作调用、`p4 where`
   共用），记录里回给消费方的始终是原始本地路径。
8. **排除走 `--exclude-dir` / `--exclude-file`**（可重复）。相对路径以 client root 为基准，
   类型由参数自己声明（不 stat、不按存在性猜）。排除是硬上限：落在排除里的文件既不报
   （一条记录都没有）也不动。配置文件里的排除与它取并集且同样优先。普通同步（`--sync`
   不带 `--force`）**不接受**这两个开关：那条路的范围就是 client view、每个动作由原生判定，
   一次性的排除会把"原生会做什么"换成另一个问题（持久边界写进 `.p4delta-scope`，那份排除
   对普通同步照样生效）。

## 记录

所有记录都是单行 JSON 对象，靠 `kind` 分派。

### `kind:"file"`

```json
{"kind":"file","mode":"open","class":"edit","action":"edit","depotFile":"//depot/ws/a.txt","clientFile":"//bench/ws/a.txt","rev":"3","applied":false,"force":false}
{"kind":"file","mode":"sync","class":"update","action":"updating","depotFile":"//depot/ws/b.txt","clientFile":"//bench/ws/b.txt","rev":"4","applied":false,"force":false,"stage":"preview","nativeAction":"updated"}
```

第二行是普通同步的记录：它比第一行多 `stage`（这条来自预演还是应用）与 `nativeAction`
（原生报出的动作原文）两项，`class:"resolve"` 的记录只有前者、没有后者（见下）。

| 字段 | 类型 | 说明 |
|---|---|---|
| `mode` | `"open"` \| `"clean"` \| `"sync"` | 三模式各自的分类学不同，`class` 必须配 `mode` 读 |
| `class` | 字符串枚举 | 稳定英文枚举，见下表；**这是权威字段** |
| `action` | 字符串 | p4 措辞的动作，供直接拼日志/toast；`class:"handoff"` 的记录例外，见下节 |
| `depotFile` | 字符串 | 恒有 |
| `clientFile` | 字符串 | client 语法；拼不出来时退化成**本地路径**并在 stderr 记 warning（口径见下） |
| `rev` | 字符串，可缺 | have / 目标版本号；**新增文件没有这一项**。原生 2024.1 的对应字段叫 `workRev`（add 也带它，但那是「将要成为的版本」而不是 have，翻译时按 class 摘掉） |
| `applied` | 布尔 | 这一轮是否带 `-a` 真改状态。**不是**逐文件的成败：某条记录的 p4 命令失败时它照样是 `true`，那一批的失败走 summary 的 `ok:false` 与 stderr 上的人类可读错误（逐文件的 error 记录当前没有，见「`kind:"error"`」） |
| `stage` | 字符串，可缺 | 普通同步（`--sync` 不带 `--force`）独有：`"preview"` 或 `"apply"`。这条路一次运行**只发一套记录**——预演那份不重复发，带了 `-a` 就是 apply 那一套 |
| `nativeAction` | 字符串，可缺 | 原生报出的动作原文（`added` / `updated` / `deleted` / `refreshed`），普通同步独有。消费方要跟 `p4` 自己的输出对齐时看它，别去反推 `action` 的派生值 |
| `force` | 布尔 | 这一轮是不是强制修复（`--sync --force`）。普通同步与强制修复共用 `mode:"sync"` 与一部分 `class`（`update` / `delete`），消费方要区分它们只能靠这个布尔 |

`class:"resolve"` 的记录**没有** `nativeAction`：原生对这类事件只有一句 `info` 提示
（`//depot/f#2 - is opened and not being changed`），没有动作词。记录里宁可诚实地缺席，
也不去编一个 p4 不会打印的词。

`clientFile` 拼不出 client 语法（退化成**本地路径**）只有一档：**路径不在 client root 下**
（盘符大小写不一致、junction / symlink 形式的根、client view 把文件映射到根之外）。根本身
拿不到不是一档退化——那是失败关闭（见「范围配置」），跑不到拼 `clientFile` 这一步。

告警做了节流——一个工作区上万个文件，逐条告警会把 stderr 淹掉：**只点名第一条**（点名的
是路径与根，正好是排查需要的两样东西），整轮结束时汇总成一句「N 条记录的 clientFile 退化
成 本地路径」；只有一条时不再重复——那一条已经点名过了。

退化之后记录仍然可用（消费方本来就把非 `//` 开头的值当本地路径用），但记录流会变成两种
拼法混排，所以必须留痕。匹配判据在 Windows 上大小写不敏感（那里的文件系统如此），其余
平台敏感。

`class` 与 `action` 的对照表（每类变更的 `action` 由 `class` 唯一决定，实现里有交叉校验
测试；`handoff` 那行例外，它的动作长在 `handoff` 字段上，见下节）：

| mode | class | action | 含义 |
|---|---|---|---|
| open | `add` | `add` | 工作区有、depot 没有，未打开 |
| open | `edit` | `edit` | 相对 have 有改动，未打开 |
| open | `delete` | `delete` | 工作区没有、depot 有，未打开 |
| open | `reopen_edit` | `edit` | 已 open for delete，磁盘上却还在且有改动 → 改开成 edit |
| open | `reopen_delete` | `delete` | 已 open for edit，磁盘上却没了 → 改开成 delete |
| open | `revert_add` | `revert` | 已 open for add，磁盘上却没了 → 撤销那次打开 |
| open | `revert_edit` | `revert` | 已 open for edit，内容与 have 一致 → 撤销打开 |
| open | `revert_delete` | `revert` | 已 open for delete，内容与 have 一致 → 撤销打开 |
| clean | `delete` | `deleting` | 删掉 depot 里没有的本地文件 |
| clean | `revert` | `reverting` | 用 have 版本覆盖本地改动 |
| clean | `restore` | `restoring` | 从 depot 写回本地缺失的文件 |
| sync | `add` | `adding` | 目标处有、工作区没有 → 写进来 |
| sync | `update` | `updating` | 拉到目标版本 |
| sync | `revert` | `reverting` | 目标版本没变、本地内容不对 → 还原 |
| sync | `restore` | `restoring` | 本地缺失 → 写回目标版本 |
| sync | `delete` | `deleting` | 目标时刻不在库 → 删掉本地文件 |
| sync | `resolve` | `scheduling` | 已打开、have 不在目标版本上 → 原生把 have 拉到目标版本、挂上待 resolve，内容一个字不改 |
| 任意 | `handoff` | `handoff` 字段的值 | 转交给原生 p4 的文件，见下节 |

`sync` 的类在两种模式下的**来源不同**（`force` 字段区分）：

- 强制修复（`force:true`）的四类（`update` / `revert` / `restore` / `delete`）来自 δ 自己的
  差异分析，那是「无论本地改没改都修成目标版本」；
- 普通同步（`force:false`）只发原生**打算传**的文件，即 `add` / `update` / `delete` 三类，
  外加 `resolve` 那一类。普通同步不做强制修复，所以 `revert` / `restore` 在它那里永不出现。

`resolve` 那一类普通同步也不是照单全收：`p4 opened` 给的是范围内**全部**已打开文件，而
原生只对 have 不在目标版本上的那些说话（have 已经在目标版本上的，预演连一句提示都没有），
所以只有 `haveRev != 目标版本` 的已打开文件才成一条 `resolve` 记录。

`resolve` 的动作词是 `scheduling` 而不是 `resolving`：δ 不做合并，它只是把文件交给原生记账。
消费方看到 `class:"resolve"` 就知道「这个文件现在是打开的，have 会被拉到目标版本」，接下来
`p4 resolve` 是用户的事。

**`open` 的 `revert_*` 三组不是 `reconcile -a -e -d` 的一部分**——它们是 `p4 revert -a` 的
语义（把「已打开但内容没变」的文件撤销打开）。实测对照：原生 `p4 reconcile -n -a -e -d`
只报 add / edit / delete / reopen_*，不报这三组。要与之逐行对齐，用 `--no-revert-groups`。

三组里 `revert_delete` 是个例外：原生 `-e` 对「文件还在、却被 open for delete」的处理
**与内容改没改无关**，一律改开成 edit（实测）。所以 `--no-revert-groups` 不是把它摘掉，
而是把它并进 `reopen_edit`（动作 `edit`，下发的 `revert -k` + `edit` 正是「改开成 edit」）。

### `kind:"file"` 的 `class:"handoff"` 变体

```json
{"kind":"file","mode":"open","class":"handoff","action":"reconcile","handoff":"reconcile","depotFile":"//depot/ws/x.bin","clientFile":"//bench/ws/x.bin","applied":true,"force":false}
```

`handoff` 字段取值 `"reconcile"` / `"clean"` / `"sync"`，说明这一批被交给原生 p4 的哪条
命令；`action` 就是它的值——这类记录没有逐文件动作，`action` 自然不是动作表里的那一套。

open 模式的**预演**额外把原生 `p4 reconcile -n -Mj -Ztag` 的记录逐条翻译成正常 file 记录，
所以那条路径上没有 handoff 记录——预演要能预告真实动作，转交的那批不能被藏起来。翻译有
三条规则：

- `clientFile` 要按硬条款 3 翻成 client 语法：原生 reconcile 回的是**本地路径**（2024.1
  实测；同族的 `opened` / `where` 回的才是 client 语法），拼不出来时同样退化成 本地路径；
- `rev` 取自原生的 `workRev`，add 那一档缺席（理由见字段表）；
- `class` 收敛回本工具的枚举：原生的 `move/add` / `branch` / `integrate` 一并按 `edit` 报
  （落到磁盘上都是「这份内容要被开出来」），判据同「宁可报得粗，不可丢行」。

其余四种（clean 预演/应用、sync 预演/应用、open 应用）只发 handoff 记录：两个路径都有，
没有逐文件动作，也不进 `counts`。

### `kind:"unmatched"`

```json
{"kind":"unmatched","path":"/ws/gone.txt"}
```

一个 scope 入口在 depot 与磁盘上都没匹配到任何东西。编辑器把「删除整个目录」这种场景
拆成成对的 `[<path>, <path>/...]` 两个入口发过来，两个都匹配不到是完全正常的答案
（「这里确实什么都没有」），不是失败。

**入口全落空**时（文本模式下 `bail!`、退出码 1）：逐条发 `unmatched` 记录，summary 里写
`ok:false, reason:"no-entry-matched"`，**退出码保持 1**。消费方按记录自己的路径判断哪些
路径被认定为空，不要只看退出码。

### `kind:"error"`

```json
{"kind":"error","message":"p4 was not found; looked at P4_EXE and PATH. …"}
```

**整轮失败时的那一条错误记录**（`run()` 拿到 `Err` 时发，`message` 是错误原文）。

记录上还可以带 `clientFile`，**当前实现不带**——它是留给「这一轮的错误能明确归到某个
文件」的未来的；消费方现在只需读 `message`，不要依赖它存在。

逐文件的失败（`p4 add` 拒收名字里带 `@` / `%` 的文件、protections 拒绝 open 之类）**不发
记录**：它们在下发命令的那一层就被拼成一条错误消息，由 summary 的 `ok:false` 与 stderr 上
的人类可读文本承担。消费方目前不读 error 记录，多造一种逐文件的错误记录，只会多出一份
没人对账的线协议。

### `kind:"summary"`

```json
{"kind":"summary","mode":"open","ok":true,"applied":false,"total":340,"counts":{"add":100,"edit":200,"delete":40},"scopeMatched":1,"unmatched":0,"elapsedMs":249,"reason":null}
```

| 字段 | 说明 |
|---|---|
| `mode` | 同 file 记录 |
| `force` | 同 file 记录：`--sync --force` 是 `true`，普通同步与其余模式是 `false` |
| `ok` | 这一轮是否有结论（**唯一**可信度凭据） |
| `applied` | 是否带 `-a` 真跑了 |
| `total` | `counts` 各值之和。口径是**「δ 给出逐文件动作的文件数」**：转交出去又没有逐条翻译的那批不进 `counts`，所以 clean / sync 的预演与应用、open 的应用都不含它；open 的预演例外（逐条翻译，见「`class:"handoff"` 变体」），那批进 `counts` |
| `counts` | `class` → 数量；**只出现非零项**，缺席即 0。`handoff` 永不出现（它没有逐文件动作，`class:"handoff"` 的记录也从来不是「一个动作」） |
| `scopeMatched` | 匹配到东西的入口数；**普通同步是 `null`**——判定权在原生手里，「入口匹配了几个」这个结论它拿不到。`null` 是「不知道」，不是 0 |
| `unmatched` | 落空入口数；普通同步恒为 0，理由同上 |
| `elapsedMs` | 整轮墙钟毫秒 |
| `reason` | `null`，或 `"no-entry-matched"`（入口全落空）；其余失败一律 `"error"` |

不进 `counts` 的还有：落空的入口（走 `unmatched`）、归档版本与从没同步过的文件（见「已知
差异」，它们只有 stderr 上的汇报）。

### `kind:"progress"`（stderr）

```json
{"kind":"progress","phase":"digest","step":3,"total":5,"message":"Checking digests for 340 files."}
```

阶段是固定枚举：`start` `analyze` `digest` `report` `done`。`step` 是阶段在表里的下标 + 1，
单调不回跳，`total` 恒为 5。消费者**只许把它当进度提示**，不许拿它当结论。

普通同步（`--sync` 不带 `--force`）走的是另一张表——它没有 analyze / digest，却有
preview / filter / apply：`start` `preview` `filter` `apply` `done`（同样 5 段）。报一个
这一轮根本不会出现的阶段，只会让消费方以为工具卡住了，所以按模式换表。`total` 两种都是 5。

只收「顺序主干」上的阶段：`depot` / `scan` / `have` 三路是并发跑的，从并发分支里发进度会让
`step` 回跳，而回跳的进度条比没有进度条更难读。这三路的细节走 `message`（自由文本）。

## 新增的开关

| 开关 | 作用 |
|---|---|
| `--json` | 打开本契约 |
| `--client-root <path>` | client 根目录。**必须是该 client 的固定 `Root`**（client spec 里那一个 `p4 client -o` 报的；对不上、拿不到、落在 AltRoots 上一律失败关闭）。它同时是 `.p4delta-scope` 的归属、相对路径的基准，以及拼 client 语法（记录里的 `clientFile`）用的根；平时不必传：工具从 client spec 取固定 `Root`，再用 `p4 info` 核对 cwd 没停在 AltRoot 上（`p4 info` 的 `clientRoot` 随 cwd 变，从 AltRoot 里发起时报的就是那个 AltRoot，不能当成根本身） |
| `--no-scope-file` | 忽略 client root 下的 `.p4delta-scope`：**只**让持久配置缺席，本次的 `--exclude-*` 与 client view 照旧生效 |
| `--exclude-dir <path>` / `--exclude-file <path>` | 可重复。本次操作的排除，目录按整棵子树、文件按精确路径；类型由参数声明（不 stat），相对路径以 client root 为基准。与配置里的排除取并集且同样优先。**普通同步不接受它**（见「硬条款」8） |
| `--no-revert-groups` | 抑制 `open` 模式的 `revert_add` / `revert_edit` 两组，并把 `revert_delete` 并进 `reopen_edit`，语义回到逐行等于 `p4 reconcile -a -e -d`（理由见上一节的例外） |

`--sync` 的两档由 `--force` 分：不带是**普通同步**（`force:false`，把工作区拉到目标版本，
覆盖保护与 opened 交给原生），带上是**强制修复**（`force:true`，修成目标版本）。两者的记录
形状差别见上文 `sync` 那几行。

## 范围配置（持久边界）

持久边界写在 **client root 下**的 `.p4delta-scope`（严格 JSON，UTF-8）。位置固定：不向上查找、
不认嵌套配置——一个 client 只有一份，就在根上。文件不存在是正常情况（ENOENT），**其它任何
读取失败都 fail closed**（权限、是个目录、坏 JSON），绝不退化成「没有配置」继续跑。

```json
{
  "include": [
    { "dir": "src" },
    { "file": "README.md" }
  ],
  "exclude": [
    { "dir": "build" },
    { "file": "src/generated.rs" }
  ]
}
```

- 顶层只认 `include` / `exclude` 两个键，值都是数组；
- 每条目**恰好**有 `dir` 或 `file` 之一：类型由声明决定，δ 不去 stat、不按存在性猜；
- 路径是**相对 client root 的 POSIX 相对路径**（分隔符一律 `/`；`"."` 目录条目表示根）。
  绝对路径、盘符、`\`、NUL、`*` `?` 一律拒绝；
- 省略 `include` = 整个 client root；`"include": []` = 显式的空集，两者不是一回事；
- 重复键（同一份 JSON 里出现两次 `include`）、不认识的字段、类型不对，一律拒绝整份配置。
  机器给的集合宁可拒绝，不可静默扩大范围；
- 配置文件自身恒被排除（隐含一条 `file` 排除），消费方不必自己写。

规则的**逐条向量**（含平台大小写策略、分量边界、父目录目标的钳制、排除盖住目标的报错等）
见 `tests/fixtures/scope-contract.json`——那是本契约的语言无关版本，语言侧的实现都用同一份
向量对拍（Rust 侧由 `src/scope.rs` 的 `mod contract` 消费）。改动本节任何一条规则，必须同步
改这份 fixture。

`--no-scope-file` 让这份配置整份缺席（不读、坏配置也不报）；本次的 `--exclude-*` 与 client
view 不受它影响。

### 求值

`配置包含 ∩ 目标 − 配置排除 − 本次排除`；配置缺席时包含即目标。目标只来自位置参数
（每个参数一条，`<path>/...` 的展开按 depot / have 列表做，与磁盘无关），排除只来自
`--exclude-dir` / `--exclude-file`（本次）与配置的 `exclude`（持久），两者取并集且优先于包含。

机器可读的入口一律 fail closed，失败发生在碰任何 p4、任何写之前：

- 配置文件不是合法 JSON、顶层不是对象、出现上面任何一条禁止项；
- 目标既没有被配置覆盖、也不在 client view 里（`--sync` 那条路径用严格求值：一条定位不了的
  目标就是错误，不是「悄悄少一块」）；
- 一个目标被排除整个盖住（排除写宽了要报出来，不能静默给出空集）；
- 一份配置都没覆盖到任何目标（两侧范围不相交）；
- `--client-root` 与 client spec 的固定 `Root` 对不上、那个 `Root` 拿不到，或调用目录落在
  AltRoots 上（`p4 info` 报的根与固定 `Root` 不是一回事）。
  换一个根，同一批本地路径对 depot 的映射就变了，范围也跟着变——这里**刻意**是失败关闭，
  不降级、不告警后继续。

## 与原生引擎的已知差异## 与原生引擎的已知差异

写进契约是因为消费方**有权知道**，不是因为它们可以忽略：

- **时间戳捷径**：mtime 落在 have 的 syncTime ±1 秒内的候选被当成「没改过」，不再读盘算摘要。
  这是 p4delta 快的全部原因之一，代价是那一秒窗口内的改动可能不被发现。
- **`unsupported` 文件转交原生 p4**：δ 算不出摘要的类型（Apple 资源叉之类）交给
  `p4 reconcile`；除 open 的预演（逐条翻译成正常 file 记录）外，转交的那批只以 `handoff`
  记录出现，没有逐文件动作，也不进 `counts`。
- **归档版本（`headAction=archive`）与从未同步过的文件**：跳过并单独汇报，不进 `counts`。
- **普通同步不扫工作区，也不读摘要缓存**：`--sync`（不带 `--force`）只问原生「你打算传哪些
  文件」，再按范围过滤。所以它没有 mtime 捷径那类差异，也**不承诺比原生 `p4 sync` 快**——
  省下的是 δ 自己的扫描与摘要，原生该传的字节一个不少。代价是 `scopeMatched` 发 `null`
  （见 summary 表）。
- **普通同步对已打开文件的补查**：原生对「已打开、have 不在目标版本上」的文件只给一句 `info`
  提示，路径埋在文本里，正文记录一条都没有。δ 不解析那句文本，而是补跑两条只读查询
  （`p4 opened` + `p4 fstat`，规模由打开数决定）拿身份，再按精确规格下发——这些文件因此
  以 `class:"resolve"` 出现在记录流里。have 已经在目标版本上的已打开文件原生连提示都不发，
  δ 也不把它们报出来。补查失败或解释不了那批提示时整轮停下，不拿一份少了东西的答案去写。
- **名字里带 p4 filespec 元字符（`@` `#` `*` `%`）的文件**：δ 报出来并去 `p4 add`，而 p4 拒收这
  类名字（要 `-f` 才肯），于是这一组会失败、`ok:false`——实测原生 `p4 reconcile` 是发一条
  `severity:2` 的警告后**跳过**。两者对同一份工作区给出的清单因此可能差这一条。δ 内部查
  client view 映射时会按 p4 的 `%xx` 规则转义，所以这类文件的 `depotFile` 是 p4 自己的转义拼法
  （`//depot/main/report%402024.txt`），`clientFile` 则是不转义的原样拼法
  （`//<client>/report@2024.txt`）。
