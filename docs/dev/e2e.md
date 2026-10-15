# e2e：真实 p4d 沙箱

`tests/e2e_*.rs` 为每个用例起一个独立的 p4d 实例，在真服务器上跑完整流程——八类变更、`--clean` 的三类动作、`--sync --force` 的四组动作、普通 `--sync` 的超集与范围边界、忽略目录剪枝、client view 排除、字符集、缓存复用、操作范围。数据库模板冷建后缓存（`<target>/e2e/template-<指纹>/`），实例从模板复制；省掉的只是固定历史的重复构造，每轮起服务器、铺工作区、跑 CLI 与取证仍是真实开销。

```bash
# 机器上已经有 p4d（比如随 P4V 装的）就能直接跑
cargo nextest run --all-targets --all-features

# 没有的话，下载一份带校验的到 vendor/（CI 走的就是这条路）
bash scripts/fetch-p4-tools.sh
```

探测顺序是 `vendor/` → `<target>/e2e/tools/` → P4V 安装目录 → `PATH`，一个都找不到时打印一行 `skipping: p4d or p4 is not available` 跳过，而不是静默通过。`P4D_EXE` / `P4_EXE` 指定了就以它为准：指到不存在的路径等于强制跳过（想只跑单元测试时好用），指到跑不起来的文件则直接失败——「没有 p4d」和「p4d 起不来」是两回事。这套探测与生产代码的 `src/locate.rs` **刻意不合并**：这里要的是配套的一对 p4 + p4d，顺序也相反（生产的 `PATH` 优先）。

**CI 上不允许跳过**：test job 设了 `P4_E2E_REQUIRED=1`，找不到 p4d/p4 时用例直接失败。`fetch-p4-tools.sh` 的硬失败只保证下载没出错、保证不了二进制真能用；grep 日志也不行——`skipping:` 走 stderr，libtest 默认连同测试输出一起捕获，根本不会出现在 CI 日志里。

## 调试：保留现场

沙箱给被测程序的环境里没有任何继承来的 `P4*` 变量（免得连上你自己的服务器），路径形式的摘要缓存也被圈进实例目录。出问题时用 `P4_KEEP_SANDBOX=1` 保留现场：

```bash
P4_KEEP_SANDBOX=1 cargo nextest run -E 'test(an_edited_file)' --no-capture
```

`-E 'test(...)'` 是按用例名过滤的 filterset（子串匹配，不绑模块路径）；`--no-capture` 让 nextest 串行执行并原样透传输出——保留现场的提示走 stderr，不关掉捕获就看不到。退出时会把实例目录、端口与复核命令一起打出来，**服务器保持运行**（杀掉的话打印出来的端口就是个死端口），复核完要自己停掉它。Windows 上**别把这条命令的输出接进管道**（`| tee`、`| tail`）：保留下来的 p4d 会继承管道的写端句柄，它不退出，管道就永远读不到 EOF——看起来像测试卡死，其实进程早就跑完了。要留日志就重定向到文件。

实例目录在 `<target>/e2e/instances/` 下，随 `cargo clean` 一起清掉。模板在 `<target>/e2e/template-<指纹>/`：指纹含 p4d 身份、种子版本与模板种类，bulk 模板还含 `tests/support/bulk.rs` 的内容指纹。模板**只发布、不修改**，旧目录留在原地——删掉一个正被别的进程读着的模板，会让它复制到一半就没东西可读。nextest 是 process-per-test，冷缓存时多个进程会各建各的 staging、抢 rename，正确性由 rename 的原子性保证，代价是种子重复跑几遍，只影响那一次冷跑；热缓存是一次毫秒级的 stamp 命中。

普通用例用五文件基础模板，bulk 用例另用含 3600 文件、两版提交历史的专用模板。设 `P4DELTA_E2E_TIMING=1` 单跑（配 `--no-capture`）可看到模板查询/构造、实例复制、服务器启动、初始同步与收尾各段耗时，bulk 另列准备、CLI、终态取证三段；那是串行单跑，**不能当全轮计时**。

