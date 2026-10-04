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
7. scope 位置参数接受 `<path>/...`（`...` 展开按 depot/have 列表做，与磁盘无关）；**消费方给的
   路径一律原样使用**，不替它做 p4 百分号转义（唯一的例外是 δ 内部查 client view 映射的那次
   `p4 where`，见「已知差异」）。
8. **入口可以带 `-` 前缀表示排除**（`-<path>`，目录整棵子树用 `-<path>/...`）。排除是硬
   上限：落在排除里的文件既不报（一条记录都没有）也不动。编辑器把
   `reconcile.excludeFolders` 直接翻成这些入口发过来，δ 侧不再有自己的 carve 逻辑；
   路径以 `-` 开头的真实文件要写成 `./-<name>`。

## 记录

所有记录都是单行 JSON 对象，靠 `kind` 分派。

### `kind:"file"`

```json
{"kind":"file","mode":"open","class":"edit","action":"edit","depotFile":"//depot/ws/a.txt","clientFile":"//bench/ws/a.txt","rev":"3","applied":false}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `mode` | `"open"` \| `"clean"` \| `"sync"` | 三模式各自的分类学不同，`class` 必须配 `mode` 读 |
| `class` | 字符串枚举 | 稳定英文枚举，见下表；**这是权威字段** |
| `action` | 字符串 | p4 措辞的动作，供直接拼日志/toast；`class:"handoff"` 的记录例外，见下节 |
| `depotFile` | 字符串 | 恒有 |
| `clientFile` | 字符串 | client 语法；拼不出来时退化成**本地路径**并在 stderr 记 warning（口径见下） |
| `rev` | 字符串，可缺 | have / 目标版本号；**新增文件没有这一项**。原生 2024.1 的对应字段叫 `workRev`（add 也带它，但那是「将要成为的版本」而不是 have，翻译时按 class 摘掉） |
| `applied` | 布尔 | 这一轮是否带 `-a` 真改状态。**不是**逐文件的成败：某条记录的 p4 命令失败时它照样是 `true`，那一批的失败走 summary 的 `ok:false` 与 stderr 上的人类可读错误（逐文件的 error 记录当前没有，见「`kind:"error"`」） |

`clientFile` 拼不出 client 语法（退化成**本地路径**）有两档，都在 stderr 上告警，而且**都
做了节流**——一个工作区上万个文件，逐条告警会把 stderr 淹掉：

- clientspec 的根整个拿不到（没给 `--client-root`、`p4 info` 也问不出 `clientRoot`）：开跑
  时记一行 warning；
- 路径不在根下（根给错、盘符大小写不一致、junction / symlink 形式的根、client view 把文件
  映射到根之外）：**第一条逐条点名**（点名的是路径与根，正好是排查需要的两样东西）。

两档都记账，整轮结束时汇总成一句「N 条记录的 clientFile 退化成 本地路径」；只有一条时不再
重复——那一条已经点名过了。

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
| sync | `update` | `updating` | 拉到目标版本 |
| sync | `revert` | `reverting` | 目标版本没变、本地内容不对 → 还原 |
| sync | `restore` | `restoring` | 本地缺失 → 写回目标版本 |
| sync | `delete` | `deleting` | 目标时刻不在库 → 删掉本地文件 |
| 任意 | `handoff` | `handoff` 字段的值 | 转交给原生 p4 的文件，见下节 |

**`open` 的 `revert_*` 三组不是 `reconcile -a -e -d` 的一部分**——它们是 `p4 revert -a` 的
语义（把「已打开但内容没变」的文件撤销打开）。实测对照：原生 `p4 reconcile -n -a -e -d`
只报 add / edit / delete / reopen_*，不报这三组。要与之逐行对齐，用 `--no-revert-groups`。

三组里 `revert_delete` 是个例外：原生 `-e` 对「文件还在、却被 open for delete」的处理
**与内容改没改无关**，一律改开成 edit（实测）。所以 `--no-revert-groups` 不是把它摘掉，
而是把它并进 `reopen_edit`（动作 `edit`，下发的 `revert -k` + `edit` 正是「改开成 edit」）。

### `kind:"file"` 的 `class:"handoff"` 变体

```json
{"kind":"file","mode":"open","class":"handoff","action":"reconcile","handoff":"reconcile","depotFile":"//depot/ws/x.bin","clientFile":"//bench/ws/x.bin","applied":true}
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
| `ok` | 这一轮是否有结论（**唯一**可信度凭据） |
| `applied` | 是否带 `-a` 真跑了 |
| `total` | `counts` 各值之和。口径是**「δ 给出逐文件动作的文件数」**：转交出去又没有逐条翻译的那批不进 `counts`，所以 clean / sync 的预演与应用、open 的应用都不含它；open 的预演例外（逐条翻译，见「`class:"handoff"` 变体」），那批进 `counts` |
| `counts` | `class` → 数量；**只出现非零项**，缺席即 0。`handoff` 永不出现（它没有逐文件动作，`class:"handoff"` 的记录也从来不是「一个动作」） |
| `scopeMatched` | 匹配到东西的入口数 |
| `unmatched` | 落空入口数 |
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

只收「顺序主干」上的阶段：`depot` / `scan` / `have` 三路是并发跑的，从并发分支里发进度会让
`step` 回跳，而回跳的进度条比没有进度条更难读。这三路的细节走 `message`（自由文本）。

## 新增的开关

| 开关 | 作用 |
|---|---|
| `--json` | 打开本契约 |
| `--client-root <path>` | client 根目录。给了就不必自己去问 `p4 info`（窄查询正是靠它省掉一次往返）。**只有拼 client 语法用**，不参与任何范围判断 |
| `--no-scope-file` | 忽略工作区里的 `.p4delta-scope`。编辑器自己持有范围（聚焦目录 + `reconcile.excludeFolders`），不希望在别人的配置上再叠一层 |
| `--no-revert-groups` | 抑制 `open` 模式的 `revert_add` / `revert_edit` 两组，并把 `revert_delete` 并进 `reopen_edit`，语义回到逐行等于 `p4 reconcile -a -e -d`（理由见上一节的例外） |

## 与原生引擎的已知差异

写进契约是因为消费方**有权知道**，不是因为它们可以忽略：

- **时间戳捷径**：mtime 落在 have 的 syncTime ±1 秒内的候选被当成「没改过」，不再读盘算摘要。
  这是 p4delta 快的全部原因之一，代价是那一秒窗口内的改动可能不被发现。
- **`unsupported` 文件转交原生 p4**：δ 算不出摘要的类型（Apple 资源叉之类）交给
  `p4 reconcile`；除 open 的预演（逐条翻译成正常 file 记录）外，转交的那批只以 `handoff`
  记录出现，没有逐文件动作，也不进 `counts`。
- **归档版本（`headAction=archive`）与从未同步过的文件**：跳过并单独汇报，不进 `counts`。
- **名字里带 p4 filespec 元字符（`@` `#` `*` `%`）的文件**：δ 报出来并去 `p4 add`，而 p4 拒收这
  类名字（要 `-f` 才肯），于是这一组会失败、`ok:false`——实测原生 `p4 reconcile` 是发一条
  `severity:2` 的警告后**跳过**。两者对同一份工作区给出的清单因此可能差这一条。δ 内部查
  client view 映射时会按 p4 的 `%xx` 规则转义，所以这类文件的 `depotFile` 是 p4 自己的转义拼法
  （`//depot/main/report%402024.txt`），`clientFile` 则是不转义的原样拼法
  （`//<client>/report@2024.txt`）。
