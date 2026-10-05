#Requires -Version 7.0
<#
.SYNOPSIS
    p4delta 的预览基准：多轮计时、动作清单一致性比对、原始输出与结果 JSON 落盘。

.DESCRIPTION
    一次基准 = 1 轮预热 + N 轮计时（默认 5）。预热只用来把「第一次运行」从统计里摘出去
    （进程冷启动、目录第一次被读进 OS 缓存、摘要缓存第一次被写），**不保证摘要缓存全热**：
    sync 的 --verify-all 每轮都绕过摘要缓存重算，那种模式下它本来就是冷的。预热那一轮
    仍然参与动作清单的一致性比对——清单要是随缓存冷热变化，那是结果依赖缓存，不能放过。

    本脚本不读、不清、不备份 p4delta 的摘要缓存：缓存是工具自己的事，预热之后要不要变热
    由它决定。操作系统的文件缓存同样不受控制，跨机器、跨次运行的秒数不可直接比较。

    每轮都固定带 -l（要文件名清单来做一致性比对），**绝不带 -a**：这是预览基准，不改工作区
    也不改 depot。除下面列出的开关外不接受任何额外参数，不做参数透传——要换个模式或加参数
    就显式改这个脚本。

    每轮的 stdout / stderr 原样写进一个新的输出目录（已存在的目录一律拒绝，不覆盖；输出目录
    也不许落在被扫描的目录里，否则基准自己的日志会变成下一轮的新增文件），跑完写
    result.json：参数、每轮耗时与峰值工作集、范围 / 中位数、各轮动作清单是否一致。

    动作清单按「动作 + 完整路径」的**多重集**比对（见 src/reconcile/changes.rs 的
    report_group 输出）：顺序不算差异，重数算（同一文件出现两次与一次不同），路径取全路径
    且按序号比较（不做大小写折叠——那会把两个只差大小写的路径悄悄并成一个）。清单里被转交
    给原生 p4 的那批「不受支持的文件」不会出现，日志里一旦出现这种转交，本次基准直接失败：
    清单不完整，一致性结论没有意义。

.PARAMETER Binary
    要测的 p4delta.exe。测的是 release 构建，一般指 target\release\p4delta.exe。

.PARAMETER Workspace
    传给 p4delta 的工作区名（-w）。

.PARAMETER Path
    传给 p4delta 的起始目录（位置参数）。必须是已存在的本地目录。

.PARAMETER Mode
    open / clean / sync 三选一，默认 open（不带模式开关的那个）。sync 指的是**普通同步**
    （原生 `p4 sync` 的对等物）；强制修复要另加 -Force。

.PARAMETER Rounds
    计时轮数，默认 5，不含预热那一轮。

.PARAMETER To
    仅 sync：--to <changelist>。0（默认）表示不传，即目标为 head。取值范围与 CLI 的 u32
    一致（1..4294967295），负数与非数字在这里就被挡下来。

.PARAMETER Force
    仅 sync：--sync --force，也就是强制修复（「只传真正需要传的文件」的 `p4 sync -f`）。
    不带时是普通同步——它只报原生打算传的文件，候选来自 `p4 sync -n`，不读摘要。

.PARAMETER VerifyAll
    仅 sync --force：--verify-all，不信任 mtime 与摘要缓存的推断——**每轮都会绕过摘要缓存
    重算**，所以这种模式下的秒数天然是「冷缓存」的，预热也改不了这一点。普通同步不读摘要，
    这个开关没有可放大的东西，CLI 那边也要求 `--force`。

.PARAMETER NoPruneIgnoredDirs
    加 --no-prune-ignored-dirs，关掉忽略目录剪枝，用于结果与性能对照。

.PARAMETER OutDir
    输出目录，必须不存在（本脚本不覆盖已有结果），也不能在被扫描的目录里。

.PARAMETER OutDirRoot
    不给 -OutDir 时，在这个目录下按「时间戳-进程号」新建一个输出目录。
    默认 <脚本所在仓库>\target\benchmark。

