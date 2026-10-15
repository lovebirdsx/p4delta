# e2e：真实 p4d 沙箱

`tests/e2e_*.rs` 为每个用例起一个独立的 p4d 实例，在真服务器上跑完整流程——八类变更、`--clean` 的三类动作、`--sync --force` 的四组动作、普通 `--sync` 的超集与范围边界、忽略目录剪枝、client view 排除、字符集、缓存复用、操作范围。数据库模板冷建后缓存（`<target>/e2e/template-<指纹>/`），之后每个实例从模板复制；这只省掉固定历史的重复构造，每轮启动服务器、铺工作区、执行 CLI 与取证仍是真实开销。

```bash
# 机器上已经有 p4d（比如随 P4V 装的）就能直接跑
cargo nextest run --all-targets --all-features

# 没有的话，下载一份带校验的到 vendor/（CI 走的就是这条路）
bash scripts/fetch-p4-tools.sh
```

探测顺序是 `vendor/` → `<target>/e2e/tools/` → P4V 的安装目录 → `PATH`；一个都找不到时打印一行 `skipping: p4d or p4 is not available` 后跳过，而不是静默通过。`P4D_EXE` / `P4_EXE` 可以直接指定二进制，指定了就以它为准：指到不存在的路径等于强制跳过（想跳过 e2e 只跑单元测试时好用），指到一个跑不起来的文件则直接失败——「这台机器没有 p4d」和「p4d 起不来」是两回事，后者不该被伪装成前者。

这套探测与生产代码的 `src/locate.rs` 是**两套独立的实现**，刻意不合并：这里要的是配套的一对 p4 + p4d（客户端与服务端主版本必须匹配），顺序也相反（生产的 `PATH` 优先）。沙箱只往 `PATH` 里注入，走的是生产探测的第 2 步；`P4_EXE` 在两边含义不同——测试里指错是强制跳过，生产里是配置错误。

**CI 上不允许跳过**：test job 设了 `P4_E2E_REQUIRED=1`，找不到 p4d/p4 时用例直接失败而不是跳过。光有 `fetch-p4-tools.sh` 的硬失败不够——那只保证下载没出错，保证不了二进制真的能用；靠 grep 日志也不行，`skipping:` 走的是 stderr，libtest 默认连同测试输出一起把它捕获了，根本不会出现在 CI 日志里。

## 调试：保留现场

沙箱给被测程序的环境里没有任何继承来的 `P4*` 变量（免得连上你自己的服务器），路径形式的摘要缓存也被圈进实例目录。出问题时用 `P4_KEEP_SANDBOX=1` 保留现场：

```bash
P4_KEEP_SANDBOX=1 cargo nextest run -E 'test(an_edited_file)' --no-capture
```

`-E 'test(...)'` 是按用例名过滤的 filterset（子串匹配，不绑模块路径）；`--no-capture` 让 nextest 串行执行并原样透传输出——保留现场的那几行提示走的是 stderr，不关掉捕获就看不到。

退出时会把实例目录、端口和复核命令一起打出来。**服务器保持运行**——杀掉的话打印出来的端口就是个死端口，现场也就没法看了；代价是复核完要自己停掉它（命令也在提示里）。

Windows 上另有一条：**别把这条命令的输出接进管道**（`| tee`、`| tail` 之类）。保留下来的 p4d 会继承管道的写端句柄，它不退出，管道就永远读不到 EOF，命令一直挂着——看起来像测试卡死，其实进程早就跑完了。要留日志就重定向到文件（`> log.txt`）。

实例目录在 `<target>/e2e/instances/` 下，随 `cargo clean` 一起清掉。数据库模板在 `<target>/e2e/template-<指纹>/`：指纹包含 p4d 的身份、种子版本与模板种类；bulk 模板还包含 `tests/support/bulk.rs` 的内容指纹，改变文件数、路径或版本内容都会冷建新模板。模板是**只发布、不修改**的，旧目录留在原地——删掉一个正被别的进程读着的模板，会让它复制到一半就没东西可读。

普通用例仍使用五文件基础模板；bulk 用例另用含 3600 文件、两版提交历史的专用模板，不把大夹具摊给普通用例。测试实例直接同步到 `#1`，验证 head=2、have=1 与逐文件内容后，再制造缺失文件并运行真实同步。排查冷/热开销时设置 `P4DELTA_E2E_TIMING=1`，单跑用例并带 `--no-capture`，可看到模板查询/构造、实例复制和初始同步三个阶段；不要把这条串行命令当作全轮计时。

