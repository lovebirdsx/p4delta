#Requires -Version 7.0
<#
.SYNOPSIS
    benchmark.ps1 的黑盒测试：用假 CLI（绝不连 P4）跑基准，断言参数、计时、日志与失败行为。

.DESCRIPTION
    假 CLI 是一对 fake-cli.cmd + fake-cli.ps1：cmd 只负责把参数原样转交（这正是
    ProcessStartInfo.ArgumentList 引用参数时要走的一跳），ps1 按 fake.plan.txt 的指示产出
    stdout / stderr 与退出码，并把每次收到的参数向量落盘。整个测试不需要 cargo build，
    也不碰真实 P4——被调用的「二进制」从头到尾都是这个假货。

    覆盖：预热 + 多轮计时的语义（含 7 轮中位数）、只传预览参数（必须有 -l、绝不能有 -a）、
    带空格与中文的路径与工作区名、带中文的 stderr、退出码非 0 时失败并保留日志、动作清单
    变化（重数 / 集合 / 只差大小写）时失败、预热清单与计时轮不一致时失败、有文件被转交给
    原生 p4 时拒绝给结论、输出目录不覆盖也不落在扫描树里、摘要缓存不被删除、参数校验在
    启动 CLI 之前。

    断言失败即 throw，退出码非 0，现场（临时目录）保留；风格对齐 scripts/test-install.ps1。

    不碰用户的东西：临时目录、假 LOCALAPPDATA、假工作区目录全在系统临时目录里。
#>
[CmdletBinding()]
param(
    # 失败时保留现场，方便手工看生成的文件。
    [switch] $Keep
)

$ErrorActionPreference = 'Stop'

# 假 CLI 靠 .cmd 把参数转交给 PowerShell，这套只在 Windows 上有意义。
if (-not $IsWindows) {
    Write-Host 'skip：这个测试只针对 Windows（假 CLI 用 .cmd 转交参数）。'
    exit 0
}

$RepoRoot = Split-Path -Parent $PSScriptRoot
$SourceScript = Join-Path $PSScriptRoot 'benchmark.ps1'
$Utf8NoBom = New-Object System.Text.UTF8Encoding($false)

# 用当前解释器跑子进程：被测脚本要求 PowerShell 7，用别的解释器跑出来的结论不算数。
$HostExe = (Get-Process -Id $PID).Path

$Root = Join-Path ([System.IO.Path]::GetTempPath()) "p4delta-benchmark-test-$PID"
# 目录名与工作区名都带空格和中文：参数引用（ArgumentList、cmd 转交）出错就会露。
$FakeDir = Join-Path $Root '假 CLI 目录'
$WorkDir = Join-Path $Root '工 作 区'
$PathDir = Join-Path $WorkDir '子 目录 空格'
$PathResolved = ''
$Workspace = 'p4 工作区'

function Assert-True($Condition, [string] $Message) {
    if (-not $Condition) {
        throw "断言失败：$Message"
    }
}

function Assert-Equal($Expected, $Actual, [string] $Message) {
    if ($Expected -ne $Actual) {
        throw "断言失败：$Message`n  期望：[$Expected]`n  实际：[$Actual]"
    }
}

# 参数向量要逐个元素比，不能只比个数或拼起来的字符串：带空格的路径被拆成两个参数时，
# 拼起来看着一样，逐个比才露。
function Assert-SequenceEqual([string[]] $Expected, [string[]] $Actual, [string] $Message) {
    Assert-Equal $Expected.Count $Actual.Count "$Message（元素个数）"
    for ($i = 0; $i -lt $Expected.Count; $i++) {
        if ($Expected[$i] -cne $Actual[$i]) {
            throw "断言失败：$Message`n  第 $i 个元素：期望 [$($Expected[$i])]，实际 [$($Actual[$i])]"
        }
    }
}

function Write-Utf8([string] $Path, [string] $Text) {
    [System.IO.File]::WriteAllText($Path, $Text, $Utf8NoBom)
}

function Write-Utf8Bom([string] $Path, [string] $Text) {
    [System.IO.File]::WriteAllText($Path, $Text, (New-Object System.Text.UTF8Encoding($true)))
}

function Read-Utf8([string] $Path) {
    return [System.IO.File]::ReadAllText($Path, $Utf8NoBom)
}

# 假 CLI 的正文。行为按「第几次被调用」区分：第 1 次是预热，之后是计时轮。
# 输出形状（9 个空格缩进的清单行）刻意对齐 src/reconcile/changes.rs 的 report_group。
$FakeCli = @'
$ErrorActionPreference = 'Stop'
$Utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$FakeDir = $PSScriptRoot

# stdout 固定按 UTF-8 写：基准按 UTF-8 解，中文路径才不会被机器代码页搅坏。
[Console]::OutputEncoding = $Utf8NoBom

# --version 由 clap 直接处理，不碰 P4，也不占运行序号。
if ($args.Count -eq 1 -and $args[0] -eq '--version') {
    [Console]::Out.WriteLine('p4delta 0.0.0-fake')
    exit 0
}