.EXAMPLE
    .\scripts\benchmark.ps1 -Binary .\target\release\p4delta.exe -Workspace your-workspace -Path E:\project\src
    open 模式，1 轮预热 + 5 轮计时，结果落在 target\benchmark\<时间戳>-<pid>\。

.EXAMPLE
    .\scripts\benchmark.ps1 -Binary .\target\release\p4delta.exe -Workspace your-workspace -Path E:\project\src `
        -Mode sync -To 12345 -Rounds 3
    sync 到 changelist 12345（预演），3 轮计时。

.EXAMPLE
    .\scripts\benchmark.ps1 -Binary .\target\release\p4delta.exe -Workspace your-workspace -Path E:\project\src `
        -Mode sync -Force -VerifyAll
    强制修复的 head 版，每轮绕过摘要缓存重算。
#>
[CmdletBinding()]
param(
    [string] $Binary,
    [string] $Workspace,
    [string] $Path,
    [ValidateSet('open', 'clean', 'sync')] [string] $Mode = 'open',
    [int] $Rounds = 5,
    [int64] $To = 0,
    [switch] $Force,
    [switch] $VerifyAll,
    [switch] $NoPruneIgnoredDirs,
    [string] $OutDir,
    [string] $OutDirRoot
)

$ErrorActionPreference = 'Stop'

$Utf8NoBom = New-Object System.Text.UTF8Encoding($false)

# -l 清单行的形状由 src/reconcile/changes.rs 的 report_group 打印，是三个模式共用的输出
# 契约：9 个空格缩进 + <动作> "<完整路径>"。改成宽松匹配会把别的输出行也吞进来。
$ListingPattern = '^ {9}(?<action>.+?) "(?<path>.+)"\.$'

# p4delta 把「不受支持的文件」转交给原生 p4 时会打这半句（open / clean / sync 三个模式
# 的文案都一样）。那一批文件不会出现在 -l 清单里，所以出现它就意味着清单不完整。
$HandoffPattern = 'not supported by p4delta'

$RepoRoot = Split-Path -Parent $PSScriptRoot
if (-not $OutDirRoot) {
    $OutDirRoot = Join-Path $RepoRoot 'target\benchmark'
}

# 用法错误一次性报全：与其跑到一半才发现 -To 用错了模式，不如在建目录之前就说清。
function Get-UsageErrors {
    $errors = [System.Collections.Generic.List[string]]::new()

    if (-not $Binary) {
        $errors.Add('缺 -Binary：指向要测的 p4delta.exe（如 target\release\p4delta.exe）。')
    } elseif (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
        $errors.Add("-Binary 不存在或不是文件：$Binary")
    }

    if (-not $Workspace) {
        $errors.Add('缺 -Workspace：传给 p4delta 的工作区名。')
    }

    if (-not $Path) {
        $errors.Add('缺 -Path：传给 p4delta 的起始目录。')
    } elseif (-not (Test-Path -LiteralPath $Path -PathType Container)) {
        $errors.Add("-Path 不是已存在的目录：$Path")
    }

    if ($Rounds -lt 1) {
        $errors.Add("-Rounds 至少是 1（现在给的是 $Rounds）。")
    }
    if ($To -lt 0 -or $To -gt [uint32]::MaxValue) {
        # CLI 的 --to 是 u32（0 被解析层当成用法错误，这里用 0 表示「不传」）。
        $errors.Add("-To 超出 changelist 号的取值范围 1..4294967295：$To")
    }
    if ($To -ne 0 -and $Mode -ne 'sync') {
        $errors.Add("-To 只在 -Mode sync 下有意义（--to 是 sync 的目标 changelist）。")
    }
    if ($Force -and $Mode -ne 'sync') {
        $errors.Add('-Force 只在 -Mode sync 下有意义（--force 是强制修复的开关）。')
    }
    if ($VerifyAll -and -not $Force) {
        # CLI 那边 `--verify-all` 就要求 `--force`：普通同步根本不读摘要，放行它等于让用户
        # 以为「验证过全部文件」。这里提前说清，别等 p4delta 以用法错误退出。
        $errors.Add('-VerifyAll 需要 -Force：它放大的只有强制修复的摘要候选，普通同步不读摘要。')
    }

    return $errors.ToArray()
}