nextest 是 process-per-test，与「逐个二进制串行」的 cargo 不同：冷缓存时（`cargo clean` 之后，或换了 p4d、改了种子）多个进程会同时走到 `ensure_template` 的「各建各的 staging、抢 rename」那条路径上。正确性由 rename 的原子性保证，模板只可能被发布一次；代价是种子会被重复跑几遍，只影响那一次冷跑。热缓存下是一次毫秒级的 stamp 命中。

## 新增用例前先量：一条用例就是整轮的地板

nextest 把每条用例放在一个 worker 上，**并行度切不开一条用例**。所以整轮墙钟 T ≥ 最慢单条 max——夹具造得大的用例会直接把整轮钉在它的耗时上。v0.1.14 引入的 `a_bulk_deep_path_sync_keeps_counts_files_and_have_consistent`（当时 9000 个文件）独占 40.8 秒，CI 的 windows job 因此从 32 秒涨到 103 秒：**是一条用例，不是总量**。

量三个数再决定加不加：

```powershell
$env:P4_E2E_REQUIRED = "1"        # 全程开着：跳过会伪装成「变快」
$env:NEXTEST_EXPERIMENTAL_LIBTEST_JSON = "1"
cargo nextest run --all-targets --all-features --message-format libtest-json > run.jsonl
# T 看 Summary 行；单条耗时从 run.jsonl 里取 event=ok 的 exec_time 排序
# 量 Σ 换 --test-threads 4（见下）
```

- **T ≈ max**：地板主导，先砍最慢的那几条；
- **T ≈ Σ / 并发度**：总量主导，砍实例数（合并用例、让沙箱复位而不是重起整个）；
- 都不接近：资源争抢，先找出拖累全体的那条。

三条踩过的坑：

1. **`exec_time` 记的是墙钟**。同一次整轮，Σ 在 16 并发下是 275 秒、4 并发下只有 159 秒——差的那 116 秒是按用例摊到的 I/O 争抢，不是它们的成本。判断「这条用例慢不慢」要低并发量或单跑。
2. **夹具规模按被测门槛反推，不要「多来点更保险」**。bulk 那条的算式写在它自己的注释里（spec 长度 → 每批条数 → 批数门槛），9000 → 6000 → 3600 靠的都是它，断言一字未改。**spec 越长达到同样批数需要的文件越少**，所以那条「deep path」用例把路径加深反而更快。
3. **动手前先把时间拆开量**。bulk 历史实现里两个 `p4 submit` 占 63%，那是生成历史时的固有成本（`--parallel=threads=4` 试过，无效）；而首轮 `submit` 之后那次 `p4 sync` 是纯冗余——`submit` 已经把 have 更新到新版本，且 r2 末尾的 `sync ...#1` 才是唯一必需的那次同步。先删冗余同步省 4 秒，之后把固定提交历史放进不可变模板，才把 submit 成本移到冷建。不拆开量，很容易把「构造时必需」误当成「每轮都必需」。

**门槛**：新增用例单条超过 3 秒，在注释里写清楚夹具为什么必须这么大、判别力落在哪条断言上；超过 5 秒的考虑是否应作为专项验证，不能为了达标临时排除已有覆盖。

### bulk 模板与调度优化实测（2026-10-15）

同一台 Windows 开发机、16 个逻辑处理器，nextest 0.9.146；使用上面的全目标/全特性命令，另设进程级 `P4PORT=nonexistent.invalid:1666` 隔离失败路径，未修改用户配置。编译和模板均已预热，默认集合前后逐名相同，三轮均 **491 条通过**，原有专项排除项未变。

| 指标（秒） | 优化前三轮 | 优化后三轮 |
|---|---|---|
| 整条命令墙钟 | 34.349 / 33.333 / 33.345 | 18.760 / 18.457 / 19.263 |
| nextest 执行时间 T | 33.729 / 32.717 / 32.767 | 18.197 / 17.876 / 18.688 |
| 全轮内 bulk 单条 | 25.696 / 25.012 / 25.241 | 17.203 / 16.961 / 17.572 |

命令墙钟中位数 **33.345 → 18.760 秒，缩短约 44%**；**没有达到 10 秒内**。改动只有固定历史模板缓存与 bulk 的 `priority=100` 优先调度，不缩文件数、不删断言、不共享可写服务器。历史上的 68.4 → 33.5 秒是此前缩减夹具等优化的另一轮记录，不与本次混算。

冷热成本另列：

