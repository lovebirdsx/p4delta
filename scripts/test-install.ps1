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

# 逐字节比较两份快照。长度相同不代表内容相同，而「文件不该被重写」的断言要的正是内容一致。
function Assert-BytesEqual([byte[]] $Expected, [byte[]] $Actual, [string] $Message) {
    Assert-Equal $Expected.Length $Actual.Length "$Message（长度）"
    for ($i = 0; $i -lt $Expected.Length; $i++) {
        if ($Expected[$i] -ne $Actual[$i]) {
            throw "断言失败：$Message（偏移 $i）"
        }
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
    # -NoP4Check 与 -Force 都是**消除机器状态**，不是被测对象：这几条用例测的是写文件的
    # 行为。前者跳过 p4 可用性检查；后者跳过「P4V 正在运行」那道守卫——开发机上 P4V 常驻
    # 是常态，而那道守卫的判据（Get-Process p4v）在别处没有用例覆盖，不该让它把整套用例
    # 拦在门外。
    $arguments = @(
        '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $DistDir 'install.ps1'),
        '-InstallDir', $InstallDir, '-CustomToolsPath', $ToolsPath, '-NoP4Check', '-Force', '-Quiet'
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
    Assert-Equal 3 $doc.SelectNodes('//CustomToolDef').Count '默认应当注册三个工具'
    Assert-Equal 'customtooldeflist' $doc.DocumentElement.GetAttribute('varName') '根元素属性'
    Assert-OurTool $doc $ReconcileTool '-a -w $c -l %D'
    Assert-OurTool $doc $CleanPreviewTool '--clean -w $c -l %D'
    Assert-OurTool $doc $CleanApplyTool '-a --clean -w $c -l %D'

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
    Assert-Equal 4 $doc.SelectNodes('//CustomToolDef').Count '别人的工具加上我们的三个'
    Assert-OurTool $doc $ReconcileTool '-a -w $c -l %D'

    # 幂等：再跑一次，内容一字不差，也不该多出备份。默认注册现在含 APPLY 及其子菜单，
    # 这一段顺带覆盖了「连着装两次子菜单」的幂等。
    $before = [System.IO.File]::ReadAllBytes($ToolsPath)
    $backupsBefore = Get-BackupCount

    # 备份文件名只精确到秒。两次运行落在同一秒里，重写产生的备份会覆盖掉上一次的，
    # 「备份数没变」这条断言就会在真的重写时也通过——这里跨过一秒，让它真的能拦住。
    Start-Sleep -Seconds 1
    Invoke-Installer

    Assert-BytesEqual $before ([System.IO.File]::ReadAllBytes($ToolsPath)) '第二次运行改动了文件'
    Assert-Equal $backupsBefore (Get-BackupCount) '内容没变就不该产生新的备份'
    Assert-OtherToolIntact (Read-ToolsDocument)
}

function Test-ExePathInstallsThatExe {
    Reset-Root

    # 换一处 exe：真实场景是 target\release\p4delta.exe（scripts\install-local.ps1 装本地构建
    # 走的就是这条路）。内容与同目录那份不同，装错了看得出来。
    $elsewhere = Join-Path $Root 'elsewhere'
    New-Item -ItemType Directory -Force -Path $elsewhere | Out-Null
    $otherExe = Join-Path $elsewhere 'p4delta.exe'
    Write-Utf8NoBom $otherExe "another fake executable$([Environment]::NewLine)"

    Invoke-Installer @('-ExePath', $otherExe)

    $installed = Join-Path $InstallDir 'p4delta.exe'
    Assert-True (Test-Path -LiteralPath $installed) 'exe 应当被铺到安装目录'
    Assert-Equal ([System.IO.File]::ReadAllText($otherExe)) ([System.IO.File]::ReadAllText($installed)) '-ExePath 指定的那份才该被装上'
    Assert-OurTool (Read-ToolsDocument) $ReconcileTool '-a -w $c -l %D'
}

function Test-MissingExePathFails {
    Reset-Root

    $missing = Join-Path $Root 'nope\p4delta.exe'
    $failed = $false
    try {
        Invoke-Installer @('-ExePath', $missing)
    } catch {
        $failed = $true
    }

    Assert-True $failed '-ExePath 指向不存在的文件时 install.ps1 应当失败'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $InstallDir 'p4delta.exe'))) '失败时不该铺出 exe'
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

    # APPLY 顺带加码：参数改坏**并且**挪出子菜单。重装要连位置一起修回来，
    # 这条覆盖 Update-ToolList 里按位置比对的 $inRightFolder 分支。
    $apply = Get-Tool $doc $CleanApplyTool
    $apply.SelectSingleNode('Definition/Arguments').InnerText = '--stale'
    [void]$apply.ParentNode.RemoveChild($apply)
    [void]$doc.DocumentElement.AppendChild($apply)
    $doc.Save($ToolsPath)

    Invoke-Installer

    $doc = Read-ToolsDocument
    Assert-Equal 3 $doc.SelectNodes('//CustomToolDef').Count '改回来时不该多出节点'
    Assert-OurTool $doc $ReconcileTool '-a -w $c -l %D'
    Assert-OurTool $doc $CleanApplyTool '-a --clean -w $c -l %D'
    Assert-Equal 1 $doc.SelectNodes('//CustomToolFolder').Count '子菜单不该被重复创建'
    $inFolder = $doc.SelectSingleNode(
        "//CustomToolFolder[Name='$CleanApplyFolder']//CustomToolDef[Definition/Name='$CleanApplyTool']")
    Assert-True ($null -ne $inFolder) '被挪出去的 APPLY 应当被放回子菜单里'
}

