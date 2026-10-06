#Requires -Version 7.0
<#
.SYNOPSIS
    scripts/release.ps1 的黑盒测试。

.DESCRIPTION
    在一个一次性 git 仓库里真跑 release.ps1：origin 是一个本地裸仓库（推送那一步是真推），
    cargo 换成 PATH 最前面的垫片（不编译、不联网，只模拟 `cargo update --workspace` 对唯一
    工作区成员做的那一件事）。被测的三个文件用的是仓库里真实的 Cargo.toml /
    p4delta.exe.manifest / Cargo.lock，格式不会跑偏。

    断言以语言无关的效果为主（文件内容、git 状态、退出码、垫片日志），只在需要区分「是哪条
    检查拦下的」时才匹配消息——消息在 stderr 上，两端都按 UTF-8 固定，见下面的 OutputEncoding。

    不碰真实仓库：一切都在系统临时目录里；全部通过即清，失败保留现场供人看。

.PARAMETER Keep
    通过时也保留现场。
#>
[CmdletBinding()]
param(
    [switch] $Keep
)

$ErrorActionPreference = 'Stop'

# 子进程的输出由这里解码：CI 的控制台代码页与开发机不同（见 docs/dev/environment.md），
# 不钉死的话中文断言会随机器变。release.ps1 写的是 UTF-8，读回来也按 UTF-8。
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)

$RepoRoot = Split-Path -Parent $PSScriptRoot
$SourceScript = Join-Path $PSScriptRoot 'release.ps1'
# 用当前解释器跑子进程：本机是 pwsh 7，CI 上也是同一条腿跑，不另找解释器。
$HostExe = (Get-Process -Id $PID).Path

$Utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$Utf8Bom = New-Object System.Text.UTF8Encoding($true)

$Root = Join-Path ([System.IO.Path]::GetTempPath()) "p4delta-release-test-$PID"
$BinDir = Join-Path $Root 'bin'
$Origin = Join-Path $Root 'origin.git'
$Repo = Join-Path $Root 'repo'
$Other = Join-Path $Root 'other'
# 被测的是临时仓库里那份副本：release.ps1 按自己所在位置推仓库根，跑本体等于对着真实仓库跑。
$RepoScript = Join-Path $Repo 'scripts/release.ps1'
$ShimLog = Join-Path $Root 'cargo.log'
$OutFile = Join-Path $Root 'out.txt'
$ErrFile = Join-Path $Root 'err.txt'

function Assert-True([bool] $Condition, [string] $Message) {
    if (-not $Condition) { throw "断言失败：$Message" }
}

function Assert-Equal([string] $Expected, [string] $Actual, [string] $Message) {
    if ($Expected -ne $Actual) {
        throw "断言失败：$Message`n  期望：[$Expected]`n  实际：[$Actual]"
    }
}

function Assert-Contains([string] $Text, [string] $Needle, [string] $Message) {
    if (-not "$Text".Contains($Needle)) {
        throw "断言失败：$Message`n  没有找到：[$Needle]`n  实际：[$Text]"
    }
}

function Read-Utf8([string] $Path) {
    return [System.IO.File]::ReadAllText($Path, $Utf8NoBom)
}

# 一次性仓库里的 git。非 0 直接失败：用例里没有「预期会失败」的 git 调用。
function Invoke-GitIn([string] $Directory, [string[]] $GitArgs) {
    $out = & git -C $Directory @GitArgs
    $code = $LASTEXITCODE
    if ($code -ne 0) {
        throw "git -C $Directory $($GitArgs -join ' ') 失败（退出码 $code）"
    }
    return $out
}

function Invoke-Git([string[]] $GitArgs) {
    return Invoke-GitIn $Repo $GitArgs
}

function Get-RepoFile([string] $Name) {
    return Read-Utf8 (Join-Path $Repo $Name)
}

function Get-RepoVersion {
    return [regex]::Match((Get-RepoFile 'Cargo.toml'), '(?m)^version = "([^"]*)"$').Groups[1].Value
}

function Get-ShimLog {
    if (-not (Test-Path -LiteralPath $ShimLog)) { return '' }
    return Read-Utf8 $ShimLog
}