- bulk 独立热跑：优化前 20.21 秒；优化后三轮 **8.141 / 7.547 / 7.737 秒**。其中实例数据库/归档复制约 3.3–3.7 秒，初始 `sync #1` 约 0.9 秒，这些没有被缓存掉。
- 两个进程同时在全新隔离 target 中冷建同一模板：各 **26.081 / 28.749 秒**，全部原断言通过；随后三次热实例也通过，实例目录全部清理。这里测的是并发冷建，不是单进程冷建基线。正常 target 的基础模板与专用模板也都走过冷建，覆盖了 `p4 admin stop` 成功、p4d 正常退出后才发布的检查（仅本机 Windows 实测）。
- 正常 target 下首次运行（本轮新指纹的基础/bulk 模板尚未建立，测试已编译）：命令 **40.302 秒**，nextest **39.765 秒**。缓存优化把固定历史成本移到了首次运行，不能宣称冷跑也变成 18 秒；CI 若没有命中该夹具缓存也不能套用热态收益。

剩余瓶颈已不只是重复 submit：bulk 单跑与全轮内的差距表明 I/O 争抢明显；全轮内还有大树扫描约 5.1–5.4 秒、忽略查询范围用例约 5.4–6.0 秒等开销。优化后同轮 Σ 为 278–291 秒，因调度重叠增加，比原来的 264–272 秒更高，**不能据此反推固定串行成本或用 Σ/线程数证明不可突破的下界**。进一步追求 10 秒需要新的剖析与夹具/取证成本优化，不是继续调一个等待间隔。

**CI 上的数字只读趋势，别读成回归**：同一天的两次相邻 run、同一份测试代码，windows-latest 的整轮墙钟是 **50.9 / 68.5 秒**（Σ 162 / 199 秒，最慢单条 31.0 / 46.5 秒）——摆动幅度大于这里优化掉的任何一项。分母是共享 runner 的磁盘与邻居负载，不是代码：那两次之间唯一的改动是一次纯 CI 配置修复，它在 ubuntu（16.3→12.6 秒）与 macos（28.7→24.6 秒）上都是下降，而 windows 的两个 job 一起变慢——其中 `e2e_stack` 那个 job 压根不经过被改的路径。判回归要么看同平台连续几次的趋势，要么回本机量。

同一次对照里，那项修复本身的确定性效果可以照常按单条读：两条拿「必然失败的 p4 查询」当夹具的单测在 `P4PORT` 指向不可达地址后从 **11.02 / 10.94 秒**降到 **0.21 / 0.12 秒**（机制见 [environment.md](environment.md) 的「p4 先连服务器」）。

**已有的无收益实验**（2026-10-15，bulk 模板缓存前）：当时默认并发（= CPU 数）比压到 4 更快，后者要 44.7 秒；合并 `e2e_scope` 那 28 条干跑用例最多省十几秒 Σ，折算到墙钟不到 1 秒；沙箱收尾的退避起步值与 `wait_until` 轮询间隔都在毫秒级，改动量级小于测量噪声。墙钟的大头只有两处：最慢的那条用例，和 4 核 CI 上的用例总量。

## 测试覆盖

