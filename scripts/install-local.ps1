#Requires -Version 5.1
<#
.SYNOPSIS
    把**本地构建**的 p4delta.exe 装进安装目录，供在 P4V 里验证。

.DESCRIPTION
    四步：cargo build → 冒烟跑一次 --version → 备份安装目录里现有的 exe → 调 install.ps1
    把它铺好并注册 P4V 工具。铺设与注册那段逻辑不在这里重写：两处各写一份迟早会漂。

    装完**不用重启 P4V**：工具定义里的 Command 是 exe 的绝对路径，P4V 每次点菜单都新起一个
    进程，只要工具定义本身没变，下一次点击用的就是新 exe。只有工具定义变了（换安装目录、
    改参数）才需要重启。

    备份只做一次：第一次本地安装时把当时那份 exe 存成 p4delta.exe.p4delta-backup-<时间戳>，
    之后反复安装不再覆盖它——否则连着装几个本地构建，能还原回去的「原件」会被自己的中间
    产物顶掉。-Restore 把这份备份放回去，放完就删掉它，下次本地安装会重新捕捉。

.PARAMETER DebugBuild
    装 debug 构建。编译快、跑得慢，适合反复改代码时用。默认装 release。
    （不能叫 -Debug：那是 [CmdletBinding()] 自带的公共参数。）

.PARAMETER NoBuild
    跳过 cargo build，直接装已有的构建产物。

.PARAMETER Restore
    把 p4delta.exe.p4delta-backup-* 里最新的那份放回安装目录，回到第一次本地安装之前的状态。
    不构建、不碰工具定义。

.PARAMETER InstallDir
    安装目录，默认与 install.ps1 一致（%LOCALAPPDATA%\Programs\p4delta）。

.PARAMETER CustomToolsPath
    P4V 自定义工具文件，默认与 install.ps1 一致。

.PARAMETER WithoutCleanApply
    透传给 install.ps1：不注册不可逆的「clean 实际清理」（默认注册；以前装过的会被摘掉）。

.PARAMETER Quiet
    透传给 install.ps1：只输出警告与错误。

.EXAMPLE
    .\scripts\install-local.ps1
    编译 release、装到默认位置、注册 P4V 工具。

.EXAMPLE
    .\scripts\install-local.ps1 -DebugBuild
    装 debug 构建，快速迭代时省编译时间。

.EXAMPLE
    .\scripts\install-local.ps1 -WithoutCleanApply
    不注册不可逆的「clean 实际清理」；以前装过的会被摘掉。

.EXAMPLE
    .\scripts\install-local.ps1 -Restore
    把本地安装之前的那份 exe 放回去。
#>
[CmdletBinding()]
param(
    [switch] $DebugBuild,
    [switch] $NoBuild,
    [switch] $Restore,
    [string] $InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\p4delta'),
    [string] $CustomToolsPath = (Join-Path $env:USERPROFILE '.p4qt\customtools.xml'),
    [switch] $WithoutCleanApply,
    [switch] $Quiet
)

$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$InstallScript = Join-Path $RepoRoot 'install.ps1'
$ExeName = 'p4delta.exe'
$BackupFilter = "$ExeName.p4delta-backup-*"

function Write-Info([string] $Message) {
    if (-not $Quiet) {
        Write-Host $Message
    }
}

# 版本号来自 VERSIONINFO（build.rs 注入）。本地构建与发布版同号，所以它只回答「这是一份
# 能起来的 p4delta」；辨认装的是哪份代码要靠下面的 git 修订号。
function Get-ExeVersion([string] $Path) {
    try {
        $info = (Get-Item -LiteralPath $Path).VersionInfo
        if ($info -and $info.ProductVersion) {
            return $info.ProductVersion
        }
    } catch {
        # 读不到版本资源不是错误，只是没有可显示的信息。
    }
    return $null
}

# 这份构建对应哪份代码。有未提交改动时必须显眼：验证时最容易搞混的就是「我验的到底是不是
# 刚才那次修改」——本地构建和发布版版本号一样，光看版本号分不出来。
function Get-SourceRevision {
    try {
        $hash = & git -C $RepoRoot rev-parse --short HEAD 2>$null
        if ($LASTEXITCODE -ne 0 -or -not $hash) {
            return $null
        }
        if (& git -C $RepoRoot status --porcelain 2>$null) {
            return "$hash（有未提交改动）"
        }
        return $hash
    } catch {
        return $null
    }
}

# 安装目录里的备份；没有则返回 $null。可能有手工留下的多份，取最新的那份。
function Find-Backup([string] $Dir) {
    $backups = @(Get-ChildItem -LiteralPath $Dir -Filter $BackupFilter -File -ErrorAction SilentlyContinue |
        Sort-Object Name -Descending)
    if ($backups.Count -eq 0) {
        return $null
    }
    return $backups[0]
}

