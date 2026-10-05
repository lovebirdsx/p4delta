# 发布与 P4V 验收

## 装本地构建到 P4V 里验证

改完代码想在 P4V 里点一遍——别手工拷 exe，用 `scripts/install-local.ps1`：它构建 → 跑一次
`--version` 冒烟 → 把安装目录里现有的 exe 备份下来 → 调 `install.ps1` 铺好并注册工具。

```powershell
pwsh -File scripts/install-local.ps1              # cargo build --release 后装上
pwsh -File scripts/install-local.ps1 -DebugBuild  # 装 debug 构建，反复改代码时省编译时间
pwsh -File scripts/install-local.ps1 -NoBuild     # 只装已有的产物
pwsh -File scripts/install-local.ps1 -Restore     # 把本地安装之前的那份 exe 放回去
```

备份只做一次（`p4delta.exe.p4delta-backup-<时间戳>`）：连装几次本地构建，能还原回去的「原件」
不会被自己的中间产物顶掉；`-Restore` 放回去之后把它删掉，下次本地安装重新捕捉。P4V 正开着时
会自动带 `-Force`。

装完**不用重启 P4V**：工具定义没变，而 Command 写的是绝对路径，P4V 每次点菜单都新起一个进程。
脚本会打印本地构建的版本号和 git 修订号（有未提交改动会标出来）——本地构建与发布版版本号相同，
装的是哪份代码只能靠它分辨。

`scripts/test-release.sh` 是发布脚本的黑盒测试，见 [testing.md](testing.md)。

## 发布检查清单

1. 跑 `bash scripts/release.sh`。**不写版本号就自动升**：默认补丁号 +1（0.1.3 → 0.1.4），
   升次版本号用 `--minor`（0.1.3 → 0.2.0），主版本号用 `--major`（0.1.3 → 1.0.0）；也可以
   直接写死一个，`bash scripts/release.sh 0.2.0`。想先看一遍加 `--dry-run`。

   它把版本号写进三处——`Cargo.toml` 的 `version`、`p4delta.exe.manifest` 的四段程序集版本、
   `Cargo.lock` 里 `p4delta` 的条目——再跑一遍本地门禁（fmt / clippy / test），然后提交、
   打附注 tag、推送。exe 里的 VERSIONINFO 由 `build.rs` 从 `Cargo.toml` 现算，不用管。门禁想
   跳过用 `--skip-check`，只想在本地备好、暂不推送用 `--no-push`。

   推之前它会先检查：工作区干净、当前在 `main` 上、新版本确实比当前大、tag 本地与远端都
   不存在、本地不落后远端。任何一条不满足都当场拒绝——这些正是「推出去才发现」的坑，
   而 tag 推出去就收不回来了（同一版本号不能发两次，自动升号也一样受这条约束）。

   Git Bash 里直接跑就行；PowerShell 与 cmd 里也可以，只要 PATH 里的 `bash` 是 Git 自带的
   那个（`where bash` 的第一条应当是 `...\Git\usr\bin\bash.exe`）。若第一条是
   `C:\Windows\System32\bash.exe`，那是 WSL，脚本会在另一个文件系统视图里跑，不是你要的——
   这种情况用绝对路径调 Git 自带的那个（路径随你的 Git 安装位置，默认在
   `C:\Program Files\Git\bin\bash.exe`）：`& "<Git>\bin\bash.exe" scripts/release.sh`。
2. 剩下的交给 `.github/workflows/release.yml`：复用 CI 当门禁，构建，断言产物里的版本与 tag
   一致、CRT 是静态链接的，打包 zip 与 `SHA256SUMS`，建 release。盯进度用 `gh run watch`。
3. release 建好后，在一台装了 P4V 的机器上核对一遍：从 zip 跑 `install.ps1` → 重启 P4V →
   `Tools > Manage Tools` 里应出现对应条目 → 右键一个无关紧要的目录跑预演确认输出正常。

   还要核对 sync 那两条入口各自出现在哪个菜单、命令行拼出来对不对：在 **History 视图**右键
   一个已提交的 changelist，`p4delta Sync to changelist` 应当出现（预演输出里的 `--to` 就是
   那一行的号），点它应当弹出问目录的输入框；在 **Workspace / Depot 树**里右键一个目录，
   `p4delta Sync this folder to changelist` 应当出现，点它问 changelist 号。

   这一段 **2026-10-03 已在本机完整对过**：我们手写的节点在 P4V 重写 `customtools.xml` 时原样
   保留（与 P4V 自己 `Export tools...` 的输出比对差异为零），两条 sync 入口都出现在预期位置、
   prompt 留空以退出码 1 报 `No path given`、输出进 P4V 自己的输出窗。之后凡是动过
   `install.ps1` 里元素形状或 Arguments 的改动，这一段就再走一遍。**2026-10-05 动过 Arguments**
   （四条 sync 入口都加上 `--force`，把「回到某个 changelist」与新的普通同步分开：不带 `--force`
   的 `--sync` 不再覆盖未打开文件上的本地改动），元素形状与 prompt 没动——按上面的规矩这段要再
   走一遍，本轮**只在沙箱里验证，没有装进 P4V**，所以还没复验。同样要留意 README 里那条：
   团队那个 `RunTaskAndSyncFiles.bat` 会拿 depot 的 `tools.xml` 覆盖 `customtools.xml`，跑过
   一次它之后 p4delta 的七条就全没了，别把"菜单里没有"误判成工具定义写错了。

   PATH 那条也要手工过一遍——自动化验得了注册表里的值，验不了广播的实际效果：装完**不重新
   登录**，从开始菜单开一个终端（Windows Terminal 若已经在跑，得整个退掉再开：在旧窗口里开
   新标签拿的还是它启动时的旧环境块），`p4delta --version` 应当直接跑起来。再跑一次带
   `-WithoutPath` 的安装确认条目被摘掉、`(Get-Item HKCU:\Environment).GetValueKind('Path')`
   与装之前一致（原值是 `REG_SZ` 的机器上尤其要看一眼：写回时不该被改成 `REG_EXPAND_SZ`）。
