#Requires -Version 7.0
<#
.SYNOPSIS
    发布一个版本：版本号写进 Cargo.toml / p4delta.exe.manifest / Cargo.lock，
    本地跑一遍门禁，然后提交、打附注 tag、推送。

.DESCRIPTION
    推送之后由 .github/workflows/release.yml 接手：复用 CI 当门禁、构建、断言产物、
    打包 zip 与 SHA256SUMS、建 release。这个脚本只负责到「把 tag 推上去」为止。

    版本号有三个去处，脚本一次改完：Cargo.toml 的 version、p4delta.exe.manifest 里
    assemblyIdentity 的 version（四段的 X.Y.Z.0）、Cargo.lock 里 p4delta 的条目。
    （exe 的 VERSIONINFO 不用管，build.rs 从 Cargo.toml 现算。）

    改完先跑一遍本地门禁（fmt / clippy / test），过了才提交、打 tag、推送 ——
    tag 一旦推出去 release.yml 就会跑，门禁在本地红比在 CI 红便宜得多。

    要求 PowerShell 7 而不是 Windows PowerShell 5.1：5.1 与原生命令之间传非 ASCII 文本要过
    一遍系统代码页（参数与输出都是），「chore: 发布 vX.Y.Z」这类提交信息、以及 git 自己的
    中文输出都可能乱码；7 全程走 UTF-8。这是开发者脚本，不随发布包走，不必为 5.1 让步。

.PARAMETER Version
    要发的版本号，写成 X.Y.Z 三段数字，不带 v 前缀（v 由脚本加）。
    不写就自动升：默认补丁号 +1（0.1.3 → 0.1.4）。

.PARAMETER Patch
    补丁号 +1（默认）。

.PARAMETER Minor
    次版本号 +1，补丁号归零（0.1.3 → 0.2.0）。

.PARAMETER Major
    主版本号 +1，后面归零（0.1.3 → 1.0.0）。

.PARAMETER DryRun
    只做检查并打印将要做什么，不改文件、不提交、不打 tag、不推送。

.PARAMETER SkipCheck
    跳过本地门禁（CI 里还会再跑一遍，但那要等 tag 推出去之后）。

.PARAMETER NoPush
    本地做完：改文件、跑门禁、提交、打 tag，但不推送。

.PARAMETER Yes
    推送前不再确认。

.PARAMETER Branch
    要求当前分支是 <名>（默认 main）。

.PARAMETER Remote
    远端名（默认 origin）。

.PARAMETER Help
    显示用法。

.EXAMPLE
    pwsh -File scripts/release.ps1 -DryRun    # 先看一眼（版本号自动升）
    pwsh -File scripts/release.ps1            # 发 0.1.3 → 0.1.4
    pwsh -File scripts/release.ps1 -Minor     # 发 0.1.3 → 0.2.0
    pwsh -File scripts/release.ps1 0.1.4      # 指定版本号，不自动升
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [string] $Version,
    [switch] $Patch,
    [switch] $Minor,
    [switch] $Major,
    [switch] $DryRun,
    [switch] $SkipCheck,
    [switch] $NoPush,
    [switch] $Yes,
    [string] $Branch = 'main',
    [string] $Remote = 'origin',
    # 别名给了 -h：bash 版的 -h/--help 是肌肉记忆；PowerShell 自己还认 -?。
    [Alias('h')]
    [switch] $Help
)

$ErrorActionPreference = 'Stop'

# 仓库里的文本文件一律无 BOM + LF（.gitattributes: text=auto eol=lf）。改写版本号时
# 必须原样写回：少了这个显式编码，5.1 会加 BOM、7 也未必保证，diff 里会多出整文件变化。
$Utf8NoBom = New-Object System.Text.UTF8Encoding($false)

$RepoRoot = Split-Path -Parent $PSScriptRoot
$TomlPath = Join-Path $RepoRoot 'Cargo.toml'
$LockPath = Join-Path $RepoRoot 'Cargo.lock'
$ManifestPath = Join-Path $RepoRoot 'p4delta.exe.manifest'