# 运行序号：第 1 次调用是预热，第 2 次起是计时轮。
$countPath = Join-Path $FakeDir 'run-count.txt'
$run = 1
if (Test-Path -LiteralPath $countPath) { $run = [int] [System.IO.File]::ReadAllText($countPath, $Utf8NoBom) + 1 }
[System.IO.File]::WriteAllText($countPath, [string] $run, $Utf8NoBom)

# 收到的参数向量原样落盘，供断言。
[System.IO.File]::WriteAllLines((Join-Path $FakeDir "args-run$run.txt"), [string[]] $args, $Utf8NoBom)

# 计划文件：key=value，前缀 all. 或 run<N>.，run<N>. 优先。
$plan = @{}
$planPath = Join-Path $FakeDir 'fake.plan.txt'
if (Test-Path -LiteralPath $planPath) {
    foreach ($line in ([System.IO.File]::ReadAllText($planPath, $Utf8NoBom) -split "`r?`n")) {
        $text = $line.Trim()
        if (-not $text -or $text.StartsWith('#')) { continue }
        $split = $text.IndexOf('=')
        if ($split -lt 1) { continue }
        $plan[$text.Substring(0, $split).Trim()] = $text.Substring($split + 1)
    }
}
function Get-Setting([string] $Key, [string] $Default) {
    if ($plan.ContainsKey("run$run.$Key")) { return $plan["run$run.$Key"] }
    if ($plan.ContainsKey("all.$Key")) { return $plan["all.$Key"] }
    return $Default
}

$exitCode = [int] (Get-Setting 'exit' '0')
$sleep = [int] (Get-Setting 'sleep' '20')
$warning = Get-Setting 'stderr' 'Warning: 假 CLI 的告警（退出码 0）'
$addCount = [int] (Get-Setting 'add_count' '1')
$extraAdd = [int] (Get-Setting 'extra_add' '0')
$note = Get-Setting 'note' ''

$path = $args[-1]
if ($warning) { [Console]::Error.WriteLine($warning) }
if ($sleep -gt 0) { Start-Sleep -Milliseconds $sleep }