function Get-PreviewArguments([string] $TargetPath) {
    # -l 固定带上（一致性比对要用它列出的清单），-a 永远不加。
    # 传进来的是解析后的绝对路径：目录名以 '-' 开头是合法的（如 E:\-temp），而原样传进去
    # 会被 CLI 当成开关。绝对路径不可能是这个形状，等于顺手把这种情况挡掉。
    $result = [System.Collections.Generic.List[string]]::new()
    $result.Add('-l')
    $result.Add('-w')
    $result.Add($Workspace)
    switch ($Mode) {
        'clean' { $result.Add('--clean') }
        'sync' {
            $result.Add('--sync')
            if ($Force) { $result.Add('--force') }
            if ($To -ne 0) {
                $result.Add('--to')
                $result.Add($To.ToString())
            }
            if ($VerifyAll) { $result.Add('--verify-all') }
        }
    }
    if ($NoPruneIgnoredDirs) { $result.Add('--no-prune-ignored-dirs') }
    $result.Add($TargetPath)
    return $result.ToArray()
}

function Get-ActionEntries([string] $Stdout) {
    # 只认清单行，动作与路径都原样保留（含重数，由调用方按多重集比对）。
    $entries = [System.Collections.Generic.List[string]]::new()
    foreach ($line in ($Stdout -split "`r?`n")) {
        $match = [regex]::Match($line, $ListingPattern)
        if ($match.Success) {
            $entries.Add("$($match.Groups['action'].Value)`t$($match.Groups['path'].Value)")
        }
    }
    return $entries.ToArray()
}

# 大小写敏感的字典：Windows 路径大多不区分大小写，但「区分」的那部分（以及将来可能的
# 非 Windows 用法）正是这里要如实报出来的东西，排序也不能让两个只差大小写的键互相顶掉。
function New-OrdinalMap {
    return [System.Collections.Generic.Dictionary[string, int]]::new([System.StringComparer]::Ordinal)
}