function Show-Usage {
    Write-Host @'
用法：pwsh -File scripts/release.ps1 [<X.Y.Z>] [选项]

不写版本号就自动升：默认补丁号 +1（0.1.3 → 0.1.4）。
  -Patch          补丁号 +1（默认）
  -Minor          次版本号 +1，补丁号归零（0.1.3 → 0.2.0）
  -Major          主版本号 +1，后面归零（0.1.3 → 1.0.0）

版本号有三个去处，脚本一次改完：Cargo.toml 的 version、p4delta.exe.manifest 里
assemblyIdentity 的 version（四段的 X.Y.Z.0）、Cargo.lock 里 p4delta 的条目。
（exe 的 VERSIONINFO 不用管，build.rs 从 Cargo.toml 现算。）

改完先跑一遍本地门禁（fmt / clippy / test），过了才提交、打 tag、推送 ——
tag 一旦推出去 release.yml 就会跑，门禁在本地红比在 CI 红便宜得多。

其他选项：
  -DryRun         只做检查并打印将要做什么，不改文件、不提交、不打 tag、不推送
  -SkipCheck      跳过本地门禁（CI 里还会再跑一遍，但那要等 tag 推出去之后）
  -NoPush         本地做完：改文件、跑门禁、提交、打 tag，但不推送
  -Yes            推送前不再确认
  -Branch <名>    要求当前分支是 <名>（默认 main）
  -Remote <名>    远端名（默认 origin）
  -Help           显示这段

例子：
  pwsh -File scripts/release.ps1 -DryRun     # 先看一眼（版本号自动升）
  pwsh -File scripts/release.ps1             # 发 0.1.3 → 0.1.4
  pwsh -File scripts/release.ps1 -Minor      # 发 0.1.3 → 0.2.0
  pwsh -File scripts/release.ps1 0.1.4       # 指定版本号，不自动升
'@
}

# 就地改写文本文件。不用「读全文、写全文」之外的招：正则替换走 [regex]，写回时显式指定
# 无 BOM 的 UTF-8，行尾与末尾换行都随原文——Cargo.toml / Cargo.lock / manifest 三个文件
# 都得保持逐字节可比，否则提交里会混进噪声。
function Edit-TextFile([string] $Path, [string] $Pattern, [string] $Replacement) {
    $text = [System.IO.File]::ReadAllText($Path)
    [System.IO.File]::WriteAllText($Path, [regex]::Replace($text, $Pattern, $Replacement), $Utf8NoBom)
}

# 三段数字逐段比大小；$Left 比 $Right 大时为真。
# 局部变量别叫 $a / $b：PowerShell 的变量名大小写不敏感，那样会和参数 $A / $B 撞名，
# 而参数上的 [string] 约束会把赋进去的数组再压回字符串（"0 1 6"），逐段比较就只剩第 0 段。
function Test-VersionGreater([string] $Left, [string] $Right) {
    $leftParts = $Left -split '\.'
    $rightParts = $Right -split '\.'
    for ($i = 0; $i -lt 3; $i++) {
        $x = if ($i -lt $leftParts.Count) { [int] $leftParts[$i] } else { 0 }
        $y = if ($i -lt $rightParts.Count) { [int] $rightParts[$i] } else { 0 }
        if ($x -gt $y) { return $true }
        if ($x -lt $y) { return $false }
    }
    return $false
}

# 升一段版本号。三段都按十进制读——前导零在 bash 里要写 10# 才不按八进制解释，
# PowerShell 的 [int] 本来就按十进制，别把那个补丁照抄过来。
function Get-NextVersion([string] $Current, [string] $Part) {
    $p = $Current -split '\.'
    switch ($Part) {
        'major' { return "$([int] $p[0] + 1).0.0" }
        'minor' { return "$([int] $p[0]).$([int] $p[1] + 1).0" }
        'patch' { return "$([int] $p[0]).$([int] $p[1]).$([int] $p[2] + 1)" }
        default { throw "不认识的升级幅度：$Part" }
    }
}

# 把推送命令拼出来，成功与失败的提示里都要用。&& 在 PowerShell 7 里也是链式操作符，
# 这行提示可以原样粘回 pwsh。
function Get-PushHint([string] $Tag) {
    return "git push $Remote $Branch && git push $Remote $Tag"
}

# 在仓库根跑一条 cargo 命令，非 0 就当成失败。cargo 必须在仓库根跑（workspace 成员、
# .config/nextest.toml 都按当前目录找）；git 则一律走 -C，不依赖当前目录。
# 参数收成一个数组：`--` 在 PowerShell 里是「参数到此为止」的记号，直接写在调用处会被
# 吃掉，而 `cargo fmt --all -- --check` 少了这个分隔符就不是同一条命令了。
function Invoke-Cargo([string[]] $CargoArgs) {
    Push-Location $RepoRoot
    try {
        & cargo @CargoArgs
        $code = $LASTEXITCODE
    } finally {
        Pop-Location
    }
    if ($code -ne 0) {
        throw "cargo $($CargoArgs -join ' ') 失败（退出码 $code）"
    }
}