## 新增用例前先量：一条用例就是整轮的地板

nextest 把每条用例放在一个 worker 上，**并行度切不开一条用例**。所以整轮墙钟 T ≥ 最慢单条 max——夹具造得大的用例会直接把整轮钉在它的耗时上。v0.1.14 引入的 `a_bulk_deep_path_sync_keeps_counts_files_and_have_consistent`（当时 9000 个文件）独占 40.8 秒，CI 的 windows job 因此从 32 秒涨到 103 秒：**是一条用例，不是总量**。

量三个数再决定加不加：

```powershell
$env:P4_E2E_REQUIRED = "1"        # 全程开着：跳过会伪装成「变快」
$env:NEXTEST_EXPERIMENTAL_LIBTEST_JSON = "1"
cargo nextest run --all-targets --all-features --message-format libtest-json > run.jsonl
# T 看 Summary 行；单条耗时从 run.jsonl 里取 event=ok 的 exec_time 排序
# 量 Σ 换 --test-threads 4
```

- **T ≈ max**：地板主导，先砍最慢的那几条；
- **T ≈ Σ / 并发度**：总量主导，砍实例数（合并用例、让沙箱复位而不是重起整个）；
- 都不接近：资源争抢，先找出拖累全体的那条。

三条踩过的坑：

1. **`exec_time` 记的是墙钟**。同一次整轮，Σ 在 16 并发下是 275 秒、4 并发下只有 159 秒——差的那 116 秒是按用例摊到的 I/O 争抢，不是它们的成本。判断「这条用例慢不慢」要低并发量或单跑。
2. **夹具规模按被测门槛反推，不要「多来点更保险」**。bulk 那条的算式写在它自己的注释里（spec 长度 → 每批条数 → 批数门槛），9000 → 6000 → 3600 靠的都是它，断言一字未改。**spec 越长，达到同样批数需要的文件越少**，所以那条「deep path」用例把路径加深反而更快。
3. **动手前先把时间拆开量**。bulk 历史实现里两个 `p4 submit` 占 63%，那是生成历史时的固有成本（`--parallel=threads=4` 试过，无效）；而首轮 `submit` 之后那次 `p4 sync` 是纯冗余——`submit` 已经把 have 更新到新版本，且 r2 末尾的 `sync ...#1` 才是唯一必需的那次同步。先删冗余同步省 4 秒，之后把固定提交历史放进不可变模板，才把 submit 成本移到冷建。不拆开量，很容易把「构造时必需」误当成「每轮都必需」。

**门槛**：新增用例单条超过 3 秒，在注释里写清楚夹具为什么必须这么大、判别力落在哪条断言上；超过 5 秒的考虑是否应作为专项验证，不能为了达标临时排除已有覆盖。

## 性能实测（2026-10-15，三轮）

同一台 Windows 开发机、16 个逻辑处理器、nextest 0.9.146；全程 `P4_E2E_REQUIRED=1` 与进程级 `P4PORT=nonexistent.invalid:1666`，默认集合 491 条，每轮逐名一致、全部通过。

| 时点 | 整轮墙钟中位 |
|---|---|
| 第一轮前 | 33.345 秒（bulk 单条 25 秒，独占整轮地板） |
| 第一轮后 | 18.760 秒 |
| 第二轮后 | 18.292 秒 |
| 第三轮后 | **15.505 秒**（用户原命令三轮 15.634 / 15.505 / 14.939） |

**第一轮：把固定历史移出热路径。** 固定提交历史放进不可变模板（冷建一次、只发布不修改），再给 bulk 那条用例 `priority = 100`，让它先启动、与其余用例重叠。整轮 33.3 → 18.8 秒，bulk 单条 25.0 → 17.2 秒。不缩文件数、不删断言、不共享可写服务器。