$lines = New-Object System.Collections.Generic.List[string]
$lines.Add("Processing path `"$path`".")
$lines.Add('   Analyzing files for inconsistencies.')
$lines.Add("      Adding $addCount files in workspace, not in depot or deleted at have revision, but not checked out for add.")
for ($i = 0; $i -lt $addCount; $i++) {
    $lines.Add("         Add `"$path\新 增 文件.txt`".")
}
if ($extraAdd -gt 0) {
    $lines.Add("         Add `"$path\额 外 文件.txt`".")
}
$lines.Add('      Editing 1 files in workspace, changed from have revision, but not checked out for edit.')
$lines.Add("         Edit `"$path\子 目录\edit 中文.txt`".")
$lines.Add("      Counted $($addCount + $extraAdd + 1) changes in 0.0 seconds.")
$lines.Add('Inconsistencies found. Re-run with -a to apply changes.')
# note 是原样追加的一行 stdout（`{path}` 会被替换成本次的目录参数），用来伪造
# 「转交给原生 p4」与「只差大小写的另一个路径」这类没法用别的字段表达的输出。
if ($note) { $lines.Add($note.Replace('{path}', $path)) }
foreach ($line in $lines) { [Console]::Out.WriteLine($line) }
exit $exitCode
'@

function Get-FakeExe {
    return (Join-Path $FakeDir 'fake-cli.cmd')
}

function Reset-Root {
    if (Test-Path -LiteralPath $Root) {
        Remove-Item -LiteralPath $Root -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $FakeDir, $PathDir | Out-Null
    $script:PathResolved = (Resolve-Path -LiteralPath $PathDir).Path

    Write-Utf8Bom (Join-Path $FakeDir 'fake-cli.ps1') $FakeCli
    # .cmd 必须是 CRLF：LF 结尾的批处理在高版本 cmd 上可能整句读错。
    # 内容只有 ASCII，中文一律留在 .ps1 里（那一个带 BOM，谁读都不跑偏）。
    $cmd = "@echo off`r`n" +
        "`"$HostExe`" -NoProfile -ExecutionPolicy Bypass -File `"%~dp0fake-cli.ps1`" %*`r`n" +
        "exit /b %ERRORLEVEL%`r`n"
    Write-Utf8 (Join-Path $FakeDir 'fake-cli.cmd') $cmd

    # 默认计划：退出码 0，但 stderr 有内容——真 p4delta 也这样打告警，
    # 「有 stderr」不该被当成失败。
    Write-Plan @('all.sleep=20')
}

function Write-Plan([string[]] $Lines) {
    Write-Utf8 (Join-Path $FakeDir 'fake.plan.txt') (($Lines -join "`n") + "`n")
}

function Get-FakeArgs([int] $Run) {
    $path = Join-Path $FakeDir "args-run$Run.txt"
    if (-not (Test-Path -LiteralPath $path)) {
        throw "假 CLI 没有记录第 $Run 次调用的参数（$path）"
    }
    return [string[]] [System.IO.File]::ReadAllLines($path, $Utf8NoBom)
}

function Read-Result([string] $OutDir) {
    $path = Join-Path $OutDir 'result.json'
    if (-not (Test-Path -LiteralPath $path)) {
        throw "没有结果 JSON：$path"
    }
    return (Read-Utf8 $path | ConvertFrom-Json)
}

# 跑一次基准脚本。没给输出目录时垫一个临时目录——测试永远不该往仓库的 target 里写东西。
function Invoke-Benchmark([string[]] $Arguments) {
    $effective = $Arguments
    if (($effective -notcontains '-OutDir') -and ($effective -notcontains '-OutDirRoot')) {
        $effective = $effective + @('-OutDirRoot', (Join-Path $Root 'default-out'))
    }
    $processArguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $SourceScript) + $effective
    $output = & $HostExe @processArguments 2>&1 | Out-String
    return [pscustomobject]@{ exit_code = $LASTEXITCODE; output = $output }
}

# 被测脚本与测试自己的 .ps1 都跟 install.ps1 一样带 UTF-8 BOM：仓库惯例，也免得谁在
# Windows PowerShell 5.1 下打开时中文全变乱码。
function Test-ScriptLayoutIsSelfContained {
    foreach ($script in @($SourceScript, $PSCommandPath)) {
        $head = [System.IO.File]::ReadAllBytes($script)[0..2]
        Assert-True (($head[0] -eq 0xEF) -and ($head[1] -eq 0xBB) -and ($head[2] -eq 0xBF)) `
            "$([System.IO.Path]::GetFileName($script)) 必须以 UTF-8 BOM 开头"
    }

    # 基准脚本不许有删除动作：它只该跑 exe、在自己的输出目录里写日志。
    $source = Read-Utf8 $SourceScript
    Assert-True (-not ($source -match 'Remove-Item|Clear-Content')) 'benchmark.ps1 不该有删除动作'
}

# ---- 峰值工作集的采样逻辑（确定性用例，不花内存、不看时序）----
#
# 直接把 benchmark.ps1 里的采样函数**原样取出来**跑（从源码 AST 里摘，不执行脚本主体），
# 喂一个模拟 .NET Process 的对象。模拟的关键是缓存语义：PeakWorkingSet64 返回上一次
# Refresh() 之后缓存的那份快照，不 Refresh 就一直是旧值——真 Process 就是这么缓存的。
# 谁把读取时的 Refresh 删了，这条用例立刻失败。
function Get-PeakWorkingSetSource {
    $tokens = $null
    $errors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($SourceScript, [ref] $tokens, [ref] $errors)
    if ($errors.Count -gt 0) { throw "benchmark.ps1 解析失败：$($errors[0].Message)" }

    $wanted = @('Read-PeakWorkingSetBytes', 'Measure-PeakWorkingSetBytes')
    $texts = [System.Collections.Generic.List[string]]::new()
    foreach ($function in $ast.FindAll({ $args[0] -is [System.Management.Automation.Language.FunctionDefinitionAst] }, $true)) {
        if ($wanted -contains $function.Name) { $texts.Add($function.Extent.Text) }
    }
    if ($texts.Count -ne $wanted.Count) {
        throw "benchmark.ps1 里找不到 $($wanted -join ' / ')：采样逻辑改名或挪走了，这个用例要跟着改"
    }
    return ($texts -join "`n")
}

# 模拟 Process。$Steps 是 Refresh() 之后「当前值」走的台阶，$RunningReads 是「还没退出」的
# 次数（之后的 WaitForExit 返回 true），$ValueMode 决定读到什么：
#   cached = 真缓存的语义（不 Refresh 就一直返回第一级台阶）；zero = 退出后读回 0；
#   throw  = 进程信息读不到（抛异常）。
function New-StubProcess([int64[]] $Steps, [int] $RunningReads, [string] $ValueMode) {
    $state = [pscustomobject]@{
        Steps        = $Steps
        Index        = 0
        Refreshes    = 0
        RunningReads = $RunningReads
        ValueMode    = $ValueMode
    }
    $stub = [pscustomobject]@{ State = $state }

    $stub | Add-Member -MemberType ScriptMethod -Name Refresh -Value {
        # 作废缓存快照、重新取：这里表现为「当前值」走到下一级台阶
        $this.State.Refreshes++
        if ($this.State.Index -lt $this.State.Steps.Count - 1) { $this.State.Index++ }
    }
    $stub | Add-Member -MemberType ScriptProperty -Name PeakWorkingSet64 -Value {
        if ($this.State.ValueMode -eq 'throw') { throw '进程已退出，读不到进程信息' }
        if ($this.State.ValueMode -eq 'zero') { return [int64] 0 }
        if ($this.State.Refreshes -eq 0) { return $this.State.Steps[0] }
        return $this.State.Steps[$this.State.Index]
    }
    $stub | Add-Member -MemberType ScriptMethod -Name WaitForExit -Value {
        param($Milliseconds)
        if ($this.State.RunningReads -gt 0) {
            $this.State.RunningReads--
            return $false
        }
        return $true
    }
    return $stub
}

function Test-PeakWorkingSetSampling {
    # 点号执行：把真函数放进本用例的作用域，下面才调得到。
    . ([scriptblock]::Create((Get-PeakWorkingSetSource)))

    # 1) 采样循环每轮都要先 Refresh。删掉它，读到的永远是缓存住的第一级台阶（10），
    #    峰值就停在最小值上——这正是要拦的变异。
    $growing = New-StubProcess ([int64[]] @(10, 20, 30, 40)) 3 'cached'
    Assert-Equal 40 (Measure-PeakWorkingSetBytes $growing 50) `
        '采样循环必须在每次读取前 Refresh，否则峰值冻结在最早那份快照上'

    # 2) 退出后 Refresh 读回 0：那一次读数无效，必须记 null（否则会把 0 当成峰值）。
    $zeroAfterExit = New-StubProcess ([int64[]] @(10, 20)) 0 'zero'
    Assert-True ($null -eq (Read-PeakWorkingSetBytes $zeroAfterExit)) '退出后读到 0 时必须记 null'

    # 3) 进程信息读不到（抛异常）同样记 null。
    $throwing = New-StubProcess ([int64[]] @(10, 20)) 0 'throw'
    Assert-True ($null -eq (Read-PeakWorkingSetBytes $throwing)) '读进程信息抛异常时必须记 null'

    # 4) 运行期一次都没读到 → 采样结果是 null：没有数字可报就不许造数字。
    $neverReadable = New-StubProcess ([int64[]] @(10, 20)) 2 'throw'
    Assert-True ($null -eq (Measure-PeakWorkingSetBytes $neverReadable 50)) '一次都读不到时该是 null，不估算'
}

function Test-WarmupAndMeasuredRounds {
    Reset-Root
    $outDir = Join-Path $Root 'out'
    $result = Invoke-Benchmark @(
        '-Binary', (Get-FakeExe), '-Workspace', $Workspace, '-Path', $PathDir, '-Rounds', '3', '-OutDir', $outDir
    )
    Assert-Equal 0 $result.exit_code "基准应当成功：$($result.output)"

    # 预热 1 轮 + 计时 3 轮，每轮 stdout / stderr 各留一份原始日志。
    foreach ($tag in @('round-00-warmup', 'round-01', 'round-02', 'round-03')) {
        foreach ($stream in @('stdout', 'stderr')) {
            Assert-True (Test-Path -LiteralPath (Join-Path $outDir "raw/$tag.$stream.txt")) `
                "缺原始日志 raw/$tag.$stream.txt"
        }
    }
    # --version 探测不该占掉一个运行序号（假 CLI 对 --version 不计数）。
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $FakeDir 'args-run5.txt'))) `
        '预热 + 3 轮只该调用 4 次，第 5 次不存在'

    $doc = Read-Result $outDir
    Assert-Equal 'ok' $doc.result 'result 应当是 ok'
    Assert-True ($null -eq $doc.failure) '成功的运行不该有 failure'
    Assert-Equal 3 (@($doc.rounds).Count) '计时轮数应当是 3（预热不算）'
    Assert-Equal 3 $doc.stats.measured '统计里只该有 3 轮'
    Assert-Equal 'p4delta 0.0.0-fake' $doc.parameters.binary_version '--version 探到的版本该记进 JSON'
    Assert-Equal $Workspace $doc.parameters.workspace '工作区名该原样记下（含空格与中文）'
    Assert-Equal 'open' $doc.parameters.mode '默认模式是 open'
    Assert-True ($null -ne $doc.warmup) '预热那一轮也该记进 JSON'
    Assert-Equal 'round-00-warmup' $doc.consistency.reference '一致性基准是预热那一轮'

    foreach ($round in @($doc.rounds)) {
        Assert-Equal 0 $round.exit_code '每轮都该是退出码 0'
        Assert-True ($round.seconds -gt 0) '每轮都该有正的耗时'
    }

    Assert-True ($doc.stats.median_seconds -ge $doc.stats.min_seconds) '中位数不该小于最小值'
    Assert-True ($doc.stats.median_seconds -le $doc.stats.max_seconds) '中位数不该大于最大值'
    Assert-Equal ([math]::Round($doc.stats.max_seconds - $doc.stats.min_seconds, 3)) $doc.stats.range_seconds `
        'range 应当是 max - min'

    Assert-True ($doc.consistency.consistent) '动作清单应当一致'
    Assert-Equal 2 $doc.consistency.listing_entries '假 CLI 每轮报 2 条动作'

    # 峰值工作集：读得到就是个正数，读不到是 null——不许拿 0 或估算值充数。
    foreach ($round in @($doc.rounds)) {
        Assert-True (($null -eq $round.peak_working_set_bytes) -or ($round.peak_working_set_bytes -gt 0)) `
            '峰值工作集要么是正整数、要么是 null'
        Assert-True (($null -eq $round.peak_working_set_source) -or $round.peak_working_set_source -in @('post-exit', 'sampled-lower-bound')) `
            '峰值工作集的来源要么是 post-exit / sampled-lower-bound，要么是 null'
    }
    Assert-True (@($doc.rounds | Where-Object { $null -ne $_.peak_working_set_bytes }).Count -gt 0) `
        '至少该有一轮读到了峰值工作集（运行期采样就够）'

    # stderr 有内容 + 退出码 0 = 正常告警，不是失败。
    $stderr = Read-Utf8 (Join-Path $outDir 'raw/round-01.stderr.txt')
    Assert-True ($stderr.Contains('假 CLI 的告警')) '假 CLI 的告警该原样留在 stderr 日志里'

    # 中文 + 空格的路径必须原样穿过基准脚本与两条管道。
    $stdout = Read-Utf8 (Join-Path $outDir 'raw/round-01.stdout.txt')
    Assert-True ($stdout.Contains("Add `"$($doc.parameters.path)\新 增 文件.txt`".")) `
        "原始 stdout 里该有带空格与中文的完整路径：$($doc.parameters.path)"
    Assert-Equal $PathResolved $doc.parameters.path '传给 CLI 的目录该是解析后的绝对路径'
}

