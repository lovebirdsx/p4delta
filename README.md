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

## 用法

程序作为 P4V 的自定义工具运行。在 P4V 里打开 `Tools > Manage Custom Tools`，新建一个 Local Tool，按下表填写：

| 字段 | 值 |
| --- | --- |
| Name | `p4delta`（只是菜单里的显示名，取什么都行） |
| Placement | `Custom Tools` |
| Application | release 目录下 `p4delta.exe` 的完整路径 |
| Arguments | `-a -w $c -l %D` |
| Start in | `$r` |
| Add to applicable context menus | 勾选 |
| Run tool in terminal window | 勾选 |
| Refresh Helix P4V upon completion | 勾选 |
| Prompt for arguments、Close window upon completion、Add file browser to prompt dialog、Ignore P4CONFIG files | 不勾 |

Arguments 里的 `$c` 是 P4V 展开的当前 workspace，`%D` 是右键选中的目录，`$r` 是 client 根目录，原样照抄即可。

- 去掉 `-a` 就是预演（dry run）：照常扫描比对并打印结果，但不向 p4 应用任何变更。
- 想从 P4V 里清理工作区的话，再建一个自定义工具：Arguments 填 `--clean -w $c -l %D` 做预演，填 `-a --clean -w $c -l %D` 做实际清理。它的动作**不可逆**，先看「clean 模式」一节。

配置完成后，在工作区里右键一个目录，选最下面的这个自定义工具。

### 非 ASCII 文件名

p4 返回的文件名使用客户端配置的字符集：unicode 模式的服务器上是 `utf8`，旧式服务器上则是 `cp936`、`shiftjis` 之类。工具必须用同一个字符集解码这些输出，否则含非 ASCII 字符的名字无法与本地文件系统报告的路径对上——每个这样的文件都会被报告两次，一次当作新增、一次当作删除。若真的应用这些变更，就会对一个已经存在的文件执行 `p4 add`，并对一个并不存在的路径执行 `p4 delete`。

字符集按以下顺序自动探测：

1. `--charset` 参数，也接受 p4 自己的写法（`utf8`、`cp936`、`shiftjis`……）。
2. `P4CHARSET` 环境变量。
3. `p4 set` 里的 `P4CHARSET`，其次是端口级的 `P4_<port>_CHARSET`。
4. UTF-8。

如果工作区里有非 ASCII 名字而探测失败，请显式设置 `--charset`——在 P4V 里把它追加到 Arguments 字段，例如 `-w $c -l %D --charset cp936`。设错是可见的而不是静默的：日志开头会打印 `Using p4 charset ...`，字符集不匹配会表现为 `-l` 输出里的乱码文件名。

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
  e2e_open.rs          八类变更（10 个用例）
  e2e_clean.rs         --clean 的三类动作（6 个用例）
  e2e_prune.rs         忽略目录剪枝（3 个用例）
  e2e_paths.rs         路径形式 / changelist / 缓存复用 / unmap（4 个用例）
  e2e_charset.rs       非 ASCII 文件名与输出契约（2 个用例）
```

依赖方向自下而上、无环：`cli` / `model` / `path` / `charset` 不依赖 crate 内其他模块；`p4/*` 依赖它们；`prune` / `workspace` / `cache` / `digest` 再往上一层；`reconcile/*` 在最上面。`lib.rs` 只做模块声明，不反向依赖任何模块。

## 发布检查清单

1. 改 `Cargo.toml` 的 `version`（`--version` 输出的就是它）。
2. 同步 `p4delta.exe.manifest` 里的 `version="X.Y.Z.0"`。这是 Win32 程序集版本，与 Cargo 的包版本是两套编号，无法自动同步，漏改不会导致构建失败，只会让文件属性里显示旧版本。
3. `cargo test --all-targets --all-features`
4. `cargo build --release`
5. 在一个真实工作区跑 `-v -l`，确认输出与预期一致。

## 已知问题

它只在我们的仓库上验证过，如果你的配置与我们不同，未必能正常工作。
请先用预演模式确认行为符合预期。你对仓库所做的改动由你自己负责。

- `p4delta` 不实现 move/add 与 move/delete 的配对识别。
- `--clean` 不还原 head revision 是归档版本的文件，只跳过并汇报；Apple/Resource 与认不出的 `headType` 则转交 `p4 clean`。
- `p4 clean -K`（抑制 ktext 关键字展开）没有暴露，clean 始终按 p4 的默认行为展开关键字。
- `--clean` 与 `-c` 同时给出时 `-c` 被忽略（会打印一行告警）。
- depot 路径里含 `#`、`%`、`@`、`*`，或以 `...` 结尾的文件名会被 p4 当成通配符解析，既有的转交路径和 clean 的还原规格都受影响。