function Get-Stdout { return Read-Utf8 $OutFile }
function Get-Stderr { return Read-Utf8 $ErrFile }

function Assert-RepoClean([string] $Message) {
    Assert-Equal '' ((Invoke-Git ('status', '--porcelain')) -join '') "$Message：工作区应当是干净的"
}

# 期望的升级结果，用 [version] 另解一遍——不要拿脚本自己的算法去断言脚本自己。
function Get-ExpectedBump([string] $Version, [string] $Part) {
    $v = [version] $Version
    switch ($Part) {
        'major' { return "$($v.Major + 1).0.0" }
        'minor' { return "$($v.Major).$($v.Minor + 1).0" }
        default { return "$($v.Major).$($v.Minor).$($v.Build + 1)" }
    }
}

# cargo 垫片。真实 cargo 的行为不在这里验证（那是 cargo 自己的事），这里只需要它把
# Cargo.lock 里工作区成员的版本改掉，好让整条链路能走完；顺便把参数记下来，供用例断言
# 门禁那三条命令确实被调用了。
function Write-CargoShim {
    $shim = @'
$ErrorActionPreference = 'Stop'
$Utf8NoBom = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::AppendAllText($env:CARGO_SHIM_LOG, (($args -join ' ') + "`n"), $Utf8NoBom)
# 失败注入：CARGO_SHIM_FAIL 指到哪条子命令，哪条就红，用来验「门禁红了就不提交」。
if ($env:CARGO_SHIM_FAIL -and $env:CARGO_SHIM_FAIL -eq $args[0]) { exit 1 }
if ($args.Count -ge 1 -and $args[0] -eq 'update') {
    # 路径取当前目录而不是相对名：垫片是被 cargo 这个名字调起来的，工作目录由被测脚本决定，
    # 写绝对路径就不必假设它一定落在仓库根。
    $repo = (Get-Location).Path
    $manifest = Join-Path $repo 'Cargo.lock'
    $version = [regex]::Match([System.IO.File]::ReadAllText((Join-Path $repo 'Cargo.toml')), '(?m)^version = "([^"]*)"$').Groups[1].Value
    $lines = [System.IO.File]::ReadAllLines($manifest)
    $out = New-Object System.Collections.Generic.List[string]
    for ($i = 0; $i -lt $lines.Count; $i++) {
        $out.Add($lines[$i])
        if ($lines[$i] -eq 'name = "p4delta"') {
            $i++
            $out.Add('version = "' + $version + '"')
        }
    }
    # 手写 LF 而不是 WriteAllLines：后者用 Environment.NewLine，在 Windows 上会把整份
    # Cargo.lock 换成 CRLF，真实 cargo 不会这么干，被测脚本的断言也就不是对着真实形状了。
    [System.IO.File]::WriteAllText($manifest, (($out -join "`n") + "`n"), $Utf8NoBom)
}
'@
    [System.IO.File]::WriteAllText((Join-Path $BinDir 'cargo.ps1'), $shim, $Utf8Bom)

    # .cmd 交给 cmd.exe 解析：内容只能是 ASCII，行尾必须是 CRLF。正文在 .ps1 里，
    # %~dp0 指到同目录；解释器用 $HostExe 的绝对路径，不指望 PATH 里正好有 pwsh。
    $cmd = "@echo off`r`n" +
        "`"$HostExe`" -NoProfile -ExecutionPolicy Bypass -File `"%~dp0cargo.ps1`" %*`r`n" +
        "exit /b %ERRORLEVEL%`r`n"
    [System.IO.File]::WriteAllText((Join-Path $BinDir 'cargo.cmd'), $cmd, $Utf8NoBom)
}