# 7 轮是奇数轮：中位数取排序后第 floor(7/2)=3 个。PowerShell 的 [int] 转换是
# 四舍六入五成双，[int](7/2) 会变成 4——取到的是下一个数，这条用例专门钉住这一点。
# 每轮的 sleep 拉开 100ms，排序后的第 4、5 个必然不同，取错索引马上露馅。
function Test-MedianOfSevenRounds {
    Reset-Root
    $plan = @('all.sleep=20')
    for ($run = 2; $run -le 8; $run++) {
        $plan += "run$run.sleep=$((($run - 1) * 100))"
    }
    Write-Plan $plan

    $outDir = Join-Path $Root 'out'
    $result = Invoke-Benchmark @(
        '-Binary', (Get-FakeExe), '-Workspace', $Workspace, '-Path', $PathDir, '-Rounds', '7', '-OutDir', $outDir
    )
    Assert-Equal 0 $result.exit_code "基准应当成功：$($result.output)"

    $doc = Read-Result $outDir
    Assert-Equal 7 (@($doc.rounds).Count) '该跑满 7 轮'
    Assert-Equal 7 $doc.stats.measured '统计里该有 7 轮'

    $sorted = @(@($doc.rounds | ForEach-Object { $_.seconds }) | Sort-Object)
    Assert-Equal 7 $sorted.Count '排序后的轮数'
    Assert-Equal ([math]::Round($sorted[3], 3)) $doc.stats.median_seconds `
        '7 轮的中位数该是排序后第 4 个（floor(7/2) = 3）'
    Assert-True ($sorted[3] -ne $sorted[4]) '第 4、5 个不该相等，否则这条用例验不出取错索引'
}

function Test-PassesOnlyPreviewArguments {
    # 每个模式单独跑一次：假 CLI 记下它真正收到的参数向量，脚本 JSON 记下它自己发出去的，
    # 两边都要对得上，且都不许出现 -a。
    $cases = @(
        @{ name = 'open';  extra = @();                                              expected = @('-l', '-w', $Workspace, $PathResolved) },
        @{ name = 'clean'; extra = @('-Mode', 'clean');                              expected = @('-l', '-w', $Workspace, '--clean', $PathResolved) },
        @{ name = 'sync';  extra = @('-Mode', 'sync');                               expected = @('-l', '-w', $Workspace, '--sync', $PathResolved) },
        @{ name = 'sync-force'; extra = @('-Mode', 'sync', '-Force', '-To', '12345', '-VerifyAll'); expected = @('-l', '-w', $Workspace, '--sync', '--force', '--to', '12345', '--verify-all', $PathResolved) },
        @{ name = 'prune'; extra = @('-NoPruneIgnoredDirs');                         expected = @('-l', '-w', $Workspace, '--no-prune-ignored-dirs', $PathResolved) }
    )

    foreach ($case in $cases) {
        Reset-Root
        $outDir = Join-Path $Root 'out'
        $result = Invoke-Benchmark (@(
                '-Binary', (Get-FakeExe), '-Workspace', $Workspace, '-Path', $PathDir, '-Rounds', '1', '-OutDir', $outDir
            ) + $case.extra)
        Assert-Equal 0 $result.exit_code "$($case.name)：基准应当成功：$($result.output)"

        # 第 1 次调用是预热，第 2 次是唯一的计时轮：两次的参数必须一模一样。
        foreach ($run in @(1, 2)) {
            $received = @(Get-FakeArgs $run)
            Assert-SequenceEqual $case.expected $received "$($case.name)：假 CLI 第 $run 次收到的参数"
            Assert-True ($received -notcontains '-a') "$($case.name)：绝不能带 -a（预览基准不改任何东西）"
            Assert-True ($received -notcontains '--apply') "$($case.name)：绝不能带 --apply"
            Assert-True ($received -contains '-l') "$($case.name)：每轮都该带 -l（要靠清单做一致性比对）"
        }

        $doc = Read-Result $outDir
        Assert-SequenceEqual $case.expected ([string[]] @($doc.parameters.arguments)) "$($case.name)：JSON 里记的参数"
    }
}

function Test-FailsOnNonZeroExitAndKeepsLogs {
    Reset-Root
    Write-Plan @('all.sleep=20', 'run2.exit=5', 'run2.stderr=假 CLI 故意失败：第 1 个计时轮')
    $outDir = Join-Path $Root 'out'
    $result = Invoke-Benchmark @(
        '-Binary', (Get-FakeExe), '-Workspace', $Workspace, '-Path', $PathDir, '-Rounds', '3', '-OutDir', $outDir
    )

    Assert-True ($result.exit_code -ne 0) '某一轮退出码非 0 时基准必须失败'
    $doc = Read-Result $outDir
    Assert-Equal 'failed' $doc.result 'result 应当是 failed'
    Assert-Equal 1 (@($doc.rounds).Count) '第一轮就失败，不该再跑第 2 轮'
    Assert-Equal 5 $doc.rounds[0].exit_code '失败那一轮的退出码该记下来'
    Assert-True ($null -eq $doc.stats) '失败时不该给出统计数字'
    Assert-True ($doc.failure -match '第 1 轮') '失败原因该点名是哪一轮'
    Assert-True ($doc.failure -match '退出码 5') '失败原因该带上退出码'

    # 原始日志必须留在原地：这是失败后唯一能查的东西。
    Assert-True (Test-Path -LiteralPath (Join-Path $outDir 'raw/round-00-warmup.stdout.txt')) '预热日志该留着'
    Assert-True (Test-Path -LiteralPath (Join-Path $outDir 'raw/round-01.stdout.txt')) '失败那一轮的 stdout 该留着'
    $stderr = Read-Utf8 (Join-Path $outDir 'raw/round-01.stderr.txt')
    Assert-True ($stderr.Contains('假 CLI 故意失败：第 1 个计时轮')) '失败那一轮的 stderr 该原样保留'
}

function Test-FailsOnActionListingChange {
    # 重数变化：同一个文件报两遍。集合相等、多重集不等——只比集合的实现在这里会放过。
    Reset-Root
    Write-Plan @('all.sleep=20', 'run3.add_count=2')
    $outDir = Join-Path $Root 'out'
    $result = Invoke-Benchmark @(
        '-Binary', (Get-FakeExe), '-Workspace', $Workspace, '-Path', $PathDir, '-Rounds', '5', '-OutDir', $outDir
    )

    Assert-True ($result.exit_code -ne 0) '动作清单重数变了，基准必须失败'
    $doc = Read-Result $outDir
    Assert-Equal 'failed' $doc.result 'result 应当是 failed'
    Assert-Equal 2 (@($doc.rounds).Count) '第 3 次调用（= 第 2 个计时轮）就不一致了，之后的轮次不该再跑'
    Assert-True (-not $doc.consistency.consistent) 'consistency 该记成不一致'
    Assert-True ($doc.failure -match '第 2 轮') '失败原因该点名是哪一轮'
    Assert-True (@($doc.consistency.differences).Count -ge 1) '差异清单不该是空的'

    $difference = @($doc.consistency.differences)[0]
    Assert-True ($difference.Contains('Add')) "差异要点名动作：$difference"
    Assert-True ($difference.Contains($doc.parameters.path)) "差异要给完整路径：$difference"
    Assert-True ($difference.Contains('1') -and $difference.Contains('2')) "差异要给出两边的重数：$difference"

    # 集合变化：多出一个文件。与上面那条是两种不同的差异。
    Reset-Root
    Write-Plan @('all.sleep=20', 'run3.extra_add=1')
    $outDir = Join-Path $Root 'out'
    $result = Invoke-Benchmark @(
        '-Binary', (Get-FakeExe), '-Workspace', $Workspace, '-Path', $PathDir, '-Rounds', '3', '-OutDir', $outDir
    )

    Assert-True ($result.exit_code -ne 0) '动作清单多出条目时，基准必须失败'
    $doc = Read-Result $outDir
    Assert-True (-not $doc.consistency.consistent) 'consistency 该记成不一致'
    $difference = @($doc.consistency.differences)[0]
    Assert-True ($difference.Contains('额 外 文件.txt')) "差异要点名多出来的那个文件：$difference"

    # 只差大小写的路径算两个条目（序号比较，不折叠大小写）：多出来的那个必须原样出现在差异里。
    Reset-Root
    Write-Plan @('all.sleep=20', 'run3.note=         Add "{path}\新 增 文件.TXT".')
    $outDir = Join-Path $Root 'out'
    $result = Invoke-Benchmark @(
        '-Binary', (Get-FakeExe), '-Workspace', $Workspace, '-Path', $PathDir, '-Rounds', '3', '-OutDir', $outDir
    )

    Assert-True ($result.exit_code -ne 0) '路径只差大小写也是一处差异，基准必须失败'
    $doc = Read-Result $outDir
    Assert-Equal 1 (@($doc.consistency.differences).Count) '该是两处独立的条目，不是被折叠成一条'
    Assert-True (@($doc.consistency.differences)[0].Contains('新 增 文件.TXT')) `
        "差异里该原样出现 .TXT 那个路径：$(@($doc.consistency.differences)[0])"
}