if ($Help) {
    Show-Usage
    exit 0
}

try {
    # ---- 参数 ----

    # 升级开关：默认 patch，一次只准给一个。判断「显式给了」要看 $PSBoundParameters，
    # 不是看开关的值——-Patch:$false 也说明用户写了它。
    $bumpNames = @(
        foreach ($name in 'Patch', 'Minor', 'Major') {
            if ($PSBoundParameters.ContainsKey($name)) { $name }
        }
    )
    if ($bumpNames.Count -gt 1) {
        throw "升级开关一次只能给一个：$(($bumpNames | ForEach-Object { "-$_" }) -join ' ')"
    }
    if ($Version -and $bumpNames.Count -gt 0) {
        throw "已经给了版本号 $Version，就不要再给 -Patch / -Minor / -Major 了"
    }
    $bump = if ($bumpNames.Count -eq 1) { $bumpNames[0].ToLowerInvariant() } else { 'patch' }

    # 显式给的版本号先验格式；没给的话，等读出当前版本再算。
    if ($Version -and $Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') {
        throw "版本号要写成 X.Y.Z 三段数字，不带 v 前缀（v 由脚本加）：$Version"
    }

    # ---- 前置检查：宁可在这里红，也别把 tag 推出去再发现 ----

    # 脚本住在 scripts/ 下，仓库根就是它的上一级；下面一律 git -C，不依赖当前目录。
    # 原生命令先赋值再读 $LASTEXITCODE：塞进管道里就未必读得到。
    $null = & git -C $RepoRoot rev-parse --show-toplevel 2>$null
    if ($LASTEXITCODE -ne 0) {
        throw "不在 git 仓库里：$RepoRoot"
    }

    # 变量名带上 current：PowerShell 的变量名大小写不敏感，叫 $branch 会和参数 $Branch 撞名，
    # 赋值会把「要求的分支」一起改掉，这条检查就永远不生效了。
    $currentBranch = & git -C $RepoRoot rev-parse --abbrev-ref HEAD
    if ($LASTEXITCODE -ne 0) {
        throw '读不出当前分支'
    }
    if ($currentBranch -ne $Branch) {
        throw "当前在 $currentBranch 分支上；发布要在 $Branch 上做（换分支用 -Branch）"
    }

    $dirty = & git -C $RepoRoot status --porcelain
    if ($LASTEXITCODE -ne 0) {
        throw 'git status 失败'
    }
    if ($dirty) {
        $details = $dirty -join "`n"
        throw "工作区不干净：发布的提交里只该有版本号这一处改动，先提交或收起来`n$details"
    }

    # 只认行首那一处 version = ，多了就说明 Cargo.toml 的结构变了，脚本不该瞎猜。
    $tomlText = [System.IO.File]::ReadAllText($TomlPath)
    $found = ([regex]::Matches($tomlText, '(?m)^version = ')).Count
    if ($found -ne 1) {
        throw "Cargo.toml 里行首的 version = 有 $found 处，脚本只认识一处，手工改吧"
    }
    $currentMatch = [regex]::Match($tomlText, '(?m)^version = "([^"]*)"$')
    if (-not $currentMatch.Success) {
        throw '读不出 Cargo.toml 的 version'
    }
    $current = $currentMatch.Groups[1].Value

    # release.yml 的 tag 断言、这里的自动升号与「必须变大」检查都只懂三段数字，口径一致。
    if ($current -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') {
        throw "Cargo.toml 当前的 version 是 $current，不是 X.Y.Z 三段数字，脚本升不了号也判不了大小，先把它理顺"
    }

    $autoBump = -not $Version
    if ($autoBump) {
        $Version = Get-NextVersion $current $bump
    }
    $tag = "v$Version"
    $how = if ($autoBump) { "（没给版本号，自动升 $bump）" } else { '' }

    # 上一版漏同步 manifest 的话，这里先拦住：脚本会把它改成新版本号，等于把错误悄悄
    # 抹平，而 manifest 的 version 本来该由测试里的用例盯着。
    $manifestText = [System.IO.File]::ReadAllText($ManifestPath)
    if (-not $manifestText.Contains('version="' + $current + '.0"')) {
        throw "p4delta.exe.manifest 的版本与 Cargo.toml 的 $current 对不上，先把上一版漏掉的同步补上"
    }

    if (-not (Test-VersionGreater $Version $current)) {
        throw "新版本 $Version 不比当前的 $current 大。同一版本不能发两次，要重发得先删掉远端 tag"
    }

    $null = & git -C $RepoRoot rev-parse -q --verify "refs/tags/$tag" 2>$null
    if ($LASTEXITCODE -eq 0) {
        throw "本地已经有 tag $tag"
    }

    & git -C $RepoRoot fetch --quiet $Remote $Branch
    if ($LASTEXITCODE -ne 0) {
        throw "取不到 $Remote/$Branch，检查网络与远端名（-Remote）"
    }

    # --exit-code：远端有同名 tag 时退出码 0，没有时 2，网络/权限问题再往上。只有 0 算「有」。
    $null = & git -C $RepoRoot ls-remote --exit-code --tags $Remote "refs/tags/$tag" 2>$null
    if ($LASTEXITCODE -eq 0) {
        throw "远端已经有 tag $tag。同一版本不能发两次：换一个版本号；确实要重发就先删掉远端 tag"
    }

    $behind = & git -C $RepoRoot rev-list --count 'HEAD..FETCH_HEAD'
    if ($LASTEXITCODE -ne 0) {
        throw "读不出本地落后 $Remote/$Branch 多少提交"
    }
    if ([int] $behind -ne 0) {
        throw "本地落后 $Remote/$Branch $behind 个提交，先 git pull —— 否则 tag 会打在旧提交上"
    }

    # ---- 到这里为止都没动过任何东西 ----

    if ($DryRun) {
        Write-Host 'dry run：检查都过了。真跑的话会做这些：'
        Write-Host "    版本号：$current → $Version$how"
        Write-Host '    1. 写进 Cargo.toml、p4delta.exe.manifest、Cargo.lock 三处'
        if ($SkipCheck) {
            Write-Host '    2. 跳过本地门禁（-SkipCheck）'
        } else {
            Write-Host '    2. 跑本地门禁：fmt / clippy / test'
        }
        Write-Host "    3. 提交，打附注 tag $tag"
        if ($NoPush) {
            Write-Host '    4. 不推送（-NoPush）'
        } else {
            Write-Host "    4. 推 $Branch 与 $tag 到 $Remote"
        }
        exit 0
    }

    Write-Host "改版本号：$current → $Version$how"

    Edit-TextFile $TomlPath '(?m)^version = "[^"]*"$' ('version = "' + $Version + '"')
    $found = ([regex]::Matches([System.IO.File]::ReadAllText($TomlPath), '(?m)^version = "' + [regex]::Escape($Version) + '"$')).Count
    if ($found -ne 1) {
        throw 'Cargo.toml 的 version 没改对，改动已留在工作区，先看再 git checkout'
    }

    # 行首锚定，为的是别碰同一行的 <assembly manifestVersion="1.0">；末尾不锚定，
    # 因为这一行后面还跟着 "/>"。
    Edit-TextFile $ManifestPath '(?m)^( *)version="[^"]*"' ('${1}version="' + $Version + '.0"')
    $manifestText = [System.IO.File]::ReadAllText($ManifestPath)
    $found = ([regex]::Matches($manifestText, '(?m)^ *version="' + [regex]::Escape("$Version.0") + '"')).Count
    if ($found -ne 1) {
        throw 'p4delta.exe.manifest 的 version 没改对，改动已留在工作区，先看再 git checkout'
    }
    if (-not $manifestText.Contains('manifestVersion="1.0"')) {
        throw 'p4delta.exe.manifest 的 manifestVersion 被改到了，改动已留在工作区，先看再 git checkout'
    }

    # Cargo.lock 里也有 p4delta 的版本号，不更新的话 CI 的 --locked 会直接红。
    # --offline：只更新工作区成员，不需要网络。
    Invoke-Cargo 'update', '--workspace', '--offline'
    $lockText = [System.IO.File]::ReadAllText($LockPath)
    $lockPattern = '(?m)^name = "p4delta"\nversion = "' + [regex]::Escape($Version) + '"$'
    if (([regex]::Matches($lockText, $lockPattern)).Count -ne 1) {
        throw 'Cargo.lock 里 p4delta 的版本没跟着更新'
    }

    if ($SkipCheck) {
        Write-Host '跳过本地门禁（-SkipCheck）'
    } else {
        Write-Host '本地门禁'
        Invoke-Cargo 'fmt', '--all', '--', '--check'
        Invoke-Cargo 'clippy', '--all-targets', '--all-features', '--locked', '--', '-D', 'warnings'
        # 默认档排掉了那条要跑十来秒的 Windows 失败注入用例（理由见 .config/nextest.toml）。
        # 这里不补跑：它在 Linux/macOS 上匹配 0 条用例，nextest 的 --no-tests 默认 fail 会让门禁
        # 无谓地红。发布流水线不缺它——release workflow 复用 ci.yml 的 test job，那边补跑。
        Invoke-Cargo 'nextest', 'run', '--all-targets', '--all-features', '--locked'
        Write-Host '    （机器上没有 p4/p4d 时 e2e 会自己跳过；CI 上不跳，那边设了 P4_E2E_REQUIRED。'
        Write-Host '     那条十来秒的失败注入用例也由 CI 的 Windows job 单独补跑，本地门禁不含它）'
    }

    Write-Host "提交并打 tag $tag"
    & git -C $RepoRoot add Cargo.toml Cargo.lock p4delta.exe.manifest
    if ($LASTEXITCODE -ne 0) {
        throw 'git add 失败'
    }
    & git -C $RepoRoot commit -m "chore: 发布 $tag"
    if ($LASTEXITCODE -ne 0) {
        throw 'git commit 失败'
    }
    & git -C $RepoRoot tag -a $tag -m $tag
    if ($LASTEXITCODE -ne 0) {
        throw 'git tag 失败'
    }

    if ($NoPush) {
        Write-Host '没推送（-NoPush）'
        Write-Host "    提交和 tag 都在本地了。要推的时候：$(Get-PushHint $tag)"
        exit 0
    }

    if (-not $Yes) {
        Write-Host "把提交与 tag $tag 推到 $Remote？[y/N] " -NoNewline
        $reply = ''
        try {
            # 非交互时（stdin 被关掉或已到 EOF）读不到回答：按「没确认」处理，走「没有推送」，
            # 而不是抛异常把脚本打断在半路。
            $reply = Read-Host
        } catch {
            $reply = ''
        }
        if ($reply -notmatch '^[yY]') {
            Write-Host '没有推送'
            Write-Host "    提交和 tag 都在本地了。要推的时候：$(Get-PushHint $tag)"
            exit 0
        }
    }

    Write-Host "推送到 $Remote"
    & git -C $RepoRoot push $Remote $Branch
    if ($LASTEXITCODE -ne 0) {
        throw "推 $Branch 失败。提交与 tag 都在本地，处理完这样重推：$(Get-PushHint $tag)"
    }
    & git -C $RepoRoot push $Remote $tag
    if ($LASTEXITCODE -ne 0) {
        throw "推 tag 失败。分支已经推上去了，修好之后：git push $Remote $tag"
    }

    Write-Host '推完了'
    # 认得三种 GitHub 远端写法就够；认不出来（比如自建服务）就只打印上面那几行，不瞎猜链接。
    $slug = ''
    $url = & git -C $RepoRoot remote get-url $Remote 2>$null
    if ($LASTEXITCODE -eq 0 -and $url) {
        switch -Regex ($url) {
            '^git@github\.com:(.+)$' { $slug = $Matches[1] }
            '^ssh://git@github\.com/(.+)$' { $slug = $Matches[1] }
            '^https://github\.com/(.+)$' { $slug = $Matches[1] }
        }
        $slug = $slug -replace '\.git$', ''
    }
    if ($slug) {
        Write-Host "    release.yml 开始跑了：https://github.com/$slug/actions/workflows/release.yml"
        Write-Host '    盯进度：gh run watch'
    }
    Write-Host '    release 建好后，在装了 P4V 的机器上按 docs/dev/release.md「发布检查清单」的最后一步核对一遍'
} catch {
    # 走 host 的错误通道而不是 Write-Host：Write-Host 在 -File 子进程里落的是 stdout，
    # 而发布脚本的错误一直（bash 版起）在 stderr 上，黑盒测试也按 stderr 断言。
    $Host.UI.WriteErrorLine("error: $($_.Exception.Message)")
    exit 1
}

exit 0
