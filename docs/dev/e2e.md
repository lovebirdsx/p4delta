# e2e：真实 p4d 沙箱

`tests/e2e_*.rs` 为每个用例起一个独立的 p4d 实例，在真服务器上跑完整流程——八类变更、`--clean` 的三类动作、`--sync --force` 的四组动作、普通 `--sync` 的超集与范围边界、忽略目录剪枝、client view 排除、字符集、缓存复用、操作范围。数据库模板只生成一次（`<target>/e2e/template-<指纹>/`），之后每个实例从模板复制，所以单个用例的开销在百毫秒级。

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

实例目录在 `<target>/e2e/instances/` 下，随 `cargo clean` 一起清掉。数据库模板在 `<target>/e2e/template-<指纹>/`：指纹由 p4d 的身份和种子版本算出，所以模板是**只发布、不修改**的，换了 p4d 或改了种子只会多出一个新目录，旧的留在原地——删掉一个正被别的进程读着的模板，会让它复制到一半就没东西可读，这比多占几十 KB 糟得多。

nextest 是 process-per-test，与「逐个二进制串行」的 cargo 不同：冷缓存时（`cargo clean` 之后，或换了 p4d、改了种子）多个进程会同时走到 `ensure_template` 的「各建各的 staging、抢 rename」那条路径上。正确性由 rename 的原子性保证，模板只可能被发布一次；代价是种子会被重复跑几遍，只影响那一次冷跑。热缓存下是一次毫秒级的 stamp 命中。

## 测试覆盖

- **clean**（`tests/e2e_clean.rs`）：三类动作（删未跟踪文件、还原改动、写回缺失文件）对着真实服务器验证，断言同时落在磁盘内容与 `p4 opened` 上；「已打开的文件不归 clean 管」有专门用例；「head 已删除、本地文件重现」用 `p4 clean -n` 的删除判定交叉验证。三类动作与原生清单的逐文件集合对照尚未自动化，不应把该个案当成完整对照覆盖。
- **sync --force**（`tests/e2e_sync.rs`）：四组动作同样对着真实服务器验证。`--to <CL>` 那条用 `p4 fstat` 的 `haveRev` 独立取证（不看工具自己的 stdout）；「默认档会漏、`--verify-all` 才抓得住」与「原生 `p4 sync -f -n` 里非 `refreshing` 的文件必须被我们的清单覆盖」都做成了断言。
- **普通 sync**（`tests/e2e_sync_normal.rs`）：正确性主张只有一个——**与原生 `p4 sync` 逐字一致**，所以几处刻意做成**对拍**：另起一个状态相同的沙箱跑原生 `p4 sync`（或 `...@CL`），比较磁盘、`p4 opened`、`p4 have`、`p4 resolve -n` 四处。断言不看工具自己的 stdout，就看这些可独立取证的状态。覆盖：本地改动/本地缺失原样留着、三类内容动作、已打开的文件（含「have 不在目标版本上」这条原生只给 `info` 提示的路径，工具靠补查 `p4 opened` + `p4 fstat` 才认得出来；以及反向的一条——have 已经在目标版本上的已打开文件原生连一句都不说，工具也不许把它报成候选）、clobber 保护与原生同样失败、`--to` 走回目标版本、排除目录一个字都不许写、隐式排除的 `.p4delta-scope`、超范围入口在写入前失败、带 p4 元字符与非 ASCII 的路径、以及「普通同步不读摘要缓存、不查 have 时间戳」的直接证据（dry run 前后缓存字节相同）。
- **`--to` 的边界**：目标 CL 早于限定子目录首次提交时拒绝执行（断言内容、have、opened 不变）；超过 head 的 CL 回落到 head（读回 `haveRev` 与内容确认没有误触发空目标护栏）；have < 目标 < head 的前向同步不会越过指定目标。目标号都从沙箱已提交记录取得，不依赖固定种子 CL。时间戳捷径的用例在修改内容后恢复实际同步时的 mtime，不依赖测试执行时的当前时钟。
- **删除组的两条腿**：一条腿失败不让另一条不跑，由 `both_delete_legs_report_their_own_failure` 守着——它把文件摁住让删除被系统拒绝，断言两条腿的失败都出现在错误信息里（改造前这条会红，因为前一条腿的 `?` 会让后一条腿整个不执行）。

为什么删除组只能用这个十秒级的失败注入：`p4 sync -f //depot/f#none` 面对**被别的进程占着的文件**会重试约十秒才放弃，而且它会先把 `deleted as` 打到 stdout、把 `unlink: ...` 写到 stderr，**have 记录原样保留**——「看起来成功、其实什么都没做」的典型。这正是那一组的判据必须是 `ExitCodeOrStderr` 而不能是 `ExitCode` 的原因（后者会让它整个隐形）。换更快的失败注入试过四条候选，**全部不成立**：只读位会被 p4 自己清掉、`icacls` 的 DENY ACE 在管理员令牌下被绕过（用例 panic 时还会留在实例目录里删不掉）、`sys.rename.*` 的退避读的是客户端里写死的值、目录与符号链接反而让另一条腿成功。过程见 git 历史。

这十秒也落在那条用例身上，而并行度对它无能为力：一条用例只跑在一个 worker 上，它跑到最后就成了整轮墙钟的地板（实测 18.9s；排除它之后是 8.0s）。所以默认档用 `.config/nextest.toml` 的 `default-filter` 把它排掉，CI 的 Windows job 用 `--ignore-default-filter` 补跑回来——门禁不缺这一环，本地 `cargo nextest run` 快一半。

## 栈边界（`tests/e2e_stack.rs`）

一批文件的摘要真要现算时（冷缓存 + mtime 被顶出 `have.syncTime ±1s`），rayon 的递归切分每层都曾带一份 128 KiB 的读缓冲——优化构建把它内联进递归帧，实测每层 131,592 字节，默认 2 MiB 线程栈下两万个文件必崩（`fatal runtime error: stack overflow`）。现在缓冲在每线程的堆缓冲里（`src/digest.rs::READ_BUFFER`，机制、测得的数字与「为什么不能是 TLS 数组」都在那里的文档注释里），递归帧降到 968 字节。深度随 log2(N) 增长，改切分策略改不掉它。

**两条用例只在 release 档有意义，dev 档对这条 bug 恒绿**（dev 档不进内联，128 KiB 数组落在叶子帧上）。所以默认档用 `default-filter` 把整个 `e2e_stack` 二进制排掉，CI 由 `test-stack-release` job 在 ubuntu-24.04 与 windows-latest 上 `--release` 补跑：

```bash
cargo nextest run --release --all-targets --all-features --locked \
  --ignore-default-filter -E 'binary(e2e_stack)'
```

三个守护点值得记：一千文件那条**刻意把线程栈压到 1 MiB**（`RUST_MIN_STACK=1048576`）让修复前必然触发——崩溃不是确定性的，在「有时崩有时不崩」的区间里做回归测试没有意义；夹具 `tests/support/mod.rs::bulk_text_files` 里**每个文件的内容都不一样**，否则「上一个文件的残留字节混进摘要」这类回归算出来偏偏是对的；`src/digest.rs::oracle_utf8_digest` 里的那个 `[0u8; READ_BUFFER_SIZE]` **刻意不动**，它是流式改造前逐字保留的对拍 oracle（理由见代码注释）。