# 预热那一轮也在比对范围内：清单要是随摘要缓存冷热变化，那是结果依赖缓存，不能放过。
function Test-FailsWhenWarmupListingDiffers {
    Reset-Root
    Write-Plan @('all.sleep=20', 'run1.add_count=2')
    $outDir = Join-Path $Root 'out'
    $result = Invoke-Benchmark @(
        '-Binary', (Get-FakeExe), '-Workspace', $Workspace, '-Path', $PathDir, '-Rounds', '3', '-OutDir', $outDir
    )

    Assert-True ($result.exit_code -ne 0) '预热轮与计时轮清单不同时，基准必须失败'
    $doc = Read-Result $outDir
    Assert-Equal 'failed' $doc.result 'result 应当是 failed'
    Assert-Equal 1 (@($doc.rounds).Count) '第 1 个计时轮就发现不一致，后面的不该再跑'
    Assert-True (-not $doc.consistency.consistent) 'consistency 该记成不一致'
    Assert-True ($doc.failure -match '预热轮') '失败原因该说明是与预热轮不一致'
}

# 有文件被转交给原生 p4 时，那批文件不出现在 -l 清单里：清单不完整，
# 不能拿它给出一致性结论。
function Test-FailsWhenFilesAreHandedOffToP4 {
    Reset-Root
    Write-Plan @('all.sleep=20', 'run2.note=Found 3 file(s) that are not supported by p4delta, running a manual reconcile')
    $outDir = Join-Path $Root 'out'
    $result = Invoke-Benchmark @(
        '-Binary', (Get-FakeExe), '-Workspace', $Workspace, '-Path', $PathDir, '-Rounds', '3', '-OutDir', $outDir
    )

    Assert-True ($result.exit_code -ne 0) '有转交时基准必须拒绝给结论'
    $doc = Read-Result $outDir
    Assert-Equal 'failed' $doc.result 'result 应当是 failed'
    Assert-True (@($doc.rounds)[0].handoff_to_p4) '转交这件事该记在那一轮的 JSON 里'
    Assert-True ($doc.failure -match 'p4delta') '失败原因该说明是转交导致清单不完整'
    $stdout = Read-Utf8 (Join-Path $outDir 'raw/round-01.stdout.txt')
    Assert-True ($stdout.Contains('not supported by p4delta')) '原始 stdout 该留着，便于核对'
}