- **clean**（`tests/e2e_clean.rs`）：三类动作（删未跟踪文件、还原改动、写回缺失文件）对着真实服务器验证，断言同时落在磁盘内容与 `p4 opened` 上；「已打开的文件不归 clean 管」有专门用例；「head 已删除、本地文件重现」用 `p4 clean -n` 的删除判定交叉验证。`clean_compares_against_the_have_revision_after_a_batch_refresh` 钉住「本地停在 have 版本、head 已往前走」时比对基线取的是 **have 版本**的摘要：两个文本文件一次批量补查（≥2 条才走 `p4 fstat -L`），逐字节等于 have 内容就不许判成改动（把回填改成用 head 字段，这条会红）。三类动作与原生清单的逐文件集合对照尚未自动化，不应把该个案当成完整对照覆盖。
- **sync --force**（`tests/e2e_sync.rs`）：四组动作同样对着真实服务器验证。`--to <CL>` 那条用 `p4 fstat` 的 `haveRev` 独立取证（不看工具自己的 stdout）；「默认档会漏、`--verify-all` 才抓得住」与「原生 `p4 sync -f -n` 里非 `refreshing` 的文件必须被我们的清单覆盖」都做成了断言。`a_historical_target_keeps_the_deleted_head_record_untouched` 与 `a_historical_target_refreshes_every_baseline_in_one_batch` 还钉着历史基线补查的两件事：**范围**（`--to` 下只有 have 恰好停在目标版本的记录进补查，把它改回全量第一条会红）与**批量路径**（两条记录一次 `-L` 批量取回，且同尺寸脏内容必须靠摘要认出来、缺失那份按目标版本写回——把回填停掉两件事都会红）。删除保护另有五条（都是 `#[cfg(windows)]`）：`a_non_ascii_delete_candidate_is_judged_by_the_verbatim_ignores_query` 与 `a_non_ascii_candidate_absent_from_the_target_is_judged_verbatim` 覆盖删除候选的两条来路（head 已删除、`--to` 目标时刻还没有该路径），中文目录、含空格的名字、方括号目录与文件名混着上，被规则盖着的留下（连同 have 记录）、没被盖着的照删，并从 shim 日志里钉住这一轮只起过一次 `ignores -i -v`、一次 `add -n` 都没有；`an_injected_verbatim_verdict_deletes_the_candidate` 不依赖这台机器收不收得下非 ASCII——它先跑一轮空输出（必然 fail-closed），从报错里取回生产代码真正发出去的那条候选路径，再原样回一条 `not ignored`，让这一轮真的删掉，成功路径因此在任何机器上都有确定性覆盖；`an_empty_verbose_ignores_output_stops_the_run` 与 `a_mangled_echo_stops_the_run` 注入「p4 起来了、退出码 0，但 stdout 是空的／回显的路径是另一个名字」这两个形态，断言整轮在应用之前停下（同轮里那个本该被 Revert 的文件一个字节都不许动）。故障注入用的是现成的转发 shim：`P4DELTA_SHIM_FAULT_COMMAND` / `P4DELTA_SHIM_FAULT_STDOUT` 把某条命令的输出整段换掉（`tests/support/p4_shim_main.rs`）。

前两条还带一道**环境分档**。p4 在 Windows 上把命令行解析两遍（宽字符一遍、ANSI 一遍），ANSI 那一遍走系统代码页（ACP），表示不了 `使用说明.txt` 的机器上这条命令行通道根本不存在，p4 直接报 `Argument parsing ambiguity.` 失败，成功路径无从执行，生产代码在那里就只能 fail-closed（见 [environment.md](environment.md)）。这两条因此先问一次 `Sandbox::argv_carries_non_ascii`（拿真实 p4 跑一遍 `ignores -i -v`、看回显对不对得上），通道有损就打印一行 `skipping:` 跳过。所以**分档的那两条只在本机 ACP 表示得了这些名字时才真的跑过，别把「本地绿」读成「CI 也绿」**；fail-closed 那两条与上面那条注入式成功用例不受这条限制，任何机器上都跑，但注入式那条证明的是「拿到逐字裁决就会照做」，不是「真实 p4 在 CI 上能给出裁决」。
- **沙箱服务器的大小写处理**：模板由 `p4d -xi` 建库，在 Windows 上是 **case insensitive** 的（`p4 info` 的 `Case Handling`），所以只差大小写的两个 depot 路径在 e2e 里造不出来。那条路径（两条记录落在同一个本地文件上、目标表与补查回填必须各归各的）由单元测试守着，它们不需要服务器：`src/p4/fstat.rs` 的 `snapshot_target_keeps_case_variant_paths_apart` 与 `case_variant_requests_keep_their_own_baseline`、`src/reconcile/analyze.rs` 的 `case_variant_targets_do_not_collapse_into_a_delete`（在库的一条被已删除的一条顶掉就会把本地文件误删）与 `a_case_conflict_that_would_delete_a_live_target_is_refused`。要在这里复现就得给种子加 `-C0`，代价是整份 e2e 的路径匹配都变成大小写敏感，与真实 Windows 服务器也更不像。
- **普通 sync**（`tests/e2e_sync_normal.rs`）：正确性主张只有一个——**与原生 `p4 sync` 逐字一致**，所以几处刻意做成**对拍**：另起一个状态相同的沙箱跑原生 `p4 sync`（或 `...@CL`），比较磁盘、`p4 opened`、`p4 have`、`p4 resolve -n` 四处。断言不看工具自己的 stdout，就看这些可独立取证的状态。覆盖：本地改动/本地缺失原样留着、三类内容动作、已打开的文件（含「have 不在目标版本上」这条原生只给 `info` 提示的路径，工具靠补查 `p4 opened` + `p4 fstat` 才认得出来；以及反向的一条——have 已经在目标版本上的已打开文件原生连一句都不说，工具也不许把它报成候选）、`allwrite noclobber` 下的逐文件拒绝（`can't update/delete modified file`、`can't overwrite existing file`——它们的 `info` 与已打开文件那句同走 notices，必须分开；被拒文件不进记录流、一个字节不动，同一轮的正常文件照常与原生对拍，全被拒时 `total:0` 但 `ok:true`，人类文案不说「已是最新」）、clobber 保护与原生同样失败、`--to` 走回目标版本、排除目录一个字都不许写、隐式排除的 `.p4delta-scope`、超范围入口在写入前失败、带 p4 元字符与非 ASCII 的路径、以及「普通同步不读摘要缓存、不查 have 时间戳」的直接证据（dry run 前后缓存字节相同）。
- **`--to` 的边界**：目标 CL 早于限定子目录首次提交时拒绝执行（断言内容、have、opened 不变）；超过 head 的 CL 回落到 head（读回 `haveRev` 与内容确认没有误触发空目标护栏）；have < 目标 < head 的前向同步不会越过指定目标。目标号都从沙箱已提交记录取得，不依赖固定种子 CL。时间戳捷径的用例在修改内容后恢复实际同步时的 mtime，不依赖测试执行时的当前时钟。
- **删除组的两条腿**：一条腿失败不让另一条不跑，由 `both_delete_legs_report_their_own_failure` 守着——它把文件摁住让删除被系统拒绝，断言两条腿的失败都出现在错误信息里（改造前这条会红，因为前一条腿的 `?` 会让后一条腿整个不执行）。

