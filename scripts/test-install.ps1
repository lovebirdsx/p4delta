#Requires -Version 5.1
<#
.SYNOPSIS
    install.ps1 的黑盒测试。

.DESCRIPTION
    把 install.ps1 和一个人造的 p4delta.exe 拷进临时目录再跑，复刻「从 release 包里
    解压出来直接运行」的布局——同时保证测试不需要先 cargo build，也保证 install.ps1
    绝不执行它安装的那个 exe。

    断言失败即 throw，退出码非 0；风格对齐 scripts/fetch-p4-tools.sh。

    不碰真实用户配置：安装目录与 customtools.xml 都重定向到临时目录，且一律带
    -NoP4Check。注册表只在 -AddToPath 时才会被碰，这里不测那条路径。
#>
[CmdletBinding()]
param(
    # 失败时保留现场，方便手工看生成的文件。
    [switch] $Keep
)

$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$SourceScript = Join-Path $RepoRoot 'install.ps1'

# 用当前解释器跑子进程：CI 会用 Windows PowerShell 5.1 与 PowerShell 7 各跑一遍这个脚本，
# 两遍合起来才说明 install.ps1 在两种解释器下都对。
$HostExe = (Get-Process -Id $PID).Path

$Root = Join-Path ([System.IO.Path]::GetTempPath()) "p4delta-install-test-$PID"
$DistDir = Join-Path $Root 'dist'
$InstallDir = Join-Path $Root 'installed'
$ToolsPath = Join-Path $Root '.p4qt\customtools.xml'

$ReconcileTool = 'p4delta Reconcile'
$CleanPreviewTool = 'p4delta Clean (preview)'
$CleanApplyTool = 'p4delta Clean (APPLY - irreversible)'
$CleanApplyFolder = 'p4delta (irreversible)'
$OtherTool = '别人的工具 & 中文'

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

function Write-Utf8NoBom([string] $Path, [string] $Content) {
    $dir = Split-Path -Parent $Path
    if ($dir -and -not (Test-Path -LiteralPath $dir)) {
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
    }
    [System.IO.File]::WriteAllText($Path, $Content, (New-Object System.Text.UTF8Encoding($false)))
}