**第二轮：减少归档对象。** bulk 热单跑里复制占 2.85 秒，于是让专用模板改用 `text+D` RCS 差分归档：`server/depot` 从 7205 文件 / 3618 目录降到 3605 文件 / 18 目录（归档字节 227 KB → 1.25 MB，**对象数才是瓶颈**）。单跑中位 6.49 → 4.20 秒；整轮 18.29 秒，与第一轮的 18.76 秒几乎相同，因此**不声称整轮有大幅提速**——回切对照（保留元数据改动、换回默认归档）也说明不能把中间几轮的下降全归功于某一次改动。

**第三轮：拆开 Σ 与吞吐上限。** 剖析结论：e2e 116 条占 Σ 的 89%（**126 个沙箱实例**），364 条单元用例只占 33.6 秒、已到进程启动地板；4 并发（近似无争抢）Σ ≈ 149–159 秒，16 并发时 CPU 94–100%；`p4.exe`（10.8 MB）并发启动实测约 **250 个/秒封顶，与并行度无关**。于是只保留两处「减少确定性工作」的脚手架改动：

- **就绪探测并进 `p4 client -i`**（`tests/support/p4d.rs`、`mod.rs`）：实例热路径不再先跑一次 `p4 info` 探测，直接交表单，失败才回落到原两段式探测。每沙箱省一次进程启动，126 个实例合计约 4.3 秒 Σ。
- **四处串行起双沙箱的对拍用例改并行**（`e2e_sync_normal.rs`，`sandbox_pair` 抽成 `pair_with`）：Σ 约 −2.5 秒，solo 单条由约 1.5 秒降到 1.22–1.26 秒。

**验证要用逐轮交替配对，别跨时段比两组数字。** 本机的噪声是**成片**的：同一份代码、同一时段，连续三轮可以是 15.7 / 15.4 / **18.8**，接着 18.9 / 19.0，再下一轮 14.6。逐条对照慢轮与快轮：376 条（几乎全是单元用例）分毫未动，109 条 e2e 均匀慢约 29%——是磁盘侧的间歇干扰，既不是降频也不是 CPU 争抢。所以验证改成 HEAD 与改动版逐轮交替（中间只做增量重编）：第三轮改动的效应是 15.547→15.029、15.189→14.809、15.306→15.387，中位差 **+0.277 秒**（约 2%），两快一平、从不变差。也因此**第三轮的 15.5 秒不能拿去减第二轮的 18.3 秒**：那 2.8 秒的主体是时段差异，改动本身只有 0.27 秒。

**撤回的实验**：bulk `threads-required=2/4`（中位 20.185 / 18.557 秒，范围重叠）；给 `e2e_sync` / `e2e_sync_normal` 加调度优先级（整轮反而 18.289 秒）；把 `--test-threads` 提到 24（交替六轮中位差 0.465 秒，但配对差 +1.200 / +0.183 / −0.201，方向不一致）。`.config/nextest.toml` 与 `ci.yml` 因此保持原样，本地与 CI 共用同一份并发设置。沙箱收尾的退避起步值与 `wait_until` 轮询间隔都在毫秒级，量级小于噪声，不动。

**CI 上的数字只读趋势，别读成回归**：同一天的两次相邻 run、同一份测试代码，windows-latest 整轮墙钟 50.9 / 68.5 秒（Σ 162 / 199 秒），摆动大于这里优化掉的任何一项——分母是共享 runner 的磁盘与邻居负载。判回归要么看同平台连续几次的趋势，要么回本机量。

**结论：仍未达到 10 秒，且按当前边界到不了。** 16 并发时机器已经满载，Σ 里相当一部分是进程启动这类压不满核的工作（250 个/秒封顶），实测地板因此落在 15 秒上下；要进 10 秒得砍掉约 40% 的 Σ，约等于整个 `e2e_sync` 或 `e2e_sync_normal` 二进制——只能靠减少沙箱数量（合并用例）或复用服务器/数据库，两条都在边界之外。

冷建单列：模板首次冷建 27.6–40.3 秒（正常 target 首次整轮命令墙钟 40.302 秒）；全新隔离 target 下两进程并发冷建各 26.081 / 28.749 秒与 31.582 / 31.975 秒，最终只发布一份模板、无 staging 残留。**热态收益不能套到 CI 冷跑上。**

## 测试覆盖