function Test-RefusesUnsafeOutputDirectory {
    # 已存在的目录：一个字节都不许动，CLI 也不许启动。
    Reset-Root
    $outDir = Join-Path $Root 'out'
    New-Item -ItemType Directory -Force -Path $outDir | Out-Null
    $sentinel = Join-Path $outDir 'sentinel.txt'
    Write-Utf8 $sentinel '别动我'

    $result = Invoke-Benchmark @(
        '-Binary', (Get-FakeExe), '-Workspace', $Workspace, '-Path', $PathDir, '-Rounds', '1', '-OutDir', $outDir
    )

    Assert-True ($result.exit_code -ne 0) '输出目录已存在时必须拒绝'
    Assert-True ($result.output.Contains($outDir)) '报错该点名那个已存在的目录'
    Assert-Equal '别动我' (Read-Utf8 $sentinel) '已有的文件一个字节都不该动'
    Assert-Equal 1 (@(Get-ChildItem -LiteralPath $outDir).Count) '被拒绝时不该往那个目录里写任何东西'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $FakeDir 'run-count.txt'))) '拒绝得比启动 CLI 更早'

    # 输出目录落在被扫描的目录里：基准自己的日志会变成下一轮的新增文件，必须拒绝。
    $result = Invoke-Benchmark @(
        '-Binary', (Get-FakeExe), '-Workspace', $Workspace, '-Path', $Root, '-Rounds', '1'
    )
    Assert-True ($result.exit_code -ne 0) '输出目录在被扫描的树里时必须拒绝'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $Root 'default-out'))) '被拒绝时连目录都不该建'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $FakeDir 'run-count.txt'))) '这一条也该在启动 CLI 之前拦下'

    # 默认输出目录：同一秒里连着跑两次也要各建一个新目录，不能互相覆盖。
    $rootDir = Join-Path $Root 'outroot'
    $arguments = @('-Binary', (Get-FakeExe), '-Workspace', $Workspace, '-Path', $PathDir, '-Rounds', '1', '-OutDirRoot', $rootDir)
    $first = Invoke-Benchmark $arguments
    Reset-FakeCounter
    $second = Invoke-Benchmark $arguments
    Assert-Equal 0 $first.exit_code "第一次应该成功：$($first.output)"
    Assert-Equal 0 $second.exit_code "第二次应该成功：$($second.output)"

    $dirs = @(Get-ChildItem -LiteralPath $rootDir -Directory)
    Assert-Equal 2 $dirs.Count '两次运行该各建一个目录'
    foreach ($dir in $dirs) {
        Assert-True (Test-Path -LiteralPath (Join-Path $dir.FullName 'result.json')) "$($dir.Name) 里该有结果"
    }
}