function Reset-Root {
    if (Test-Path -LiteralPath $Root) {
        Remove-Item -LiteralPath $Root -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $DistDir | Out-Null

    Copy-Item -LiteralPath $SourceScript -Destination (Join-Path $DistDir 'install.ps1') -Force
    # 人造 exe：内容刻意不是可执行文件，install.ps1 要是真去跑它就会炸。
    Write-Utf8NoBom (Join-Path $DistDir 'p4delta.exe') "this is not a real executable$([Environment]::NewLine)"
}

function Invoke-Installer([string[]] $ExtraArguments = @()) {
    $arguments = @(
        '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $DistDir 'install.ps1'),
        '-InstallDir', $InstallDir, '-CustomToolsPath', $ToolsPath, '-NoP4Check', '-Quiet'
    ) + $ExtraArguments

    & $HostExe @arguments
    if ($LASTEXITCODE -ne 0) {
        throw "install.ps1 退出码 $LASTEXITCODE（参数：$($ExtraArguments -join ' ')）"
    }
}

function Read-ToolsDocument {
    $doc = New-Object System.Xml.XmlDocument
    $doc.Load($ToolsPath)
    return $doc
}

function Get-Tool($doc, [string] $Name) {
    foreach ($tool in $doc.SelectNodes('//CustomToolDef')) {
        $nameNode = $tool.SelectSingleNode('Definition/Name')
        if ($nameNode -and $nameNode.InnerText -eq $Name) {
            return $tool
        }
    }
    return $null
}

function Get-ChildText($node, [string] $path) {
    $child = $node.SelectSingleNode($path)
    if ($null -eq $child) { return $null }
    return $child.InnerText
}

function Get-BackupCount {
    return @(Get-ChildItem -LiteralPath (Split-Path -Parent $ToolsPath) -Filter 'customtools.xml.p4delta-backup-*' -ErrorAction SilentlyContinue).Count
}

$OtherToolXml = @"
<?xml version="1.0" encoding="UTF-8"?>
<!--perforce-xml-version=1.0-->
<CustomToolDefList varName="customtooldeflist">
  <CustomToolDef>
    <Definition>
      <Name>别人的工具 &amp; 中文</Name>
      <Command>C:\tools\other.exe</Command>
      <Arguments>--x &amp; --y</Arguments>
      <Shortcut>Ctrl+1</Shortcut>
    </Definition>
    <Prompt>
      <PromptText>说点什么</PromptText>
      <ShowBrowse>true</ShowBrowse>
    </Prompt>
    <Console>
      <CloseOnExit>true</CloseOnExit>
    </Console>
    <AddToContext>false</AddToContext>
    <Refresh>false</Refresh>
  </CustomToolDef>
</CustomToolDefList>
"@

function Assert-OtherToolIntact($doc) {
    $other = Get-Tool $doc $OtherTool
    Assert-True ($null -ne $other) '第三方的工具不该被删掉'
    Assert-Equal 'C:\tools\other.exe' (Get-ChildText $other 'Definition/Command') '第三方的 Command 不该被改'
    Assert-Equal '--x & --y' (Get-ChildText $other 'Definition/Arguments') '第三方的 Arguments 不该被改'
    Assert-Equal 'Ctrl+1' (Get-ChildText $other 'Definition/Shortcut') '第三方的 Shortcut 不该被改'
    Assert-Equal '说点什么' (Get-ChildText $other 'Prompt/PromptText') '第三方的 Prompt 不该被丢'
    Assert-Equal 'true' (Get-ChildText $other 'Prompt/ShowBrowse') '第三方的 ShowBrowse 不该被丢'
    Assert-Equal 'true' (Get-ChildText $other 'Console/CloseOnExit') '第三方的 Console 设置不该被改'
    Assert-Equal 'false' (Get-ChildText $other 'AddToContext') '第三方的 AddToContext 不该被改'
    Assert-Equal 'false' (Get-ChildText $other 'Refresh') '第三方的 Refresh 不该被改'
}

function Assert-OurTool($doc, [string] $Name, [string] $Arguments) {
    $tool = Get-Tool $doc $Name
    Assert-True ($null -ne $tool) "应当注册 $Name"
    Assert-Equal (Join-Path $InstallDir 'p4delta.exe') (Get-ChildText $tool 'Definition/Command') "$Name 的 Command 应当是安装路径"
    Assert-Equal $Arguments (Get-ChildText $tool 'Definition/Arguments') "$Name 的 Arguments"
    Assert-Equal '$r' (Get-ChildText $tool 'Definition/InitDir') "$Name 的 Start in 应当是 `$r"
    Assert-Equal 'false' (Get-ChildText $tool 'Console/CloseOnExit') "$Name 不该勾 Close window upon completion"
    Assert-Equal 'true' (Get-ChildText $tool 'AddToContext') "$Name 该进右键菜单"
    Assert-Equal 'true' (Get-ChildText $tool 'Refresh') "$Name 该刷新 P4V"
    Assert-True ($null -eq $tool.SelectSingleNode('Prompt')) "$Name 不该勾 Prompt for arguments"
}

# ---- 用例 ----

function Test-InstallLayoutIsSelfContained {
    # 脚本自己带 BOM：Windows PowerShell 5.1 没有 BOM 时会按系统 ANSI 代码页解析，
    # 里面的中文全变乱码。这条断言拦住「顺手把 BOM 存掉」。
    $head = [System.IO.File]::ReadAllBytes($SourceScript)[0..2]
    Assert-True (($head[0] -eq 0xEF) -and ($head[1] -eq 0xBB) -and ($head[2] -eq 0xBF)) 'install.ps1 必须以 UTF-8 BOM 开头'
}

function Test-FreshInstall {
    Reset-Root
    Invoke-Installer

    Assert-True (Test-Path -LiteralPath (Join-Path $InstallDir 'p4delta.exe')) 'exe 应当被铺到安装目录'
    Assert-True (Test-Path -LiteralPath (Join-Path $InstallDir 'install.ps1')) '脚本应当自留一份，方便日后卸载'
    Assert-True (Test-Path -LiteralPath $ToolsPath) '应当新建自定义工具文件'

    $doc = Read-ToolsDocument
    Assert-Equal 2 $doc.SelectNodes('//CustomToolDef').Count '默认应当注册两个工具'
    Assert-Equal 'customtooldeflist' $doc.DocumentElement.GetAttribute('varName') '根元素属性'
    Assert-OurTool $doc $ReconcileTool '-a -w $c -l %D'
    Assert-OurTool $doc $CleanPreviewTool '--clean -w $c -l %D'
    Assert-True ($null -eq (Get-Tool $doc $CleanApplyTool)) '不可逆的 clean 默认不该注册'

    # 编码：P4V 自己导出的文件没有 BOM，这里也不该有。
    $head = [System.IO.File]::ReadAllBytes($ToolsPath)[0..2]
    Assert-True (-not (($head[0] -eq 0xEF) -and ($head[1] -eq 0xBB) -and ($head[2] -eq 0xBF))) '自定义工具文件不该带 BOM'
}

function Test-KeepsForeignToolsAndIsIdempotent {
    Reset-Root
    Write-Utf8NoBom $ToolsPath $OtherToolXml

    Invoke-Installer
    $doc = Read-ToolsDocument
    Assert-OtherToolIntact $doc
    Assert-Equal 3 $doc.SelectNodes('//CustomToolDef').Count '别人的工具加上我们的两个'
    Assert-OurTool $doc $ReconcileTool '-a -w $c -l %D'

    # 幂等：再跑一次，内容一字不差，也不该多出备份。
    $before = [System.IO.File]::ReadAllBytes($ToolsPath)
    $backupsBefore = Get-BackupCount

    # 备份文件名只精确到秒。两次运行落在同一秒里，重写产生的备份会覆盖掉上一次的，
    # 「备份数没变」这条断言就会在真的重写时也通过——这里跨过一秒，让它真的能拦住。
    Start-Sleep -Seconds 1
    Invoke-Installer
    $after = [System.IO.File]::ReadAllBytes($ToolsPath)

    Assert-Equal $before.Length $after.Length '第二次运行不该改动文件长度'
    for ($i = 0; $i -lt $before.Length; $i++) {
        if ($before[$i] -ne $after[$i]) {
            throw "断言失败：第二次运行改动了文件（偏移 $i）"
        }
    }
    Assert-Equal $backupsBefore (Get-BackupCount) '内容没变就不该产生新的备份'
    Assert-OtherToolIntact (Read-ToolsDocument)
}

function Test-UpdatesATamperedDefinition {
    Reset-Root
    Invoke-Installer

    # 幂等的反面：定义被改坏（手工改过，或者是旧版本装的）时必须改回来。
    # 没有这一条的话，上面那条「内容没变就不重写」可以靠「永远不重写」蒙混过关。
    $doc = Read-ToolsDocument
    $tool = Get-Tool $doc $ReconcileTool
    $tool.SelectSingleNode('Definition/Arguments').InnerText = '--stale'
    $tool.SelectSingleNode('Definition/Command').InnerText = 'C:\old\p4delta.exe'
    $doc.Save($ToolsPath)

    Invoke-Installer

    $doc = Read-ToolsDocument
    Assert-Equal 2 $doc.SelectNodes('//CustomToolDef').Count '改回来时不该多出节点'
    Assert-OurTool $doc $ReconcileTool '-a -w $c -l %D'
    Assert-True ($null -eq (Get-Tool $doc $CleanApplyTool)) '没要 clean 实际清理时不该冒出来'
}

function Test-WithCleanApplyUsesASubmenu {
    Reset-Root
    Invoke-Installer @('-WithCleanApply')

    $doc = Read-ToolsDocument
    Assert-Equal 3 $doc.SelectNodes('//CustomToolDef').Count '-WithCleanApply 时三个工具'
    Assert-OurTool $doc $CleanApplyTool '-a --clean -w $c -l %D'

    $folders = $doc.SelectNodes('//CustomToolFolder')
    Assert-Equal 1 $folders.Count '不可逆的那个应当单独放在一个子菜单里'
    Assert-Equal $CleanApplyFolder (Get-ChildText $folders[0] 'Name') '子菜单名'
    $inFolder = $folders[0].SelectSingleNode(".//CustomToolDef[Definition/Name='$CleanApplyTool']")
    Assert-True ($null -ne $inFolder) '不可逆的工具应当在这个子菜单里'
    Assert-True ($null -eq (Get-Tool $doc $ReconcileTool).SelectSingleNode('ancestor::CustomToolFolder')) '安全的那两个不该被塞进子菜单'
}

function Test-Uninstall {
    Reset-Root
    Write-Utf8NoBom $ToolsPath $OtherToolXml
    Invoke-Installer @('-WithCleanApply')
    Assert-Equal 4 (Read-ToolsDocument).SelectNodes('//CustomToolDef').Count '装完应当是他们的一个加我们的三个'

    Invoke-Installer @('-Uninstall')

    $doc = Read-ToolsDocument
    Assert-Equal 1 $doc.SelectNodes('//CustomToolDef').Count '卸载后只剩下别人的工具'
    Assert-OtherToolIntact $doc
    Assert-Equal 0 $doc.SelectNodes('//CustomToolFolder').Count '空掉的子菜单也该清掉'
    Assert-True (-not (Test-Path -LiteralPath $InstallDir)) '安装目录应当被删掉'

    # 再卸一次：什么都不该发生，更不该报错。
    Invoke-Installer @('-Uninstall')
    Assert-Equal 1 (Read-ToolsDocument).SelectNodes('//CustomToolDef').Count '重复卸载不该动别人的工具'
}

function Test-WhatIfChangesNothing {
    Reset-Root
    Invoke-Installer @('-WhatIf')

    Assert-True (-not (Test-Path -LiteralPath $InstallDir)) '-WhatIf 不该创建安装目录'
    Assert-True (-not (Test-Path -LiteralPath $ToolsPath)) '-WhatIf 不该写自定义工具文件'
}

$cases = @(
    'Test-InstallLayoutIsSelfContained',
    'Test-FreshInstall',
    'Test-KeepsForeignToolsAndIsIdempotent',
    'Test-UpdatesATamperedDefinition',
    'Test-WithCleanApplyUsesASubmenu',
    'Test-Uninstall',
    'Test-WhatIfChangesNothing'
)

$failed = 0
Write-Host "install.ps1 测试（$([System.IO.Path]::GetFileName($HostExe))）"

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
Write-Host "全部通过"
exit 0
