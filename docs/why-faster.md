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
- **先写独占创建的唯一临时文件，再 rename 发布**，避免并发运行共用临时文件、相互覆盖；发布失败保留旧缓存。成功后清除脏标记，没有新变化时收尾不会再次全量写盘。加载失败只告警并整份重建。
- **阶段间保存**：每个摘要阶段成功完成后检查一次，距离上次保存达到 60 秒或新增 10 万条时落盘
  （`src/cache.rs::CACHE_SAVE_MIN_INTERVAL` / `CACHE_SAVE_MIN_NEW_ENTRIES`）；第一次有脏数据时即可保存，
  整轮结束再保存尚未落盘的变化。它不是计算过程中每 60 秒自动 checkpoint：单个阶段失败时，
  该阶段已算出的摘要仍不会写回缓存，但之前成功保存的阶段可以复用。

这一层直接对应 README 里的 55s → 14s 和 4.5s → 1s。**那 4 倍差距纯粹来自缓存**，与摘要算法无关。
open 模式与 `--clean` 共用同一份缓存，两个方向互相加速。

## 三、判定「要不要算摘要」的漏斗

在摘要之前还有两道闸，成本递增：

1. **二进制文件 + size 与 depot 不同 → 直接判为改动，零 I/O**（`src/reconcile/analyze.rs`）。
   原生没有这一条。文本文件不能这么判——CRLF 归一化会改变字节数，size 相等不代表内容相等，反之亦然
   （`src/reconcile/analyze.rs` 里有专门的回归用例）。
2. **mtime 与 have 记录的 syncTime 差 ≤1 秒 → 跳过摘要**（`src/digest.rs::is_unchanged_since_sync`）。
   文件 mtime 先截断到秒，再与 syncTime 比较；这是“可能自 sync 后未变”的快捷判断，不是内容相同的证明。
   运行结束会打印跳过的比例；需要重新验证内容的 sync 使用 `--sync --force --verify-all`，同时绕过此捷径与摘要缓存。
   （普通 `--sync` 根本不走这条管线——它不扫工作区也不读摘要，见「普通 sync 为什么也快」一节。）
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
- **每次批处理调用的并发上限为 8**（`src/p4/process.rs::MAX_PARALLEL_P4_COMMANDS`），防止一批文件
  启动无界数量的子进程。不同查询可以重叠，因此它不是整个程序的全局进程数上限；是否需要统一预算，
  要以实际服务端压力与吞吐测量为准。
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
不可再分的并行单元——这个开关定的是「每个并行单元装多少个文件」，不是栈深度），以及 128 KiB 的读缓冲
（`src/lib.rs::READ_BUFFER_SIZE`，注释自陈是按 PCIe 4.0 SSD 调的）。没有 memmap。UTF-8 路径将 BOM
嗅探/解码 reader 直接接到缓冲流，不再先存一份完整解码文件；换行归一化仍按行处理，因此超长单行仍会占用
与行长成比例的内存，不能称为严格固定内存。

**那块读缓冲住在每线程的堆缓冲里，不在栈上**（`src/digest.rs::READ_BUFFER`）。这是被一次崩溃逼出来的：
rayon 的切分是递归的，一批 N 个文件的摘要会走到约 log2(N) 层，而优化构建把 `compute_digest_binary`
整个内联进了那个递归帧——缓冲只要还在栈上，每层就要再放一份 128 KiB（实测每层 131,592 字节）。
两万个文件约 15 层，默认 2 MiB 的工作线程栈被压穿，进程以 `fatal runtime error: stack overflow`
直接 abort。挪到堆上之后每层只剩 968 字节，深度再长也与栈大小无关；`tests/e2e_stack.rs` 用
「1 MiB 线程栈 + 一千文件」和「默认栈 + 两万文件」两条用例守住这条边界。

**所以快的是「哪些文件不用算」，不是「算得多快」。** 把这一层单独拎出来和原生比，两者的摘要吞吐是同一
量级——差的是有多少文件落到这一层。