function Compare-ActionMultisets([string[]] $Reference, [string[]] $Other) {
    # 多重集比较：先按条目计数，再比计数。用集合或去重会把「同一个文件出现两次」这类差异
    # 抹掉，而工作区在基准期间被改动时，最典型的样子正好就是某个文件的动作条数变了。
    $referenceCounts = New-OrdinalMap
    foreach ($entry in $Reference) {
        if ($referenceCounts.ContainsKey($entry)) { $referenceCounts[$entry]++ } else { $referenceCounts[$entry] = 1 }
    }
    $otherCounts = New-OrdinalMap
    foreach ($entry in $Other) {
        if ($otherCounts.ContainsKey($entry)) { $otherCounts[$entry]++ } else { $otherCounts[$entry] = 1 }
    }

    $keys = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::Ordinal)
    foreach ($key in $referenceCounts.Keys) { [void] $keys.Add($key) }
    foreach ($key in $otherCounts.Keys) { [void] $keys.Add($key) }
    $ordered = [string[]] @($keys)
    # 输出的差异清单要稳定可 diff，所以按键排序；Ordinal 排序与上面的比较口径一致。
    [System.Array]::Sort($ordered, [System.StringComparer]::Ordinal)

    $differences = [System.Collections.Generic.List[string]]::new()
    foreach ($key in $ordered) {
        $expected = if ($referenceCounts.ContainsKey($key)) { $referenceCounts[$key] } else { 0 }
        $actual = if ($otherCounts.ContainsKey($key)) { $otherCounts[$key] } else { 0 }
        if ($expected -ne $actual) {
            $parts = $key -split "`t", 2
            $differences.Add("$($parts[0]) `"$($parts[1])`"：基准 $expected 个，本轮 $actual 个")
        }
    }
    return $differences.ToArray()
}

# 读一次峰值工作集。**读之前必须 Refresh()**：.NET 的 Process 把进程信息（PeakWorkingSet64
# 就在里面）缓存在一份快照里，不 Refresh 就读到旧快照。把刷新和读数绑在一个函数里，
# 就不会有哪次读取漏掉它。
#
# 进程退出后 Refresh 常常取回 0（句柄还在、进程信息已经查不到了），那一次的读数无效；
# 0 与读失败一律记 null——没有数字可报的时候不许造数字。
#
# 参数不写类型：测试用一个模拟 Process 的对象（带 Refresh / PeakWorkingSet64 / WaitForExit）
# 直接调它，针对的就是这两段真实源码。
function Read-PeakWorkingSetBytes($Process) {
    try {
        $Process.Refresh()
        $value = [int64] $Process.PeakWorkingSet64
    } catch {
        return $null
    }
    if ($value -le 0) { return $null }
    return $value
}

# 运行期采样：等进程退出的同时每 $SampleMilliseconds 毫秒 Refresh 一次再读。采样点之间可能
# 漏掉临退出前的峰值，所以读到的是**下界**（JSON 里记成 sampled-lower-bound）；循环结束即进程
# 已退出。
function Measure-PeakWorkingSetBytes($Process, [int] $SampleMilliseconds) {
    $peak = $null
    while (-not $Process.WaitForExit($SampleMilliseconds)) {
        $value = Read-PeakWorkingSetBytes $Process
        if ($null -ne $value -and ($null -eq $peak -or $value -gt $peak)) { $peak = $value }
    }
    return $peak
}

function Invoke-P4deltaOnce([string] $Tag) {
    $stdoutPath = Join-Path $RawDir "$Tag.stdout.txt"
    $stderrPath = Join-Path $RawDir "$Tag.stderr.txt"

    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $BinaryPath
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.CreateNoWindow = $true
    # p4delta 是 Rust，原样往 stdout 写 UTF-8 字节；显式按 UTF-8 解，免得跟着机器代码页变。
    # 带空格与中文的路径只有这样才原样比得出来。
    $startInfo.StandardOutputEncoding = $Utf8NoBom
    $startInfo.StandardErrorEncoding = $Utf8NoBom
    # ArgumentList 按 Windows 的规则逐个引用参数：带空格、中文、引号的路径不会被拆开，
    # 也不需要自己拼命令行字符串。
    foreach ($argument in $Arguments) { $startInfo.ArgumentList.Add($argument) }

    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    $process = [System.Diagnostics.Process]::Start($startInfo)
    try {
        # 两条管道并发读：同步读一条、等另一条把管道缓冲区写满，WaitForExit 会永远不返回。
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()

        # 边等边采样（每 50ms Refresh 一次再读）；循环结束即进程已退出。
        $peakSampled = Measure-PeakWorkingSetBytes $process 50
        # 带超时的 WaitForExit 不等输出冲刷完，这里补一次不带超时的那次再收管道。
        $process.WaitForExit()

        $stdout = $stdoutTask.GetAwaiter().GetResult()
        $stderr = $stderrTask.GetAwaiter().GetResult()
        $stopwatch.Stop()

        $exitCode = $process.ExitCode

        # 退出后再读一次：读得到就是进程的最终峰值（真值），读不到（很常见）就退回采样下界。
        $peak = $null
        $peakSource = $null
        $peakAfterExit = Read-PeakWorkingSetBytes $process
        if ($null -ne $peakAfterExit) {
            $peak = if ($null -ne $peakSampled -and $peakSampled -gt $peakAfterExit) { $peakSampled } else { $peakAfterExit }
            $peakSource = 'post-exit'
        } elseif ($null -ne $peakSampled) {
            $peak = $peakSampled
            $peakSource = 'sampled-lower-bound'
        }
    } finally {
        $process.Dispose()
    }

    [System.IO.File]::WriteAllText($stdoutPath, $stdout, $Utf8NoBom)
    [System.IO.File]::WriteAllText($stderrPath, $stderr, $Utf8NoBom)

    $entries = @(Get-ActionEntries $stdout)
    return [pscustomobject]@{
        tag                     = $Tag
        exit_code               = $exitCode
        seconds                 = [math]::Round($stopwatch.Elapsed.TotalSeconds, 3)
        peak_working_set_bytes  = $peak
        peak_working_set_source = $peakSource
        listing_entries         = $entries.Count
        handoff_to_p4           = [bool] ($stdout -match $HandoffPattern)
        entries                 = $entries
        stdout_file             = "raw/$Tag.stdout.txt"
        stderr_file             = "raw/$Tag.stderr.txt"
    }
}

function Get-BinaryVersion([string] $ExePath) {
    # --version 由 clap 在任何 P4 调用之前处理，不会连服务器。探不到就记 null：
    # 版本只是标注，不是基准的必需品。
    try {
        $startInfo = New-Object System.Diagnostics.ProcessStartInfo
        $startInfo.FileName = $ExePath
        $startInfo.UseShellExecute = $false
        $startInfo.RedirectStandardOutput = $true
        $startInfo.RedirectStandardError = $true
        $startInfo.CreateNoWindow = $true
        $startInfo.StandardOutputEncoding = $Utf8NoBom
        $startInfo.StandardErrorEncoding = $Utf8NoBom
        $startInfo.ArgumentList.Add('--version')

        $process = [System.Diagnostics.Process]::Start($startInfo)
        try {
            $stdoutTask = $process.StandardOutput.ReadToEndAsync()
            $stderrTask = $process.StandardError.ReadToEndAsync()
            $process.WaitForExit()
            $text = $stdoutTask.GetAwaiter().GetResult()
        } finally {
            $process.Dispose()
        }
    } catch {
        return $null
    }

    $line = @($text -split "`r?`n" | Where-Object { $_.Trim() } | Select-Object -First 1)
    if ($line.Count -eq 0) { return $null }
    return $line[0].Trim()
}

function Get-ScriptRepoGitState([string] $Directory) {
    # 记下「这次是用哪份脚本、哪份仓库状态跑的」。注意它说的是**脚本所在仓库**，不是 $Binary
    # 对应的那次提交：本地构建与发布版版本号一样，能看出来的只有脚本这边的改动痕迹。
    # 原生命令不要放进管道里读退出码：那时候 $LASTEXITCODE 取不到值。
    try {
        $revisionLines = & git -C $Directory rev-parse --short HEAD 2>$null
        if ($LASTEXITCODE -ne 0) { return $null }
        $revision = @($revisionLines)[0]
        if (-not $revision) { return $null }
        $statusLines = @(& git -C $Directory status --porcelain 2>$null)
        return [pscustomobject]@{ revision = $revision.Trim(); dirty = ($statusLines.Count -gt 0) }
    } catch {
        return $null
    }
}

# 绝对化 + 忽略大小写的前缀判断：输出目录与被扫描目录互相覆盖时都要拦下来。
function Test-IsInsidePath([string] $Candidate, [string] $Parent) {
    $candidateFull = [System.IO.Path]::GetFullPath($Candidate).TrimEnd('\', '/')
    $parentFull = [System.IO.Path]::GetFullPath($Parent).TrimEnd('\', '/')
    if ($candidateFull.Equals($parentFull, [System.StringComparison]::OrdinalIgnoreCase)) { return $true }
    return $candidateFull.StartsWith($parentFull + [System.IO.Path]::DirectorySeparatorChar,
        [System.StringComparison]::OrdinalIgnoreCase)
}

function Format-Peak($Bytes) {
    if ($null -eq $Bytes) { return '未取到' }
    return ('{0:N1} MB' -f ($Bytes / 1MB))
}

$failure = $null
$warmup = $null
$measuredRounds = [System.Collections.Generic.List[object]]::new()
$stats = $null
$consistency = $null
$binaryVersion = $null
$scriptRepoGit = $null
$startedUtc = [datetime]::UtcNow
$outputCreated = $false

try {
    $usageErrors = @(Get-UsageErrors)
    if ($usageErrors.Count -gt 0) {
        throw ("参数有问题：`n  " + ($usageErrors -join "`n  "))
    }

    $BinaryPath = (Resolve-Path -LiteralPath $Binary).Path
    $PathFull = (Resolve-Path -LiteralPath $Path).Path
    $Arguments = @(Get-PreviewArguments $PathFull)

    # 输出目录一律新建，已有的目录一个字节都不碰：上一轮的结果常常正是要对照的那份。
    if ($OutDir) {
        $OutDir = [System.IO.Path]::GetFullPath($OutDir)
        if (Test-Path -LiteralPath $OutDir) {
            throw "输出目录已存在：$OutDir。本脚本不覆盖已有结果，换一个 -OutDir（或先手工删掉它）。"
        }
    } else {
        $stem = "$([datetime]::Now.ToString('yyyyMMdd-HHmmss'))-$PID"
        $OutDir = Join-Path $OutDirRoot $stem
        $suffix = 2
        while (Test-Path -LiteralPath $OutDir) {
            $OutDir = Join-Path $OutDirRoot "$stem-$suffix"
            $suffix++
        }
    }

    # 输出目录落在被扫描的树里（或反过来包住它），基准自己的日志就会变成下一轮的新增文件，
    # 清单与计时都不再可信。与其悄悄剔除，不如直接拒绝。
    if ((Test-IsInsidePath $OutDir $PathFull) -or (Test-IsInsidePath $PathFull $OutDir)) {
        throw ("输出目录不能与被扫描的目录互相包含：`n  输出：$OutDir`n  扫描：$PathFull`n" +
            '基准自己的日志会在下一次扫描里变成新增文件。用 -OutDir / -OutDirRoot 指到扫描范围之外。')
    }

    # 逐级建父目录（-Force 只对父目录用），最后一级单独建：已存在就在这里失败，
    # 不会把上一轮的结果当成这次的输出目录。
    New-Item -ItemType Directory -Path (Split-Path -Parent $OutDir) -Force | Out-Null
    New-Item -ItemType Directory -Path $OutDir | Out-Null
    $RawDir = Join-Path $OutDir 'raw'
    New-Item -ItemType Directory -Path $RawDir | Out-Null
    $resultPath = Join-Path $OutDir 'result.json'
    $outputCreated = $true

    Write-Host "p4delta 预览基准：$Mode 模式，$Rounds 轮计时（另有 1 轮预热）"
    $binaryVersion = Get-BinaryVersion $BinaryPath
    $scriptRepoGit = Get-ScriptRepoGitState $RepoRoot
    Write-Host "  二进制：$BinaryPath$(if ($binaryVersion) { "（$binaryVersion）" })"
    Write-Host "  工作区：$Workspace"
    Write-Host "  目录：$PathFull"
    Write-Host "  参数：$($Arguments -join ' ')"
    Write-Host "  输出：$OutDir"
    Write-Host ''

    $consistency = [ordered]@{
        reference   = 'round-00-warmup'
        consistent  = $true
        differences = @()
    }

    $warmup = Invoke-P4deltaOnce 'round-00-warmup'
    Write-Host "  预热（不计入统计）：$($warmup.seconds) 秒"
    if ($warmup.exit_code -ne 0) {
        throw "预热那一轮就失败了（退出码 $($warmup.exit_code)）：先看 $RawDir\round-00-warmup.stderr.txt。"
    }
    if ($warmup.handoff_to_p4) {
        throw ('预热那一轮里有文件被转交给原生 p4（日志里的 "not supported by p4delta"）：' +
            '-l 清单不完整，一致性结论没有意义。先处理这批文件，或换一个目录。')
    }
    # 清单只留这一份基准，每轮比对完就把那一轮的丢掉：450k 文件的工作区里一份清单就是
    # 几百 MB，几轮叠在内存里不值得（原文都在 raw\ 下）。
    $referenceEntries = @($warmup.entries)
    $warmup.entries = $null

    for ($index = 1; $index -le $Rounds; $index++) {
        $record = Invoke-P4deltaOnce ('round-{0:d2}' -f $index)
        $record | Add-Member -NotePropertyName index -NotePropertyValue $index
        $measuredRounds.Add($record)
        Write-Host ("  第 {0} 轮：{1} 秒（峰值工作集 {2}，动作 {3} 条）" -f `
                $index, $record.seconds, (Format-Peak $record.peak_working_set_bytes), $record.listing_entries)

        if ($record.exit_code -ne 0) {
            throw "第 $index 轮退出码 $($record.exit_code)：先看 $RawDir\$($record.stderr_file)。"
        }
        if ($record.handoff_to_p4) {
            throw "第 $index 轮里有文件被转交给原生 p4（日志里的 `"not supported by p4delta`"）：-l 清单不完整，一致性结论没有意义。"
        }

        # 与预热那一轮比：清单要是随摘要缓存的冷热变化，那是结果依赖缓存，不能放过。
        $differences = @(Compare-ActionMultisets $referenceEntries $record.entries)
        if ($differences.Count -gt 0) {
            $consistency.consistent = $false
            $consistency.differences = @($differences | Select-Object -First 20)
            throw ("动作清单第 $index 轮与预热轮不一致（$($differences.Count) 处）：" +
                "$($differences[0])。工作区在基准期间被改过，或结果依赖摘要缓存，这轮秒数没有意义。")
        }
        $record.entries = $null
    }

    $seconds = @($measuredRounds | ForEach-Object { $_.seconds })
    $sorted = @($seconds | Sort-Object)
    # 中位数取排序后第 floor(n/2) 个（偶数个取中间两个的均值）。这里必须 Floor：
    # PowerShell 的 [int] 转换是四舍六入五成双，7 轮的 [int](7/2) 会变成 4，
    # 取的不是中位数而是下一个。
    $half = [int] [math]::Floor($sorted.Count / 2)
    $median = if ($sorted.Count % 2 -eq 1) {
        $sorted[$half]
    } else {
        ($sorted[$half - 1] + $sorted[$half]) / 2
    }
    $peaks = @($measuredRounds | Where-Object { $null -ne $_.peak_working_set_bytes } |
            ForEach-Object { $_.peak_working_set_bytes })

    $stats = [ordered]@{
        measured                   = $sorted.Count
        min_seconds                = $sorted[0]
        max_seconds                = $sorted[-1]
        range_seconds              = [math]::Round($sorted[-1] - $sorted[0], 3)
        median_seconds             = [math]::Round($median, 3)
        mean_seconds               = [math]::Round(($seconds | Measure-Object -Average).Average, 3)
        peak_working_set_bytes_max = if ($peaks.Count -gt 0) { [int64] ($peaks | Measure-Object -Maximum).Maximum } else { $null }
    }
    $consistency.listing_entries = $warmup.listing_entries
} catch {
    $failure = $_.Exception.Message
}

