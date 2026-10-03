# p4delta 为什么比 p4 原生快

[README](../README.md#性能测试) 给的是数字，这里给的是原因。机制全部写在代码注释里，本文把它们收敛成
一条因果链，并分清**哪些是真的算得快、哪些是压根没做**。

## 先看对照基线

README 里的对手是 **P4V 的 `Reconcile Offline Work`（GUI 工具）**，不是 `p4 reconcile` 命令。GUI 那一层
自带开销：结果预览表格、过滤器，以及一个已知缺陷——窗口刷新会重新发起扫描，大工作区上表现为「永远跑不完」
（Perforce bug #70465）。所以「快 10–100 倍」里有一部分是 CLI 与 GUI 的差距，拿 `p4 reconcile` 命令本身
去比，差距会小一些，但仍然显著。

## 一、不做 move detection（最大的单项）

`p4 reconcile` 会把 add 和 delete 配对，识别成 move/add + move/delete（P4V 的
「Detect moved files when reconciling offline work」偏好默认勾选）。Perforce 自己的
[KB 15133](https://portal.perforce.com/s/article/15133) 指出 reconcile 有两个最慢阶段，move detection 是
其中之一，客户端为此要对候选对做 digest 检查、序列树比对和 `p4 diff` 逻辑：

> During a reconcile, the client may check digests (*Digest*) and use a sequence tree (*Sequence*) and
> p4 diff logic (*Diff*) to determine if add/delete pairs are moves.

这项开销随待配对文件数急剧增长（社区有 O(n²) 的实测报告），直到 **2025.1 才加了 `--parallel=n`** 让客户端
多线程做这部分——一个需要专门发版优化的阶段，说明它确实贵。

p4delta 完全不实现 move 配对（README「已知问题」第一条明写）。这一条同时解释了 README 里两个最扎眼的数字：

- 「`Reconcile Offline Work` 的性能随改动文件数增加会迅速劣化，`p4delta` 不会」——O(n²) 对 O(n)。
- 「二进制 UE 版本升级」那一次（几乎全树改动 → 海量 add/delete 对）原生跑约 2 小时被取消，p4delta 约 30 秒。
  那次的时间不可能花在算摘要上——30 秒算完的摘要量，原生不会因为算法差就变成 2 小时。

代价是没有 move 历史，重命名会如实报告成一次删除加一次新增。这是刻意的取舍，不是遗漏。

## 二、跨运行记住摘要

原生没有任何跨进程的摘要缓存：每次 reconcile 都从头算。P4V 反复 reconcile 同一个工作区，每次都重付一遍。

p4delta 把「路径 → 摘要」持久化在
`%LOCALAPPDATA%\p4delta\cache\digests_<workspace>.bin`（缓存目录见 `src/lib.rs` 里的 `ProjectDirs`；
条目结构 `src/model.rs::WorkspaceCacheEntry`，键是规范化后的本地路径）。命中判据是 **size、mtime、
路径键三者全等**（`src/digest.rs::parallel_compute_digests`）。三个设计细节：

- **bincode 2 + 流式读写**（`src/lib.rs` 的加载路径、`src/cache.rs::CacheWriter::maybe_save`）。缓存可以到
  几百 MB，整份读进内存再解码会让峰值翻倍。
- **先写 `.bin.tmp` 再 rename**，崩溃不会留下半截坏缓存；加载失败只告警并整份重建。
- **边算边存**：60 秒或新增 10 万条任一触发（`src/cache.rs::CACHE_SAVE_MIN_INTERVAL` /
  `CACHE_SAVE_MIN_NEW_ENTRIES`），且每个摘要阶段结束后都存一次。注释里写着理由——以前只在整轮结束时写一次，
  大工作区上内存耗尽时前面算的全作废，「一次都跑不完的机器上永远跑不完」。

这一层直接对应 README 里的 55s → 14s 和 4.5s → 1s。**那 4 倍差距纯粹来自缓存**，与摘要算法无关。
open 模式与 `--clean` 共用同一份缓存，两个方向互相加速。

## 三、判定「要不要算摘要」的漏斗

在摘要之前还有两道闸，成本递增：

1. **二进制文件 + size 与 depot 不同 → 直接判为改动，零 I/O**（`src/reconcile/analyze.rs`）。
   原生没有这一条。文本文件不能这么判——CRLF 归一化会改变字节数，size 相等不代表内容相等，反之亦然
   （`src/reconcile/analyze.rs` 里有专门的回归用例）。
2. **mtime 与 have 记录的 syncTime 差 ≤1 秒 → 整组跳过摘要**（`src/digest.rs::is_unchanged_since_sync`，
   在 `src/reconcile/mod.rs::reconcile_dir` 里对三组候选分别过滤）。判据是「自 sync 之后没人动过这个文件」，
   而这正是「内容是否与 have revision 一致」的充分条件。运行结束会打印跳过的比例。
3. 缓存三字段命中，见上一节。

> 公平起见：**时间戳捷径原生也有**。`p4 reconcile -m` 就是「比较 depot 里的 sync/submit 时间与工作区文件的
> 修改时间，相同就跳过昂贵的摘要比对」，而 P4V 的日志显示它确实带着 `-m` 调用。所以第 2 条不构成独有优势；p4delta 的差别是
> 容差放宽到 ±1 秒（文件系统与 p4 的秒级时间会互相差一秒），以及它和缓存叠加使用。真正独有的是第 1 条和缓存。

## 四、扫描面更小、往返次数更少

- **忽略目录整棵剪枝**：`src/workspace.rs::collect_workspace_files` 用 `Walker::skip_current_dir()`
  跳过整棵子树，里面的文件连 stat 都不做。剪枝门控很保守（仅 `P4IGNORE=.p4ignore`、无嵌套 `.p4ignore`、
  生效规则不含 `!` 重包含、查询结果可完整识别才启用），任一不满足就回退全量扫描。目录是否被忽略始终由
  `p4 ignores` 判断，不硬编码 `node_modules`、`.git` 这类名字。查询本身也做了分批：根的直接子目录先问
  （大块头通常就在这一层），其余一次问完，已剪子树不再查询——避免每层都启动一次 p4。
- **`p4 where` 只对判定为新增的文件查**（`src/workspace.rs::filter_unmapped_paths`），用来剔除被 client view
  排除行覆盖的路径。注释写明了原因：对全部工作区文件做这件事，在几十万文件的目录上会变成灾难。
- **路径参数走 stdin 批处理**：`p4 -x - -b N`，一批一个进程（`src/p4/process.rs`）。这最初是为绕开
  Windows 命令行按 ANSI 代码页转码导致非 ASCII 文件名失效，顺带也绕开了命令行长度上限。
- **并发上限 8**（`src/p4/process.rs::MAX_PARALLEL_P4_COMMANDS`）。注释写着理由：服务端 `maxParallel=8`，
  再多只是增加进程启动开销——无上限那版曾一度同时起上万个 `p4.exe`。
- **三路并发**：`src/reconcile/mod.rs::reconcile_dir` 用 `futures::join!` 同时跑 depot 查询、本地扫描、
  `p4 -G have`。原生是串行的。
- **`p4 fstat -L`** 一次批量取回旧版本文件的元数据（`src/p4/fstat.rs`），注释称「比逐个文件查询快得多」，
  代价是必须严格校验返回的记录，校验不过就停，绝不用最新版本的摘要顶替 have revision 的。

还有一条严格说不算「更快」，而是「能跑完」：`p4 fstat` 与 `p4 -G have` 在大型工作区上的响应是 GB 级的，
p4delta 全程流式解析（`src/p4/fstat.rs`、`src/p4/marshal.rs`），只对确实要用的字段做字符集解码。
注释里记着代价——整段缓冲再解析正是当年内存耗尽的原因。

## 五、摘要计算本身（这层没有魔法）

摘要算法没得选：必须和 Perforce 服务端的表示逐字节对齐，所以用的是 MD5（`md-5`，刻意没有换成 blake3
或 xxhash 这类更快的哈希）。文本要按 p4 的规则做换行归一化——只删紧邻 `\n` 的那一个 `\r`，行中间的回车
是内容的一部分。符号链接摘要的是链接目标本身（写成「正斜杠目标 + 换行」），不是它指向的文件。

真算的时候靠 rayon 并行（`src/digest.rs::parallel_compute_digests`，用 `with_max_len(1)` 让每个文件成为
不可再分的并行单元），以及 128 KiB 的读缓冲（`src/lib.rs::READ_BUFFER_SIZE`，注释自陈是按 PCIe 4.0 SSD
调的）。没有 memmap。

**所以快的是「哪些文件不用算」，不是「算得多快」。** 把这一层单独拎出来和原生比，两者的摘要吞吐是同一
量级——差的是有多少文件落到这一层。

## clean 为什么也快

`p4 clean` 就是 `p4 reconcile -w`：**它得先把 reconcile 的账全部付完**——扫盘、算摘要、move detection、
与服务端往返——才有资格去修工作区。clean 慢是结构性的，不是 clean 自己的实现问题。

p4delta 的 clean 与 reconcile 共用同一条分析管线和同一份摘要缓存：
`src/reconcile/clean.rs::CleanChanges::project` 只是把八类变更投影成三类，由
`src/reconcile/mod.rs::reconcile_dir` 调用。只有动作层不同：

- **删除**（工作区有、depot 没有）是本地 `std::fs` + rayon 并行，**完全不碰服务端**。
- **还原 / 写回**是批量 `p4 sync -f //depot/file#haveRev`（`src/reconcile/clean.rs::restore_specs`），
  显式钉死 have revision，不走 changelist。

于是 reconcile 侧省下的每一分，clean 全部继承。clean 唯一多出来的是它自己的动作阶段，而那部分要么是纯本地
文件操作，要么是已经批处理化的 `p4 sync`。

## 代价与边界

把话说全，免得这份文档读起来像宣传：

- **判定范围更窄。** 不做 move 配对；Apple/Resource 旧格式和认不出的 `headType` 转交原生 p4；head revision
  是归档版本（archived）的文件跳过并汇报。这些都在 README「已知问题」里。
- **摘要是拿正确性换速度。** 缓存键是秒级的 size + mtime。mtime 被外力改回原值，或碰上时间戳粒度、
  时钟回拨，就会误判成「未变」。这是粒度取舍，不是缺陷，但值得知道边界在哪。
- **时间戳捷径原生也有**（`p4 reconcile -m`），见第三节的说明。
- **没有 benchmark 基建。** 无 `benches/`、无 criterion、CI 里没有性能 job；README 的数字是手工测量的，
  没有原始日志，不可复现。唯一内置的 A/B 开关是 `--no-prune-ignored-dirs`，用来对照目录剪枝的收益
  （见 README「路径与忽略目录优化」）。要据此判断自家仓库的收益，请以预览结果一致性和多轮计时为准。