function Test-CleanApplyUsesASubmenu {
    Reset-Root
    Invoke-Installer

    $doc = Read-ToolsDocument
    Assert-Equal 3 $doc.SelectNodes('//CustomToolDef').Count '默认三个工具'
    Assert-OurTool $doc $CleanApplyTool '-a --clean -w $c -l %D'

    $folders = $doc.SelectNodes('//CustomToolFolder')
    Assert-Equal 1 $folders.Count '不可逆的那个应当单独放在一个子菜单里'
    Assert-Equal $CleanApplyFolder (Get-ChildText $folders[0] 'Name') '子菜单名'
    $inFolder = $folders[0].SelectSingleNode(".//CustomToolDef[Definition/Name='$CleanApplyTool']")
    Assert-True ($null -ne $inFolder) '不可逆的工具应当在这个子菜单里'
    Assert-True ($null -eq (Get-Tool $doc $ReconcileTool).SelectSingleNode('ancestor::CustomToolFolder')) 'Reconcile 不该被塞进子菜单'
    Assert-True ($null -eq (Get-Tool $doc $CleanPreviewTool).SelectSingleNode('ancestor::CustomToolFolder')) 'clean 预演也不该被塞进子菜单'
}

function Test-WithoutCleanApplySkipsCleanApply {
    Reset-Root
    Invoke-Installer @('-WithoutCleanApply')

    $doc = Read-ToolsDocument
    Assert-Equal 2 $doc.SelectNodes('//CustomToolDef').Count '-WithoutCleanApply 时只有两个工具'
    Assert-OurTool $doc $ReconcileTool '-a -w $c -l %D'
    Assert-OurTool $doc $CleanPreviewTool '--clean -w $c -l %D'
    Assert-True ($null -eq (Get-Tool $doc $CleanApplyTool)) '不可逆的 clean 不该注册'
    # 全新机器上跑退出口，不该凭空造出（或留下）子菜单目录。
    Assert-Equal 0 $doc.SelectNodes('//CustomToolFolder').Count '不该有子菜单目录'
}