# 只清假 CLI 的运行计数（连跑两次时计划里的 runN. 才有意义），不碰别的东西。
function Reset-FakeCounter {
    $countPath = Join-Path $FakeDir 'run-count.txt'
    if (Test-Path -LiteralPath $countPath) { Remove-Item -LiteralPath $countPath -Force }
}

function Test-DoesNotTouchDigestCache {
    Reset-Root
    # p4delta 的摘要缓存落在 %LOCALAPPDATA%\p4delta\cache\digests_<workspace>.bin。把它
    # 重定向到临时目录，放一个哨兵在那，跑完基准后必须原样还在——基准只该新建自己的输出
    # 目录，不该动用户的缓存。
    $localAppData = Join-Path $Root 'fake-localappdata'
    $cacheDir = Join-Path $localAppData 'p4delta\cache'
    New-Item -ItemType Directory -Force -Path $cacheDir | Out-Null
    $sentinel = Join-Path $cacheDir "digests_$Workspace.bin"
    Write-Utf8 $sentinel '这不是真的缓存，只用来验证基准不删它'

    $previous = $env:LOCALAPPDATA
    try {
        $env:LOCALAPPDATA = $localAppData
        $result = Invoke-Benchmark @(
            '-Binary', (Get-FakeExe), '-Workspace', $Workspace, '-Path', $PathDir, '-Rounds', '2', '-OutDir', (Join-Path $Root 'out')
        )
        Assert-Equal 0 $result.exit_code "基准应当成功：$($result.output)"
    } finally {
        $env:LOCALAPPDATA = $previous
    }

    Assert-True (Test-Path -LiteralPath $sentinel) '基准不该删掉摘要缓存'
    Assert-Equal '这不是真的缓存，只用来验证基准不删它' (Read-Utf8 $sentinel) '摘要缓存的内容不该被改'
}