function Reset-Root {
    if (Test-Path -LiteralPath $Root) {
        Remove-Item -LiteralPath $Root -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

    # 裸仓库也要 -b main：不带的话它的 HEAD 指向 master，clone 出来会是个空工作区。
    & git init --quiet --bare -b main $Origin
    if ($LASTEXITCODE -ne 0) { throw "git init --bare 失败（$Origin）" }
    & git init --quiet -b main $Repo
    if ($LASTEXITCODE -ne 0) { throw "git init 失败（$Repo）" }
    Invoke-Git ('config', 'user.name', 'p4delta 测试') | Out-Null
    Invoke-Git ('config', 'user.email', 'test@example.com') | Out-Null
    # 别让开发机的 gpgsign 配置把提交或 tag 卡住。
    Invoke-Git ('config', 'commit.gpgsign', 'false') | Out-Null
    Invoke-Git ('config', 'tag.gpgsign', 'false') | Out-Null
    # 真实仓库有 .gitattributes（* text=auto eol=lf），这里对应地把行尾转换关掉：
    # 开发机上 core.autocrlf=true 时，git 会把「LF 会被换成 CRLF」的警告刷满屏幕，
    # 断言工作区是否干净也会被这种无关差异干扰。
    Invoke-Git ('config', 'core.autocrlf', 'false') | Out-Null

    New-Item -ItemType Directory -Force -Path (Join-Path $Repo 'scripts') | Out-Null
    Copy-Item -LiteralPath $SourceScript -Destination (Join-Path $Repo 'scripts/release.ps1')
    foreach ($name in 'Cargo.toml', 'Cargo.lock', 'p4delta.exe.manifest') {
        Copy-Item -LiteralPath (Join-Path $RepoRoot $name) -Destination $Repo
    }
    Invoke-Git ('add', '-A') | Out-Null
    Invoke-Git ('commit', '--quiet', '-m', '初始提交') | Out-Null
    Invoke-Git ('remote', 'add', 'origin', $Origin) | Out-Null
    Invoke-Git ('push', '--quiet', '-u', 'origin', 'main') | Out-Null

    Write-CargoShim
    [System.IO.File]::WriteAllText($OutFile, '', $Utf8NoBom)
    [System.IO.File]::WriteAllText($ErrFile, '', $Utf8NoBom)
    [System.IO.File]::WriteAllText($ShimLog, '', $Utf8NoBom)
}

# 跑一次 release.ps1：返回退出码，stdout / stderr 落在 $OutFile / $ErrFile。
# PATH 前置垫片目录、工作目录切到一次性仓库，调用前后都还原；跑的是新进程，
# 输出走文件而不是管道，退出码读 $LASTEXITCODE（原生命令进了管道也读得到，
# 但输出就分不出是 stdout 还是 stderr 了）。
# -EmptyStdin：把子进程的 stdin 接成空管道（等价于非交互下读不到回答），用来验确认那一步。
function Invoke-Release([string[]] $Arguments, [switch] $EmptyStdin) {
    $previousPath = $env:PATH
    $previousLog = $env:CARGO_SHIM_LOG
    try {
        $env:PATH = "$BinDir;$previousPath"
        $env:CARGO_SHIM_LOG = $ShimLog
        Push-Location $Repo
        try {
            if ($EmptyStdin) {
                '' | & $HostExe -NoProfile -ExecutionPolicy Bypass -File $RepoScript @Arguments > $OutFile 2> $ErrFile
            } else {
                & $HostExe -NoProfile -ExecutionPolicy Bypass -File $RepoScript @Arguments > $OutFile 2> $ErrFile
            }
            return $LASTEXITCODE
        } finally {
            Pop-Location
        }
    } finally {
        $env:PATH = $previousPath
        $env:CARGO_SHIM_LOG = $previousLog
    }
}

# ---- 用例 ----

function Test-ScriptLayoutIsSelfContained {
    # 被测脚本与本测试脚本都跟仓库其它 .ps1 一样带 UTF-8 BOM：不带的 .ps1 在
    # Windows PowerShell 5.1 里会按系统 ANSI 代码页解析，中文全变乱码。
    foreach ($script in @($SourceScript, $PSCommandPath)) {
        $head = [System.IO.File]::ReadAllBytes($script)[0..2]
        Assert-True (($head[0] -eq 0xEF) -and ($head[1] -eq 0xBB) -and ($head[2] -eq 0xBF)) `
            "$([System.IO.Path]::GetFileName($script)) 必须以 UTF-8 BOM 开头"
    }
}

function Test-ChildOutputKeepsChinese {
    # 下面那些中文断言全都依赖「子进程写出的 UTF-8 能原样落到文件里」。本机 ACP=65001，
    # 编码问题在本地根本复现不出来（docs/dev/environment.md 讲过这台机器），CI 上却可能变
    # （见 docs/dev/testing.md 第 4 条）。这条先红就先修编码，别去看别的用例的报错——
    # 那些只会说「没找到某某字符串」，看不出是编码问题。
    Reset-Root

    $code = Invoke-Release ('-Help')
    Assert-Equal 0 $code '打印用法不该失败'
    Assert-Contains (Get-Stdout) '版本号有三个去处' '子进程的中文没能原样传出来：查 [Console]::OutputEncoding 与子进程的输出编码'
}

function Test-DryRunChangesNothing {
    Reset-Root
    $before = Get-RepoVersion

    $code = Invoke-Release ('-DryRun', '9.9.9')
    Assert-Equal 0 $code "dry run 不该失败：$(Get-Stderr)$(Get-Stdout)"

    Assert-Equal $before (Get-RepoVersion) 'dry run 不该改版本号'
    Assert-Equal '' ((Invoke-Git ('tag', '-l')) -join '') 'dry run 不该打 tag'
    Assert-Equal '1' (Invoke-Git ('rev-list', '--count', 'HEAD')) 'dry run 不该产生新提交'
    Assert-RepoClean 'dry run 之后'
}

function Test-Releases {
    Reset-Root

    $code = Invoke-Release ('9.9.9', '-Yes')
    Assert-Equal 0 $code "发布失败：$(Get-Stderr)$(Get-Stdout)"

    # 三个文件各改一处
    Assert-Equal '9.9.9' (Get-RepoVersion) 'Cargo.toml 的版本号'
    Assert-Contains (Get-RepoFile 'p4delta.exe.manifest') 'version="9.9.9.0"' 'manifest 的程序集版本'
    Assert-Contains (Get-RepoFile 'p4delta.exe.manifest') 'manifestVersion="1.0"' 'manifest 的 manifestVersion 不该被动'
    Assert-Contains (Get-RepoFile 'Cargo.lock') 'version = "9.9.9"' 'Cargo.lock 里 p4delta 的版本'

    # 门禁三条命令都跑过，而且 Cargo.lock 是走 cargo 刷新的
    $log = Get-ShimLog
    Assert-Contains $log 'fmt --all -- --check' '门禁应当跑 fmt'
    Assert-Contains $log 'clippy --all-targets' '门禁应当跑 clippy'
    Assert-Contains $log 'nextest run --all-targets' '门禁应当跑 test'
    Assert-Contains $log 'update --workspace' '应当刷新 Cargo.lock'

    # 提交只含那三个文件
    Assert-Equal 'chore: 发布 v9.9.9' ((Invoke-Git ('log', '-1', '--pretty=%s')) -join '') '提交标题'
    $files = (Invoke-Git ('show', '--pretty=format:', '--name-only', 'HEAD') | Where-Object { $_ } | Sort-Object) -join ' '
    Assert-Equal 'Cargo.lock Cargo.toml p4delta.exe.manifest' $files '提交里的文件'

    # 附注 tag，指向刚提交的提交，并且推到了 origin
    Assert-Equal 'tag' ((Invoke-Git ('cat-file', '-t', 'v9.9.9')) -join '') 'v9.9.9 应当是附注 tag'
    Assert-Equal ((Invoke-Git ('rev-parse', 'HEAD')) -join '') ((Invoke-Git ('rev-parse', 'v9.9.9^{commit}')) -join '') `
        'tag 应当指向刚提交的提交'
    Assert-Equal 'v9.9.9' ((Invoke-GitIn $Origin ('tag', '-l', 'v9.9.9')) -join '') 'tag 应当推到远端'
    Assert-Equal ((Invoke-Git ('rev-parse', 'HEAD')) -join '') ((Invoke-GitIn $Origin ('rev-parse', 'main')) -join '') `
        '分支应当推到远端'
    Assert-RepoClean '发布之后'
}

function Test-AutoBumpsPatch {
    Reset-Root
    $before = Get-RepoVersion

    $code = Invoke-Release ('-SkipCheck', '-Yes')
    Assert-Equal 0 $code "不带版本号发布失败：$(Get-Stderr)$(Get-Stdout)"

    $after = Get-RepoVersion
    Assert-Equal (Get-ExpectedBump $before 'patch') $after '不带版本号应当把补丁号 +1'
    Assert-Equal "v$after" ((Invoke-GitIn $Origin ('tag', '-l')) -join '') 'tag 用的应当是自动升出来的版本'
    Assert-Contains (Get-Stdout) '自动升 patch' '应当说明版本号是自动升的'
}

function Test-AutoBumpsMinor {
    Reset-Root
    $before = Get-RepoVersion

    $code = Invoke-Release ('-Minor', '-SkipCheck', '-Yes')
    Assert-Equal 0 $code "-Minor 发布失败：$(Get-Stderr)$(Get-Stdout)"

    $after = Get-RepoVersion
    Assert-Equal (Get-ExpectedBump $before 'minor') $after '-Minor 应当升次版本号并把补丁号归零'
}

function Test-AutoBumpsMajor {
    Reset-Root
    $before = Get-RepoVersion

    $code = Invoke-Release ('-Major', '-SkipCheck', '-Yes')
    Assert-Equal 0 $code "-Major 发布失败：$(Get-Stderr)$(Get-Stdout)"

    $after = Get-RepoVersion
    Assert-Equal (Get-ExpectedBump $before 'major') $after '-Major 应当升主版本号并把后面归零'
}

function Test-AutoBumpRespectsExistingTags {
    Reset-Root
    # 自动升出来的那个版本已经有 tag 了（比如上一次发布卡在推送之后）——
    # 自动升号不该把「同一版本不能发两次」这条绕过。
    $next = Get-ExpectedBump (Get-RepoVersion) 'patch'
    Invoke-Git ('push', '--quiet', 'origin', "HEAD:refs/tags/v$next") | Out-Null

    $code = Invoke-Release ('-Yes')
    Assert-True ($code -ne 0) "自动升出来 v$next 已经有 tag 了，不该继续"
    Assert-Contains (Get-Stderr) "远端已经有 tag v$next" '错误信息'
}

function Test-RejectsVersionWithBumpFlag {
    Reset-Root
    $before = Get-RepoVersion

    $code = Invoke-Release ('9.9.9', '-Minor', '-Yes')
    Assert-True ($code -ne 0) '既给了版本号又给了 -Minor 时不该继续'
    Assert-Contains (Get-Stderr) '就不要再给' '错误信息'
    Assert-Equal $before (Get-RepoVersion) '拒绝时不该改版本号'
}

function Test-SkipCheckSkipsTheGate {
    Reset-Root

    $code = Invoke-Release ('9.9.9', '-SkipCheck', '-Yes')
    Assert-Equal 0 $code "发布失败：$(Get-Stderr)$(Get-Stdout)"

    $gate = @((Get-ShimLog) -split "`r?`n" | Where-Object { $_ -match '^(fmt|clippy|nextest) ' })
    Assert-Equal 0 $gate.Count "-SkipCheck 不该跑门禁，实际调用：$(Get-ShimLog)"
    Assert-Contains (Get-ShimLog) 'update --workspace' '-SkipCheck 不该连 Cargo.lock 的刷新一起跳过'
}

function Test-GateFailureStopsTheRelease {
    Reset-Root
    # 门禁红了就不该留下提交和 tag —— 这是发布脚本存在的理由，也是最该有人盯着的一条。
    # 版本号这时已经写进工作区了，脚本故意不回滚：现场留给用户自己看、自己 git checkout。
    $env:CARGO_SHIM_FAIL = 'fmt'
    try {
        $code = Invoke-Release ('9.9.9', '-Yes')
    } finally {
        Remove-Item Env:\CARGO_SHIM_FAIL -ErrorAction SilentlyContinue
    }

    Assert-True ($code -ne 0) '门禁失败时不该继续'
    Assert-Contains (Get-Stderr) 'fmt' '错误信息里应当点名是哪条门禁红了'
    Assert-Equal '1' (Invoke-Git ('rev-list', '--count', 'HEAD')) '门禁失败时不该产生提交'
    Assert-Equal '' ((Invoke-Git ('tag', '-l')) -join '') '门禁失败时不该打 tag'
    Assert-Equal '9.9.9' (Get-RepoVersion) '改动应当留在工作区里等人处理，而不是被脚本悄悄回滚'
}

function Test-NoPushStopsBeforePushing {
    Reset-Root

    $code = Invoke-Release ('9.9.9', '-SkipCheck', '-NoPush')
    Assert-Equal 0 $code "-NoPush 不该失败：$(Get-Stderr)$(Get-Stdout)"

    Assert-Equal 'v9.9.9' ((Invoke-Git ('tag', '-l')) -join '') '本地应当已经提交并打好 tag'
    Assert-Equal '' ((Invoke-GitIn $Origin ('tag', '-l')) -join '') '远端不该有 tag'
    Assert-Equal '1' (Invoke-GitIn $Origin ('rev-list', '--count', 'main')) '远端不该收到新提交'
    # 重推命令是 ASCII，正好不受编码影响
    Assert-Contains (Get-Stdout) 'git push origin main && git push origin v9.9.9' '应当给出重推命令'
}

function Test-DeclinedConfirmationDoesNotPush {
    Reset-Root

    # 不带 -Yes、stdin 又读不到回答（非交互）：应当停在本地，而不是把提交推上去或直接崩掉。
    $code = Invoke-Release ('9.9.9', '-SkipCheck') -EmptyStdin
    Assert-Equal 0 $code "没确认时不该失败：$(Get-Stderr)$(Get-Stdout)"

    Assert-Equal 'v9.9.9' ((Invoke-Git ('tag', '-l')) -join '') '本地应当已经提交并打好 tag'
    Assert-Equal '' ((Invoke-GitIn $Origin ('tag', '-l')) -join '') '没确认就不该推 tag'
    Assert-Equal '1' (Invoke-GitIn $Origin ('rev-list', '--count', 'main')) '没确认就不该推分支'
}

function Test-RejectsDirtyTree {
    Reset-Root
    # 拿一个已跟踪的文件弄脏工作区（版本号那一行还在，所以挡住它的只可能是这条检查）
    [System.IO.File]::AppendAllText((Join-Path $Repo 'p4delta.exe.manifest'), "`n", $Utf8NoBom)
    $before = Get-RepoVersion

    $code = Invoke-Release ('9.9.9', '-Yes')
    Assert-True ($code -ne 0) '工作区不干净时不该继续'
    Assert-Contains (Get-Stderr) '工作区不干净' '错误信息'
    Assert-Contains (Get-Stderr) 'p4delta.exe.manifest' '错误信息里应当带上是哪个文件脏了'
    Assert-Equal $before (Get-RepoVersion) '拒绝时不该改版本号'
    Assert-Equal '' ((Invoke-Git ('tag', '-l')) -join '') '拒绝时不该打 tag'
}

function Test-RejectsExistingTag {
    Reset-Root
    Invoke-Git ('tag', '-a', 'v9.9.9', '-m', 'v9.9.9') | Out-Null
    $before = Get-RepoVersion

    $code = Invoke-Release ('9.9.9', '-Yes')
    Assert-True ($code -ne 0) '本地已有同名 tag 时不该继续'
    Assert-Contains (Get-Stderr) '本地已经有 tag' '错误信息'
    Assert-Equal $before (Get-RepoVersion) '拒绝时不该改版本号'
}

function Test-RejectsRemoteTag {
    Reset-Root
    # 只推到远端，本地没有 —— 挡住它的必须是远端那一条检查
    Invoke-Git ('push', '--quiet', 'origin', 'HEAD:refs/tags/v9.9.9') | Out-Null

    $code = Invoke-Release ('9.9.9', '-Yes')
    Assert-True ($code -ne 0) '远端已有同名 tag 时不该继续'
    Assert-Contains (Get-Stderr) '远端已经有 tag' '错误信息'
}

function Test-RejectsBadVersion {
    Reset-Root

    $code = Invoke-Release ('9.9', '-Yes')
    Assert-True ($code -ne 0) '两段的版本号不该被接受'
    Assert-Contains (Get-Stderr) 'X.Y.Z' '错误信息'

    $code = Invoke-Release ('v9.9.9', '-Yes')
    Assert-True ($code -ne 0) '带 v 前缀的版本号不该被接受（v 由脚本自己加）'
    Assert-Equal '1' (Invoke-Git ('rev-list', '--count', 'HEAD')) '拒绝时不该产生提交'
}

function Test-RejectsVersionNotGreater {
    Reset-Root
    $current = Get-RepoVersion

    $code = Invoke-Release ($current, '-Yes')
    Assert-True ($code -ne 0) '重发同一版本不该被接受'
    Assert-Contains (Get-Stderr) '不比当前的' '错误信息'

    $code = Invoke-Release ('0.0.1', '-Yes')
    Assert-True ($code -ne 0) '更小的版本号不该被接受'
    Assert-Equal $current (Get-RepoVersion) '拒绝时不该改版本号'
}

function Test-RejectsWrongBranch {
    Reset-Root
    Invoke-Git ('checkout', '--quiet', '-b', 'feature') | Out-Null

    $code = Invoke-Release ('9.9.9', '-Yes')
    Assert-True ($code -ne 0) '不在 main 上时不该继续'
    Assert-Contains (Get-Stderr) '当前在 feature 分支上' '错误信息'
    Assert-Equal '' ((Invoke-Git ('tag', '-l')) -join '') '拒绝时不该打 tag'
}

function Test-RejectsDivergedRemote {
    Reset-Root
    # 别人往 origin 推了一个提交，本地没跟上 —— tag 会打在旧提交上，必须先拦住
    & git clone --quiet $Origin $Other
    if ($LASTEXITCODE -ne 0) { throw 'git clone 失败' }
    Invoke-GitIn $Other ('config', 'user.name', '别人') | Out-Null
    Invoke-GitIn $Other ('config', 'user.email', 'other@example.com') | Out-Null
    Invoke-GitIn $Other ('config', 'core.autocrlf', 'false') | Out-Null
    [System.IO.File]::WriteAllText((Join-Path $Other 'notes.txt'), "x`n", $Utf8NoBom)
    Invoke-GitIn $Other ('add', '-A') | Out-Null
    Invoke-GitIn $Other ('commit', '--quiet', '-m', '别人推的提交') | Out-Null
    Invoke-GitIn $Other ('push', '--quiet', 'origin', 'main') | Out-Null

    $code = Invoke-Release ('9.9.9', '-Yes')
    Assert-True ($code -ne 0) '本地落后远端时不该继续'
    Assert-Contains (Get-Stderr) '落后' '错误信息'
    Assert-Equal '1' (Invoke-Git ('rev-list', '--count', 'HEAD')) '拒绝时不该产生提交'
}

# ---- 跑 ----

$cases = @(
    'Test-ScriptLayoutIsSelfContained'
    'Test-ChildOutputKeepsChinese'
    'Test-DryRunChangesNothing'
    'Test-Releases'
    'Test-AutoBumpsPatch'
    'Test-AutoBumpsMinor'
    'Test-AutoBumpsMajor'
    'Test-AutoBumpRespectsExistingTags'
    'Test-RejectsVersionWithBumpFlag'
    'Test-SkipCheckSkipsTheGate'
    'Test-GateFailureStopsTheRelease'
    'Test-NoPushStopsBeforePushing'
    'Test-DeclinedConfirmationDoesNotPush'
    'Test-RejectsDirtyTree'
    'Test-RejectsExistingTag'
    'Test-RejectsRemoteTag'
    'Test-RejectsBadVersion'
    'Test-RejectsVersionNotGreater'
    'Test-RejectsWrongBranch'
    'Test-RejectsDivergedRemote'
)

$failed = 0
Write-Host "release.ps1 测试（$([System.IO.Path]::GetFileName($HostExe))）"

foreach ($case in $cases) {
    try {
        & $case
        Write-Host "  ok   $case"
    } catch {
        $failed++
        Write-Host "  FAIL $case"
        Write-Host "       $($_.Exception.Message)"
    }
}

if ($failed -gt 0) {
    Write-Host "$failed 个用例失败，现场保留在 $Root"
    exit 1
}
if (-not $Keep) {
    Remove-Item -LiteralPath $Root -Recurse -Force -ErrorAction SilentlyContinue
}
Write-Host '全部通过'
exit 0