- **clean**（`tests/e2e_clean.rs`）：三类动作（删未跟踪文件、还原改动、写回缺失文件）对着真实服务器验证，断言落在磁盘内容与 `p4 opened` 上；「已打开的文件不归 clean 管」有专门用例；「head 已删除、本地文件重现」用 `p4 clean -n` 的删除判定交叉验证。`clean_compares_against_the_have_revision_after_a_batch_refresh` 钉住「本地停在 have 版本、head 已往前走」时比对基线取的是 **have 版本**（改成用 head 字段这条会红）。三类动作与原生清单的逐文件集合对照尚未自动化。
- **sync --force**（`tests/e2e_sync.rs`）：四组动作同样对着真实服务器验证。`--to <CL>` 用 `p4 fstat` 的 `haveRev` 独立取证（不看工具自己的 stdout）；「默认档会漏、`--verify-all` 才抓得住」与「原生 `p4 sync -f -n` 里非 `refreshing` 的文件必须被我们的清单覆盖」都做成了断言；两条历史基线用例钉住**范围**（`--to` 下只有 have 恰好停在目标版本的记录进补查）与**批量路径**（两条记录一次 `-L` 取回，同尺寸脏内容靠摘要认出来）。删除保护另有五条（都是 `#[cfg(windows)]`），删除候选的两条来路（head 已删除、`--to` 目标时刻还没有该路径）都覆盖，并从 shim 日志里钉住这一轮只起过一次 `ignores -i -v`；`an_injected_verbatim_verdict_deletes_the_candidate` 不依赖本机收不收得下非 ASCII，让成功路径在任何机器上都有确定性覆盖；另两条注入「p4 起来了、退出码 0，但 stdout 是空的／回显的路径是另一个名字」，断言整轮在应用之前停下。故障注入用的是现成的转发 shim（`P4DELTA_SHIM_FAULT_COMMAND` / `P4DELTA_SHIM_FAULT_STDOUT`，`tests/support/p4_shim_main.rs`）。
- **非 ASCII 的环境分档**：p4 在 Windows 上把命令行解析两遍，ANSI 那一遍走系统代码页，表示不了 `使用说明.txt` 的机器上那条通道根本不存在（p4 报 `Argument parsing ambiguity.`，生产代码只能 fail-closed，见 [environment.md](environment.md)）。上面那两条拿非 ASCII 名字当夹具的成功用例因此先问一次 `Sandbox::argv_carries_non_ascii`，通道有损就打印 `skipping:` 跳过——**别把「本地绿」读成「CI 也绿」**；注入式那条与两条 fail-closed 不受这条限制，任何机器上都跑。
- **普通 sync**（`tests/e2e_sync_normal.rs`）：正确性主张只有一个——**与原生 `p4 sync` 逐字一致**，所以几处刻意做成**对拍**：另起一个状态相同的沙箱跑原生 `p4 sync`（或 `...@CL`），比较磁盘、`p4 opened`、`p4 have`、`p4 resolve -n` 四处，断言不看工具自己的 stdout。覆盖：本地改动/缺失原样留着、三类内容动作、已打开的文件（含 have 不在目标版本上的路径，以及反向的一条——have 已在目标版本的已打开文件原生连一句都不说，工具也不许把它报成候选）、`allwrite noclobber` 下的逐文件拒绝（两族 info 必须分开；被拒文件不进记录流、一个字节不动；全被拒时 `total:0` 但 `ok:true`）、clobber 保护、`--to` 走回目标版本、排除目录一个字都不许写、隐式排除的 `.p4delta-scope`、超范围入口在写入前失败、带 p4 元字符与非 ASCII 的路径、以及「普通同步不读摘要缓存、不查 have 时间戳」的直接证据（dry run 前后缓存字节相同）。
- **沙箱服务器的大小写处理**：模板由 `p4d -xi` 建库，Windows 上是 **case insensitive** 的，只差大小写的两个 depot 路径在 e2e 里造不出来。那条路径由单元测试守着（`src/p4/fstat.rs` 的 `snapshot_target_keeps_case_variant_paths_apart`、`case_variant_requests_keep_their_own_baseline`，`src/reconcile/analyze.rs` 的 `case_variant_targets_do_not_collapse_into_a_delete` 与 `a_case_conflict_that_would_delete_a_live_target_is_refused`）；要在这里复现得给种子加 `-C0`，代价是整份 e2e 的路径匹配都变成大小写敏感，与真实 Windows 服务器也更不像。
- **`--to` 的边界**：目标 CL 早于限定子目录首次提交时拒绝执行（断言内容、have、opened 不变）；超过 head 的 CL 回落到 head（读回 `haveRev` 与内容确认没有误触发空目标护栏）；have < 目标 < head 的前向同步不会越过指定目标。目标号都从沙箱已提交记录取得，不依赖固定种子 CL。时间戳捷径的用例在修改内容后恢复实际同步时的 mtime，不依赖测试执行时的当前时钟。
- **删除组的两条腿**：一条腿失败不让另一条不跑，由 `both_delete_legs_report_their_own_failure` 守着——它把文件摁住让删除被系统拒绝，断言两条腿的失败都出现在错误信息里（改造前这条会红，因为前一条腿的 `?` 会让后一条腿整个不执行）。为什么只能用这个十秒级的失败注入：`p4 sync -f //depot/f#none` 面对被别的进程占着的文件会重试约十秒才放弃，而且会先把 `deleted as` 打到 stdout、把 `unlink: ...` 写到 stderr、**have 记录原样保留**——「看起来成功、其实什么都没做」的典型，这正是那组的判据必须是 `ExitCodeOrStderr` 而不能是 `ExitCode` 的原因。换更快的失败注入试过四条候选（只读位、`icacls` 的 DENY ACE、`sys.rename.*` 的退避、目录与符号链接），**全部不成立**，过程见 git 历史。这十秒也落在用例身上，并行度对它无能为力，所以默认档用 `.config/nextest.toml` 的 `default-filter` 把它排掉，CI 的 Windows job 用 `--ignore-default-filter` 补跑回来——门禁不缺这一环（单跑实测 10.8 秒，2026-10-15；更早一次 18.9 秒，退避时长随系统状态浮动）。