function Invoke-Restore {
    $targetExe = Join-Path $InstallDir $ExeName
    $backup = Find-Backup $InstallDir
    if (-not $backup) {
        throw "没有可还原的备份：$InstallDir 下找不到 $BackupFilter。要么这里还没被本地安装覆盖过，要么备份已经还原掉了（那就跑一次 release 包里的 install.ps1）。"
    }

    Copy-Item -LiteralPath $backup.FullName -Destination $targetExe -Force
    $version = Get-ExeVersion $targetExe
    if ($version) {
        Write-Info "已还原 $targetExe（p4delta $version）"
    } else {
        Write-Info "已还原 $targetExe"
    }

    try {
        Remove-Item -LiteralPath $backup.FullName -Force
        Write-Info "备份 $($backup.Name) 已移除；下次本地安装会重新捕捉当时的那一份。"
    } catch {
        Write-Warning "还原完成，但备份没删掉：$($_.Exception.Message)"
    }

    Write-Info '工具定义没动，P4V 也不用重启。'
}

function Invoke-LocalInstall {
    if (-not (Test-Path -LiteralPath $InstallScript -PathType Leaf)) {
        throw "没找到 $InstallScript —— 本脚本要放在仓库的 scripts\ 目录里跑。"
    }

    $profileName = if ($DebugBuild) { 'debug' } else { 'release' }
    $exePath = Join-Path $RepoRoot "target\$profileName\$ExeName"

    if ($NoBuild) {
        Write-Info "跳过构建（-NoBuild），直接用 $exePath"
    } else {
        if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
            throw '没找到 cargo：装好 Rust 工具链并把它放进 PATH，或者用 -NoBuild 装已有的产物。'
        }
        $cargoArgs = @('build')
        if (-not $DebugBuild) {
            $cargoArgs += '--release'
        }
        Write-Info "cargo $($cargoArgs -join ' ')（在 $RepoRoot）"
        Push-Location $RepoRoot
        try {
            & cargo @cargoArgs
            if ($LASTEXITCODE -ne 0) {
                throw "cargo build 失败（退出码 $LASTEXITCODE）"
            }
        } finally {
            Pop-Location
        }
    }

    if (-not (Test-Path -LiteralPath $exePath -PathType Leaf)) {
        throw "构建产物不存在：$exePath"
    }

    # 冒烟：装一个起不来的 exe 进 P4V，报错只会出现在 P4V 的输出窗格里，很难查；
    # 在这里跑一下只要几十毫秒。
    $version = & $exePath --version
    if ($LASTEXITCODE -ne 0) {
        throw "$exePath --version 退出码 $LASTEXITCODE，这份构建不能用。"
    }
    $revision = Get-SourceRevision
    if ($revision) {
        Write-Info "本地构建：$version  $revision"
    } else {
        Write-Info "本地构建：$version"
    }

    $targetExe = Join-Path $InstallDir $ExeName
    $existing = Find-Backup $InstallDir
    if ($existing) {
        Write-Info "安装目录里已有备份 $($existing.Name)，保留不动。"
    } elseif (Test-Path -LiteralPath $targetExe -PathType Leaf) {
        $backup = Join-Path $InstallDir ("$ExeName.p4delta-backup-" + (Get-Date -Format 'yyyyMMdd-HHmmss'))
        New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
        Copy-Item -LiteralPath $targetExe -Destination $backup -Force
        Write-Info "已备份现有的 exe：$backup（-Restore 可以放回去）"
    } else {
        Write-Info '安装目录里还没有 exe，不需要备份。'
    }

    # 按名传参只能用**哈希** splat。数组 splat 是把元素按位置传过去的，'-ExePath' 这样的字符串
    # 不会被当成参数名：实测（pwsh 7.6）`& $script @('-ExePath','X','-InstallDir','Y')` 会把
    # '-ExePath' 顶到第 0 个位置参数 -InstallDir 上，'X' 顶到 -CustomToolsPath，'Y' 再往下
    # 就无处可去，报「找不到接受自变量 'Y' 的位置参数」——看着像参数名写错了，其实是 splat 形式错。
    $installArgs = @{
        ExePath         = $exePath
        InstallDir      = $InstallDir
        CustomToolsPath = $CustomToolsPath
    }
    if ($WithoutCleanApply) {
        $installArgs.WithoutCleanApply = $true
    }
    if ($Quiet) {
        $installArgs.Quiet = $true
    }
    # P4V 常驻时 install.ps1 默认拒绝往下走（它怕 P4V 退出时用内存里的工具列表覆盖这次写入）。
    # 本地安装通常不改工具定义，而内容没变时 install.ps1 根本不写那个文件，所以这里检测到
    # P4V 在跑就自动带上 -Force，省得每验一次都要关一次 P4V。
    if (Get-Process -Name p4v -ErrorAction SilentlyContinue) {
        Write-Info 'P4V 正在运行：带 -Force 继续（工具定义没变时不会写那个文件）。'
        $installArgs.Force = $true
    }

    & $InstallScript @installArgs
    if ($LASTEXITCODE -ne 0) {
        throw "install.ps1 退出码 $LASTEXITCODE"
    }
}

try {
    if ($Restore) {
        Invoke-Restore
    } else {
        Invoke-LocalInstall
    }
} catch {
    Write-Host "error: $($_.Exception.Message)" -ForegroundColor Red
    exit 1
}

exit 0