## clean 为什么也快

`p4 clean` 就是 `p4 reconcile -w`：**它得先把 reconcile 的账全部付完**——扫盘、算摘要、move detection、
与服务端往返——才有资格去修工作区。clean 慢是结构性的，不是 clean 自己的实现问题。

p4delta 的 clean 与 reconcile 共用同一条分析管线和同一份摘要缓存：
`src/reconcile/clean.rs::CleanChanges::project` 只是把八类变更投影成三类，由
`src/reconcile/mod.rs::reconcile_dir` 调用。clean 会跳过最终不消费的已打开文件摘要候选，
保留元数据分析与校验；已打开文件多且缓存未命中时，这能省下无效读盘。动作层则不同：

- **删除**（工作区有、depot 没有）是本地 `std::fs` + rayon 并行，**完全不碰服务端**。
- **还原 / 写回**是批量 `p4 sync -f //depot/file#haveRev`（`src/reconcile/clean.rs::restore_specs`），
  显式钉死 have revision，不走 changelist。

于是 reconcile 侧省下的每一分，clean 全部继承。clean 唯一多出来的是它自己的动作阶段，而那部分要么是纯本地
文件操作，要么是已经批处理化的 `p4 sync`。

## 普通 sync 为什么也快（以及它不承诺什么）

`--sync`（不带 `--force`）走的是另一条路：它**不扫工作区、不读摘要、不算摘要，也不碰摘要
缓存**，只做三件事——问原生 `p4 sync -n` 打算传哪些文件、把答案按范围过滤成精确到
`//depot/file#rev` 的规格、把规格交回原生 `p4 sync` 执行。所以上面第一到第五节讲的机制，
它一条都不参与，也就谈不上继承。

省下的是 p4delta 自己的扫描与摘要计算；**原生该传的字节一个不少**，它也不比原生 `p4 sync`
快。它的价值在范围：`.p4delta-scope` 里的排除与 client view 对它是硬边界，原生 p4 没有这个概念。
拿它的秒数去跟 `--force` 档（或 `p4 sync -f`）比是没有意义的——两者做的事不一样。

代价是多两次只读往返：预演真的报了「已打开、have 不在目标版本上」那类提示时，还要补查
`p4 opened` 与 `p4 fstat` 才拿得到那些文件的身份（原生对这类事件只给一句提示文本，路径埋在
文本里）。补查的规模由打开数决定，与工作区大小无关，而且只在预演真的报出那句话时才跑。

## 代价与边界

把话说全，免得这份文档读起来像宣传：

- **判定范围更窄。** 不做 move 配对；Apple/Resource 旧格式和认不出的 `headType` 转交原生 p4；head revision
  是归档版本（archived）的文件跳过并汇报。这些都在 README「已知问题」里。
- **元数据捷径不是内容验证。** 摘要缓存比较路径、size 与完整的 `SystemTime` mtime，代码不主动将缓存
  时间截断到秒；另一个独立的“同步后未变”捷径才是秒级 ±1 秒判断。保留 size 和 mtime 的内容修改可能
  命中旧缓存，落入同步时间窗口的修改可能被捷径跳过；两者不能混为一谈。`--sync --force --verify-all` 会绕过两者。
- **时间戳捷径原生也有**（`p4 reconcile -m`），见第三节的说明。
- **历史数字不等于本轮优化的实测收益。** README 的数据来自手工测量，不是此次改动的 A/B 结果。
  现在有轻量的 `scripts/benchmark.ps1`，记录预览的多轮耗时、主进程峰值工作集与原始日志，并核对
  动作多重集；用法与局限见 [性能测量](dev/benchmark.md#预览性能测量powershell-7)。没有 criterion
  或 CI 耗时硬门禁，也没有分阶段/进程树性能统计。`--no-prune-ignored-dirs` 可用于剪枝对照；
  脚本不清摘要缓存，不控制操作系统缓存，不能把自然预热重复测量叫作冷缓存基准。