function Test-WithoutCleanApplyRemovesRegisteredCleanApply {
    Reset-Root
    Write-Utf8NoBom $ToolsPath $OtherToolXml
    Invoke-Installer
    Assert-Equal 4 (Read-ToolsDocument).SelectNodes('//CustomToolDef').Count '先按默认装齐：别人的一个加我们的三个'

    # -WhatIf：移除只发生在内存里，文件一字不动，也不该产生备份。
    $before = [System.IO.File]::ReadAllBytes($ToolsPath)
    $backupsBefore = Get-BackupCount
    Invoke-Installer @('-WithoutCleanApply', '-WhatIf')
    Assert-BytesEqual $before ([System.IO.File]::ReadAllBytes($ToolsPath)) '-WhatIf 改动了文件'
    Assert-Equal $backupsBefore (Get-BackupCount) '-WhatIf 不该产生备份'

    # 真摘：APPLY 与空掉的子菜单都没了，别人的工具与安全的两条原样。
    # 备份文件名只精确到秒，先跨过一秒——否则这次的备份会覆盖掉上面安装时那份，
    # 「多出一份备份」的断言就会假失败（pwsh 跑得快时真的撞上过）。
    Start-Sleep -Seconds 1
    Invoke-Installer @('-WithoutCleanApply')
    $doc = Read-ToolsDocument
    Assert-Equal 3 $doc.SelectNodes('//CustomToolDef').Count '别人的一个加我们的两个'
    Assert-True ($null -eq (Get-Tool $doc $CleanApplyTool)) 'APPLY 条目应当被摘掉'
    Assert-Equal 0 $doc.SelectNodes('//CustomToolFolder').Count '空掉的子菜单目录应当被清掉'
    Assert-OtherToolIntact $doc
    Assert-OurTool $doc $ReconcileTool '-a -w $c -l %D'
    Assert-OurTool $doc $CleanPreviewTool '--clean -w $c -l %D'
    Assert-Equal ($backupsBefore + 1) (Get-BackupCount) '真摘掉了就该走一次备份 + 保存'

    # 退出口自身幂等：再跑一次，字节不变、不多出备份。跨过一秒的理由同上一条用例。
    $before = [System.IO.File]::ReadAllBytes($ToolsPath)
    Start-Sleep -Seconds 1
    Invoke-Installer @('-WithoutCleanApply')
    Assert-BytesEqual $before ([System.IO.File]::ReadAllBytes($ToolsPath)) '第二次退出口改动了文件'
    Assert-Equal ($backupsBefore + 1) (Get-BackupCount) '没变化就不该产生新的备份'
}

function Test-WithoutCleanApplyKeepsFolderWithForeignTools {
    Reset-Root
    Invoke-Installer

    # 用户往我们的子菜单里塞了自己的工具：退出口只摘 APPLY，子菜单得留着。
    $doc = Read-ToolsDocument
    $folderList = $doc.SelectSingleNode("//CustomToolFolder[Name='$CleanApplyFolder']/CustomToolDefList")
    Assert-True ($null -ne $folderList) '默认安装应当建出子菜单'
    $otherDoc = New-Object System.Xml.XmlDocument
    $otherDoc.LoadXml($OtherToolXml)
    [void]$folderList.AppendChild($doc.ImportNode($otherDoc.SelectSingleNode('//CustomToolDef'), $true))
    $doc.Save($ToolsPath)

    Invoke-Installer @('-WithoutCleanApply')

    $doc = Read-ToolsDocument
    Assert-True ($null -eq (Get-Tool $doc $CleanApplyTool)) 'APPLY 仍然要被摘掉'
    Assert-True ($null -ne (Get-Tool $doc $OtherTool)) '塞进去的工具不该被动'
    Assert-Equal 3 $doc.SelectNodes('//CustomToolDef').Count '我们的两条加塞进来的一条'
    Assert-Equal 1 $doc.SelectNodes('//CustomToolFolder').Count '子菜单里还有别人的工具，就该留着'
}

function Test-Uninstall {
    Reset-Root
    Write-Utf8NoBom $ToolsPath $OtherToolXml
    Invoke-Installer
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
    'Test-ExePathInstallsThatExe',
    'Test-MissingExePathFails',
    'Test-KeepsForeignToolsAndIsIdempotent',
    'Test-UpdatesATamperedDefinition',
    'Test-CleanApplyUsesASubmenu',
    'Test-WithoutCleanApplySkipsCleanApply',
    'Test-WithoutCleanApplyRemovesRegisteredCleanApply',
    'Test-WithoutCleanApplyKeepsFolderWithForeignTools',
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
