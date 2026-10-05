# 测试：nextest 与脚本测试

面向改 Rust 代码或 PowerShell 脚本、要跑测试的人。e2e 沙箱见 [e2e.md](e2e.md)，
性能测量与 dry run 比对见 [benchmark.md](benchmark.md)，本地与 CI 的环境差异见
[environment.md](environment.md)。

## nextest

测试跑的是 cargo-nextest 而不是 `cargo test`：cargo 逐个测试二进制**串行**执行；nextest 把所有目标的用例放进同一个调度池，并发度默认等于 CPU 数。装一份：

```powershell
cargo install cargo-nextest --locked     # 或者用 nexte.st 上的预编译包，快得多
```

`.config/nextest.toml` 里配了三条，理由写在文件内注释里：不 fail-fast（一次把失败跑全，而不是首个失败就跳过后面所有）、不重试（不靠重试掩盖 flake）、单用例 180 秒硬超时（nextest 默认只把超时的用例标记为「慢」，不终止进程）。

两点与 `cargo test` 的差别要知道：nextest **不跑 doctest**（本项目目前没有真的 doctest，唯一的文档代码块是 text 类型的；将来真加了要另外补 `cargo test --doc`）；nextest 默认**隐藏通过用例的输出**，`skipping:` 那类提示只在你显式 `--no-capture` 或用例失败时才看得见。

`cargo test --all-targets` 默认 fail-fast：lib 单元测试一失败就**不再跑后面的测试二进制**，`tests/cli.rs` 可能长期没被执行过却看着「测试在跑」。用 `--no-fail-fast` 或直接上 nextest 都能避开。

测试分三处：单元测试跟着被测代码放在各模块的 `#[cfg(test)] mod tests` 里，跨模块复用的测试基建在 `src/test_util.rs`；`tests/` 下是黑盒用例，通过进程边界观察，不引用 crate 内部符号；`tests/support/` 是 e2e 沙箱框架。

## 脚本测试

安装脚本另有黑盒测试，不需要构建产物（它用一个人造 exe 复刻「从 release 包解压出来直接跑」的布局）：

```powershell
pwsh -File scripts/test-install.ps1        # 或 powershell -File ...
```

CI 用 Windows PowerShell 5.1 与 PowerShell 7 各跑一遍。两个脚本都**必须带 UTF-8 BOM**：5.1 对没有 BOM 的文件按系统 ANSI 代码页解析，里面的中文会全变乱码（`test-install.ps1` 有一条断言拦着）。

写/改这些脚本时实测踩过的四条，都不会以「用法错误」的形式报出来：

1. **数组 splat 不按参数名绑定**：`& $script @('-ExePath','X','-InstallDir','Y')` 是把元素**按位置**传过去——`'-ExePath'` 落到第 0 个位置参数上，之后整体错位一格，报错却是 `找不到接受自变量 'Y' 的位置参数`（点名的是一个**值**，看着像参数名写错，其实是 splat 形式错）。按名传参**只能用哈希**：`@{ ExePath = $x; InstallDir = $y }`，开关写 `Force = $true`。（`& $exe @arguments` 传原生 exe 是另一回事，那里没有 PowerShell 参数绑定，数组没问题——`test-install.ps1` 用的就是这种。）
2. **`[CmdletBinding()]` 自带一套公共参数，自己不能再声明同名的**：`Debug`、`Verbose`、`ErrorAction`、`OutVariable`、`PipelineVariable`，以及 `SupportsShouldProcess` 带来的 `WhatIf`/`Confirm`，撞上直接是元数据错误。`Force` 不在公共参数里，可以随便用。改名时小心：只改 `param()` 块的话，函数体里的旧名仍能「跑通」——未定义变量是 `$null`，`if (-not $Debug)` 恒为真，静默走了错误分支；改完 grep 一遍旧名。
3. **同一个开关传两次 = 参数绑定错误**，不是「后一个覆盖前一个」。构建参数**数组**时最容易撞上——重复是运行时拼出来的，报错不指向调用点。项目里的落地：辅助函数默认补 `-WithoutPath`，需要真写 PATH 的用例走单独的 `-WithPath` 开关，不能把默认项塞进调用方的 `ExtraArguments`。
4. **拿子进程输出做文本断言时，中文可能已经变形**：一是流（`Write-Warning` 在 `-File`/`-Command` 下走的是子进程的 **stdout**，不是 stderr），二是编码（非 UTF-8 控制台上中文过管道会被打散成 `?`，匹配中文的断言会退化成**永远通过的空断言**——本机 UTF-8 控制台复现不出来）。断言只匹配消息里刻意留的 **ASCII 记号**（`install.ps1` 的警告文案里留了 `WM_SETTINGCHANGE`）。

最后一条用例（`Test-UserPathIsManagedByDefault`）是唯一碰真实用户配置的：它临时改写 `HKCU\Environment\Path` 来验默认安装会加进去、卸载会摘掉，先快照、`finally` 还原。**别同时跑两份测试**（两份快照会互相覆盖），也别在它跑的当口手工改 PATH；其余用例一律带 `-WithoutPath`，绝不碰这台机器的 PATH。

发布脚本也有黑盒测试，它在一次性 git 仓库里真跑一遍改版本号、提交、打附注 tag、推送，只把 `cargo` 换成垫片：

```bash
bash scripts/test-release.sh
```

部分用例需要机器上有 `p4`（用来读 `.p4ignore`、跑真实 marshal 输出）。缺失时它们会输出 `skipping: p4 is not available` 后跳过，但通过用例的输出默认被捕获，不能靠 CI 日志里没有这行来判断覆盖完整。CI 设置 `P4_E2E_REQUIRED=1`，让缺少工具直接失败。