# 失败也写 JSON：这次跑到哪、为什么停，都该跟原始日志待在一起。用法错误发生在建目录
# 之前，没有目录可写，那就只报错。
if ($outputCreated) {
    $result = [ordered]@{
        schema      = 1
        started_utc = $startedUtc.ToString('yyyy-MM-ddTHH:mm:ssZ')
        host        = "PowerShell $($PSVersionTable.PSVersion)"
        os          = [System.Runtime.InteropServices.RuntimeInformation]::OSDescription
        result      = if ($failure) { 'failed' } else { 'ok' }
        failure     = $failure
        output_dir  = $OutDir
        parameters  = [ordered]@{
            binary                = $BinaryPath
            binary_version        = $binaryVersion
            workspace             = $Workspace
            path                  = $PathFull
            mode                  = $Mode
            force                 = [bool] $Force
            to                    = if ($To -ne 0) { $To } else { $null }
            verify_all            = [bool] $VerifyAll
            no_prune_ignored_dirs = [bool] $NoPruneIgnoredDirs
            rounds                = $Rounds
            arguments             = $Arguments
        }
        method      = [ordered]@{
            warmup_runs      = 1
            measured_rounds  = $Rounds
            timing           = '墙上时钟：Process.Start 到两条输出管道读完，含读取该轮输出的时间'
            peak_working_set = '只覆盖 p4delta 主进程，不含它拉起的 p4 子进程；每次读之前都 Refresh（.NET 会把进程信息缓存住，不刷新就读到旧快照），退出后读得到最终峰值记 post-exit，读不到（常见）就用每 50ms 刷新一次的采样下界记 sampled-lower-bound，都拿不到记 null，不估算'
            digest_cache     = '脚本不读、不清、不备份摘要缓存；预热只是让「第一次运行」不计入统计，不保证缓存全热（--verify-all 每轮都绕过它）'
            os_cache         = '操作系统的文件缓存不受控制，跨机器、跨次运行的秒数不可直接比较'
            consistency      = '各轮 -l 清单按「动作 + 完整路径」的多重集比对（序号比较，不做大小写折叠）：顺序无关，重数参与比对；预热那一轮也在比对范围内'
            handoff          = '日志里出现 "not supported by p4delta"（有文件被转交给原生 p4）时清单不完整，本次基准直接失败'
            script_repo_git  = '脚本所在仓库的 git 状态，只说明「用哪份脚本跑的」，不是 $Binary 对应的提交'
        }
        script_repo_git = $scriptRepoGit
        # 每轮只记指标与计数，不把成千上万条清单条目抄进 JSON：清单原文在 raw\ 下，
        # 差异细节在 consistency.differences 里。
        warmup      = $warmup | Select-Object tag, exit_code, seconds, peak_working_set_bytes,
            peak_working_set_source, listing_entries, handoff_to_p4, stdout_file, stderr_file
        rounds      = @($measuredRounds | Select-Object tag, index, exit_code, seconds, peak_working_set_bytes,
                peak_working_set_source, listing_entries, handoff_to_p4, stdout_file, stderr_file)
        stats       = $stats
        consistency = $consistency
    }

    try {
        [System.IO.File]::WriteAllText($resultPath, ($result | ConvertTo-Json -Depth 8) + [Environment]::NewLine, $Utf8NoBom)
    } catch {
        Write-Host "error: 结果 JSON 写不进去（$resultPath）：$($_.Exception.Message)" -ForegroundColor Red
        exit 1
    }
}

if ($failure) {
    if ($outputCreated) {
        Write-Host ''
        Write-Host "基准失败：$failure" -ForegroundColor Red
        Write-Host "原始输出与结果保留在 $OutDir"
    } else {
        Write-Host "error: $failure" -ForegroundColor Red
    }
    exit 1
}

Write-Host ''
Write-Host ("  中位数 {0} 秒，最小 {1}，最大 {2}，范围 {3} 秒" -f `
        $stats.median_seconds, $stats.min_seconds, $stats.max_seconds, $stats.range_seconds)
Write-Host ("  峰值工作集（主进程）：{0}" -f (Format-Peak $stats.peak_working_set_bytes_max))
Write-Host "  动作清单：预热 + $Rounds 轮一致（$($consistency.listing_entries) 条）"
Write-Host "  结果：$resultPath"
exit 0