function Test-RejectsBadArguments {
    Reset-Root
    $fake = Get-FakeExe
    $cases = @(
        @{ label = '-To 只在 sync 下有意义'; args = @('-Mode', 'clean', '-To', '5') },
        @{ label = '-To 不能是负数'; args = @('-Mode', 'sync', '-To', '-1') },
        @{ label = '-To 不能超出 u32'; args = @('-Mode', 'sync', '-To', '4294967296') },
        @{ label = '-Force 只在 sync 下有意义'; args = @('-Mode', 'clean', '-Force') },
        @{ label = '-VerifyAll 需要 -Force'; args = @('-Mode', 'sync', '-VerifyAll') },
        @{ label = '-VerifyAll 只在 sync 下有意义'; args = @('-VerifyAll') },
        @{ label = '-Rounds 小于 1'; args = @('-Rounds', '0') },
        @{ label = '-Binary 不存在'; args = @(); binary = (Join-Path $Root '没有这个.exe') },
        @{ label = '-Path 不是目录'; args = @(); path = (Join-Path $Root '没有这个目录') },
        @{ label = '缺 -Workspace'; args = @(); workspace = '' }
    )

    foreach ($case in $cases) {
        $binary = if ($case.ContainsKey('binary')) { $case.binary } else { $fake }
        $path = if ($case.ContainsKey('path')) { $case.path } else { $PathDir }
        $workspace = if ($case.ContainsKey('workspace')) { $case.workspace } else { $Workspace }
        $arguments = @('-Binary', $binary, '-Workspace', $workspace, '-Path', $path) + $case.args

        $result = Invoke-Benchmark $arguments
        Assert-True ($result.exit_code -ne 0) "$($case.label)：该被拒绝"
    }

    Assert-True (-not (Test-Path -LiteralPath (Join-Path $FakeDir 'run-count.txt'))) `
        '参数有问题时一个进程都不该起（校验要在启动 CLI 之前）'
}

$cases = @(
    'Test-ScriptLayoutIsSelfContained',
    'Test-PeakWorkingSetSampling',
    'Test-WarmupAndMeasuredRounds',
    'Test-MedianOfSevenRounds',
    'Test-PassesOnlyPreviewArguments',
    'Test-FailsOnNonZeroExitAndKeepsLogs',
    'Test-FailsOnActionListingChange',
    'Test-FailsWhenWarmupListingDiffers',
    'Test-FailsWhenFilesAreHandedOffToP4',
    'Test-RefusesUnsafeOutputDirectory',
    'Test-DoesNotTouchDigestCache',
    'Test-RejectsBadArguments'
)

$failed = 0
Write-Host "benchmark.ps1 测试（$([System.IO.Path]::GetFileName($HostExe))）"

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