为什么删除组只能用这个十秒级的失败注入：`p4 sync -f //depot/f#none` 面对**被别的进程占着的文件**会重试约十秒才放弃，而且它会先把 `deleted as` 打到 stdout、把 `unlink: ...` 写到 stderr，**have 记录原样保留**——「看起来成功、其实什么都没做」的典型。这正是那一组的判据必须是 `ExitCodeOrStderr` 而不能是 `ExitCode` 的原因（后者会让它整个隐形）。换更快的失败注入试过四条候选，**全部不成立**：只读位会被 p4 自己清掉、`icacls` 的 DENY ACE 在管理员令牌下被绕过（用例 panic 时还会留在实例目录里删不掉）、`sys.rename.*` 的退避读的是客户端里写死的值、目录与符号链接反而让另一条腿成功。过程见 git 历史。

这十秒也落在那条用例身上，而并行度对它无能为力：一条用例只跑在一个 worker 上，它跑到最后就成了整轮墙钟的地板。所以默认档用 `.config/nextest.toml` 的 `default-filter` 把它排掉，CI 的 Windows job 用 `--ignore-default-filter` 补跑回来——门禁不缺这一环，本地 `cargo nextest run` 快一截（单跑实测 10.8 秒，2026-10-15；更早一次是 18.9 秒——退避重试的时长随系统状态浮动，十秒量级不变）。

## 栈边界（`tests/e2e_stack.rs`）

一批文件的摘要真要现算时（冷缓存 + mtime 被顶出 `have.syncTime ±1s`），rayon 的递归切分每层都曾带一份 128 KiB 的读缓冲——优化构建把它内联进递归帧，实测每层 131,592 字节，默认 2 MiB 线程栈下两万个文件必崩（`fatal runtime error: stack overflow`）。现在缓冲在每线程的堆缓冲里（`src/digest.rs::READ_BUFFER`，机制、测得的数字与「为什么不能是 TLS 数组」都在那里的文档注释里），递归帧降到 968 字节。深度随 log2(N) 增长，改切分策略改不掉它。

**两条用例只在 release 档有意义，dev 档对这条 bug 恒绿**（dev 档不进内联，128 KiB 数组落在叶子帧上）。所以默认档用 `default-filter` 把整个 `e2e_stack` 二进制排掉，CI 由 `test-stack-release` job 在 ubuntu-24.04 与 windows-latest 上 `--release` 补跑：

```bash
cargo nextest run --release --all-targets --all-features --locked \
  --ignore-default-filter -E 'binary(e2e_stack)'
```

三个守护点值得记：一千文件那条**刻意把线程栈压到 1 MiB**（`RUST_MIN_STACK=1048576`）让修复前必然触发——崩溃不是确定性的，在「有时崩有时不崩」的区间里做回归测试没有意义；夹具 `tests/support/mod.rs::bulk_text_files` 里**每个文件的内容都不一样**，否则「上一个文件的残留字节混进摘要」这类回归算出来偏偏是对的；`src/digest.rs::oracle_utf8_digest` 里的那个 `[0u8; READ_BUFFER_SIZE]` **刻意不动**，它是流式改造前逐字保留的对拍 oracle（理由见代码注释）。