## 栈边界（`tests/e2e_stack.rs`）

一批文件的摘要真要现算时（冷缓存 + mtime 被顶出 `have.syncTime ±1s`），rayon 的递归切分每层都曾带一份 128 KiB 的读缓冲——优化构建把它内联进递归帧，实测每层 131,592 字节，默认 2 MiB 线程栈下两万个文件必崩（`fatal runtime error: stack overflow`）。现在缓冲在每线程的堆缓冲里（`src/digest.rs::READ_BUFFER`，机制、测得的数字与「为什么不能是 TLS 数组」都在那里的文档注释里），递归帧降到 968 字节。深度随 log2(N) 增长，改切分策略改不掉它。

**两条用例只在 release 档有意义，dev 档对这条 bug 恒绿**（dev 档不进内联，128 KiB 数组落在叶子帧上）。所以默认档用 `default-filter` 把整个 `e2e_stack` 二进制排掉，CI 由 `test-stack-release` job 在 ubuntu-24.04 与 windows-latest 上 `--release` 补跑：

```bash
cargo nextest run --release --all-targets --all-features --locked \
  --ignore-default-filter -E 'binary(e2e_stack)'
```

三个守护点值得记：一千文件那条**刻意把线程栈压到 1 MiB**（`RUST_MIN_STACK=1048576`）让修复前必然触发——崩溃不是确定性的，在「有时崩有时不崩」的区间里做回归测试没有意义；夹具 `tests/support/mod.rs::bulk_text_files` 里**每个文件的内容都不一样**，否则「上一个文件的残留字节混进摘要」这类回归算出来偏偏是对的；`src/digest.rs::oracle_utf8_digest` 里的那个 `[0u8; READ_BUFFER_SIZE]` **刻意不动**，它是流式改造前逐字保留的对拍 oracle（理由见代码注释）。
