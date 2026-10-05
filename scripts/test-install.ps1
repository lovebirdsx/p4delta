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
    -NoP4Check。PATH 上，除最后一条用例（唯一碰真注册表的：先快照、finally 还原）外，
    其余一律带 -WithoutPath，绝不碰这台机器的 PATH。也**别同时跑两份测试**——两份快照
    会互相覆盖。

    那条唯一碰真注册表的用例可以用 -SkipRealPath 单独摘掉（CI 上更窄的口子，不是默认）。
#>
[CmdletBinding()]
param(
    # 失败时保留现场，方便手工看生成的文件。
    [switch] $Keep,

    # 只跳过 Test-UserPathIsManagedByDefault——全套里唯一碰 HKCU\Environment\Path 的用例。
    # 默认照跑：其余用例都带 -WithoutPath，「默认会写 PATH」这条行为只有它能验。
    [switch] $SkipRealPath
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
$SyncHistoryPreviewTool = 'p4delta Sync to changelist (preview)'
$SyncHistoryApplyTool = 'p4delta Sync to changelist (APPLY - irreversible)'
$SyncFolderPreviewTool = 'p4delta Sync this folder to changelist (preview)'
$SyncFolderApplyTool = 'p4delta Sync this folder to changelist (APPLY - irreversible)'
$IrreversibleFolder = 'p4delta (irreversible)'
$OtherTool = '别人的工具 & 中文'

# prompt 文本是契约的一部分（P4V 用它问用户缺的那半个参数），与 Arguments 一样在这里另写
# 一份：测试要是从 install.ps1 里读，改坏了也测不出来。
$FolderPromptText = '要同步哪个目录？从 History 视图的路径栏复制（本地路径或 depot 路径都行）：'
$ChangelistPromptText = '要同步到哪个 changelist？填你在 History 里看到的那个号：'


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

function Get-InstallerArguments([string[]] $ExtraArguments = @(), [switch] $WithPath) {
    # -NoP4Check 与 -Force 都是**消除机器状态**，不是被测对象：这几条用例测的是写文件的
    # 行为。前者跳过 p4 可用性检查；后者跳过「P4V 正在运行」那道守卫——开发机上 P4V 常驻
    # 是常态，而那道守卫的判据（Get-Process p4v）在别处没有用例覆盖，不该让它把整套用例
    # 拦在门外。
    #
    # -WithoutPath 同理：install.ps1 现在**默认**会往这台机器的用户级 PATH 里写东西，而这
    # 些用例测的是写文件——不能让整套用例污染开发机（多数用例装完并不卸载）。真注册表那条
    # 用例显式用 -WithPath 打开，于是「谁碰了真注册表」在代码里一眼可数。
    #
    # 注意：**别**把 -WithoutPath 写进 $ExtraArguments——重复指定同一个开关，PowerShell 会
    # 直接报参数绑定错误。
    $arguments = @(
        '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $DistDir 'install.ps1'),
        '-InstallDir', $InstallDir, '-CustomToolsPath', $ToolsPath, '-NoP4Check', '-Force', '-Quiet'
    ) + $ExtraArguments
    if (-not $WithPath) {
        $arguments += '-WithoutPath'
    }
    return $arguments
}

function Invoke-Installer([string[]] $ExtraArguments = @(), [switch] $WithPath) {
    $arguments = Get-InstallerArguments $ExtraArguments -WithPath:$WithPath

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

function Assert-OurTool($doc, [string] $Name, [string] $Arguments, [string] $PromptText = '') {
    $tool = Get-Tool $doc $Name
    Assert-True ($null -ne $tool) "应当注册 $Name"
    Assert-Equal (Join-Path $InstallDir 'p4delta.exe') (Get-ChildText $tool 'Definition/Command') "$Name 的 Command 应当是安装路径"
    Assert-Equal $Arguments (Get-ChildText $tool 'Definition/Arguments') "$Name 的 Arguments"
    Assert-Equal '$r' (Get-ChildText $tool 'Definition/InitDir') "$Name 的 Start in 应当是 `$r"
    Assert-Equal 'false' (Get-ChildText $tool 'Console/CloseOnExit') "$Name 不该勾 Close window upon completion"
    Assert-Equal 'true' (Get-ChildText $tool 'AddToContext') "$Name 该进右键菜单"
    Assert-Equal 'true' (Get-ChildText $tool 'Refresh') "$Name 该刷新 P4V"
    if ($PromptText) {
        # 有 Prompt 块 = 勾上 "Prompt for arguments"：$D 得有人填，没这个块参数就是空的。
        Assert-Equal $PromptText (Get-ChildText $tool 'Prompt/PromptText') "$Name 的 PromptText"
        # 浏览按钮（`<ShowBrowse>`）不给：2026-10-03 实测它打开的是**文件**选择器、选不了目录，
        # 而两条 sync 入口问的都是目录。顺带把「没写它」钉住，防止哪天又被顺手加回来。
        Assert-True ($null -eq $tool.SelectSingleNode('Prompt/ShowBrowse')) "$Name 不该勾 Add file browser to prompt dialog"
    } else {
        Assert-True ($null -eq $tool.SelectSingleNode('Prompt')) "$Name 不该勾 Prompt for arguments"
    }
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
    Assert-Equal 7 $doc.SelectNodes('//CustomToolDef').Count '默认应当注册七个工具'
    Assert-Equal 'customtooldeflist' $doc.DocumentElement.GetAttribute('varName') '根元素属性'
    Assert-OurTool $doc $ReconcileTool '-a -w $c -l %D'
    Assert-OurTool $doc $CleanPreviewTool '--clean -w $c -l %D'
    Assert-OurTool $doc $CleanApplyTool '-a --clean -w $c -l %D'
    # sync 两条入口各自动一半、手补一半：History 那条自动 changelist（%S），prompt 问目录；
    # 工作区树那条自动目录（%D），prompt 问 changelist。
    Assert-OurTool $doc $SyncHistoryPreviewTool '--sync --force -w $c -l $D --to %S' $FolderPromptText
    Assert-OurTool $doc $SyncHistoryApplyTool '-a --sync --force -w $c -l $D --to %S' $FolderPromptText
    Assert-OurTool $doc $SyncFolderPreviewTool '--sync --force -w $c -l %D --to $D' $ChangelistPromptText
    Assert-OurTool $doc $SyncFolderApplyTool '-a --sync --force -w $c -l %D --to $D' $ChangelistPromptText

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
    Assert-Equal 8 $doc.SelectNodes('//CustomToolDef').Count '别人的工具加上我们的七个'
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
    Assert-Equal 7 $doc.SelectNodes('//CustomToolDef').Count '改回来时不该多出节点'
    Assert-OurTool $doc $ReconcileTool '-a -w $c -l %D'
    Assert-OurTool $doc $CleanApplyTool '-a --clean -w $c -l %D'
    Assert-Equal 1 $doc.SelectNodes('//CustomToolFolder').Count '子菜单不该被重复创建'
    $inFolder = $doc.SelectSingleNode(
        "//CustomToolFolder[Name='$IrreversibleFolder']//CustomToolDef[Definition/Name='$CleanApplyTool']")
    Assert-True ($null -ne $inFolder) '被挪出去的 APPLY 应当被放回子菜单里'
}

function Test-IrreversibleToolsUseASubmenu {
    Reset-Root
    Invoke-Installer

    $doc = Read-ToolsDocument
    Assert-Equal 7 $doc.SelectNodes('//CustomToolDef').Count '默认七个工具'
    Assert-OurTool $doc $CleanApplyTool '-a --clean -w $c -l %D'
    Assert-OurTool $doc $SyncHistoryApplyTool '-a --sync --force -w $c -l $D --to %S' $FolderPromptText
    Assert-OurTool $doc $SyncFolderApplyTool '-a --sync --force -w $c -l %D --to $D' $ChangelistPromptText

    # 三条不可逆的共用一个子菜单，四条安全的都留在顶层。
    $folders = $doc.SelectNodes('//CustomToolFolder')
    Assert-Equal 1 $folders.Count '不可逆的那三条应当共用一个子菜单'
    Assert-Equal $IrreversibleFolder (Get-ChildText $folders[0] 'Name') '子菜单名'
    foreach ($name in @($CleanApplyTool, $SyncHistoryApplyTool, $SyncFolderApplyTool)) {
        $inFolder = $folders[0].SelectSingleNode(".//CustomToolDef[Definition/Name='$name']")
        Assert-True ($null -ne $inFolder) "$name 应当在这个子菜单里"
    }
    foreach ($name in @($ReconcileTool, $CleanPreviewTool, $SyncHistoryPreviewTool, $SyncFolderPreviewTool)) {
        $tool = Get-Tool $doc $name
        Assert-True ($null -eq $tool.SelectSingleNode('ancestor::CustomToolFolder')) "$name 不该被塞进子菜单"
    }
}

# sync 的两条入口各自动一半、手补一半。这不是偷懒，是 P4V 的两条实测限制逼出来的：
#   1. 一个工具定义里**只能有一个 `%` 参数**——`%D %S` 同用会当场弹
#      "More than one replaceable file argument of type %X is not allowed"。
#   2. 「文件夹的历史」里 `%D` 是空的（变量取不到值时 P4V 干脆不显示带它的工具），
#      所以 History 视图那条拿不到目录，只能 prompt 手填。
#
# `%S`（Selected submitted changelists）只对**已提交**的 changelist 有值，History 那条因此
# 在 Pending 视图里不触发。换成 `%c` 就漏了：它在 Pending 视图同样有值，会把一个 pending 号
# 喂给 `--to`，而 changelist 号是创建时分配的——pending 号完全可能小于 head，那一下就是
# 「退回历史版本并删掉之后新建的文件」。
#
# 这条用例把这些选择钉住，免得日后被「统一成 %D / %c」或者「范围改回 $r」顺手改掉。
function Test-SyncToolsWireUpBothEntryPoints {
    Reset-Root
    Invoke-Installer

    $doc = Read-ToolsDocument

    # History 视图那条：changelist 自动，目录由 prompt 补。
    foreach ($name in @($SyncHistoryPreviewTool, $SyncHistoryApplyTool)) {
        $arguments = Get-ChildText (Get-Tool $doc $name) 'Definition/Arguments'
        Assert-True ($arguments -clike '*--to %S*') "$name 应当用 %S 取已提交的 changelist"
        Assert-True ($arguments -clike '*-l $D*') "$name 应当把目录交给 prompt 的 `$D"
        Assert-True ($arguments -cnotlike '*%D*') "$name 不能带 %D：一个工具只允许一个 % 参数"
        Assert-True ($arguments -cnotlike '*%c*') "$name 不该用 %c：它在 Pending 视图里也有值"
        Assert-True ($arguments -cnotlike '*-l $r*') "$name 不该同步整个工作区：范围要落在 prompt 给的那个目录上"
        Assert-Equal $FolderPromptText (Get-ChildText (Get-Tool $doc $name) 'Prompt/PromptText') "$name 的 prompt 该问目录"
    }

    # 工作区树那条：目录自动（右键选中项），changelist 由 prompt 补。
    foreach ($name in @($SyncFolderPreviewTool, $SyncFolderApplyTool)) {
        $arguments = Get-ChildText (Get-Tool $doc $name) 'Definition/Arguments'
        Assert-True ($arguments -clike '*-l %D*') "$name 应当同步右键选中的那个目录"
        Assert-True ($arguments -clike '*--to $D*') "$name 应当把 changelist 交给 prompt 的 `$D"
        Assert-True ($arguments -cnotlike '*%S*') "$name 不能带 %S：一个工具只允许一个 % 参数"
        Assert-True ($arguments -cnotlike '*%c*') "$name 不该用 %c：它在 Pending 视图里也有值"
        Assert-Equal $ChangelistPromptText (Get-ChildText (Get-Tool $doc $name) 'Prompt/PromptText') "$name 的 prompt 该问 changelist"
    }

    # 预演与实际执行只差一个 -a，别的部分一字不差——两边的行为才是同一件事。
    foreach ($pair in @(
            @($SyncHistoryPreviewTool, $SyncHistoryApplyTool),
            @($SyncFolderPreviewTool, $SyncFolderApplyTool)
        )) {
        $preview = Get-ChildText (Get-Tool $doc $pair[0]) 'Definition/Arguments'
        $apply = Get-ChildText (Get-Tool $doc $pair[1]) 'Definition/Arguments'
        Assert-Equal "-a $preview" $apply "$($pair[1]) 应当就是 $($pair[0]) 加一个 -a"
    }
}

function Test-WithoutCleanApplySkipsCleanApply {
    Reset-Root
    Invoke-Installer @('-WithoutCleanApply')

    $doc = Read-ToolsDocument
    Assert-Equal 6 $doc.SelectNodes('//CustomToolDef').Count '-WithoutCleanApply 时只有六个工具'
    Assert-OurTool $doc $ReconcileTool '-a -w $c -l %D'
    Assert-OurTool $doc $CleanPreviewTool '--clean -w $c -l %D'
    Assert-OurTool $doc $SyncHistoryPreviewTool '--sync --force -w $c -l $D --to %S' $FolderPromptText
    Assert-OurTool $doc $SyncFolderPreviewTool '--sync --force -w $c -l %D --to $D' $ChangelistPromptText
    Assert-True ($null -eq (Get-Tool $doc $CleanApplyTool)) '不可逆的 clean 不该注册'
    # 子菜单留着：sync 那两条不可逆的还在里面。clean 的退出口不该把同住一个子菜单的邻居带走。
    Assert-Equal 1 $doc.SelectNodes('//CustomToolFolder').Count '装 sync 那两条的子菜单还在'
    Assert-True ($null -ne (Get-Tool $doc $SyncHistoryApplyTool)) 'sync 那两条不受 -WithoutCleanApply 影响'
    Assert-True ($null -ne (Get-Tool $doc $SyncFolderApplyTool)) 'sync 那两条不受 -WithoutCleanApply 影响'
}

function Test-WithoutCleanApplyRemovesRegisteredCleanApply {
    Reset-Root
    Write-Utf8NoBom $ToolsPath $OtherToolXml
    Invoke-Installer
    Assert-Equal 8 (Read-ToolsDocument).SelectNodes('//CustomToolDef').Count '先按默认装齐：别人的一个加我们的七个'

    # -WhatIf：移除只发生在内存里，文件一字不动，也不该产生备份。
    $before = [System.IO.File]::ReadAllBytes($ToolsPath)
    $backupsBefore = Get-BackupCount
    Invoke-Installer @('-WithoutCleanApply', '-WhatIf')
    Assert-BytesEqual $before ([System.IO.File]::ReadAllBytes($ToolsPath)) '-WhatIf 改动了文件'
    Assert-Equal $backupsBefore (Get-BackupCount) '-WhatIf 不该产生备份'

    # 真摘：APPLY 没了，别人的工具与安全的四条原样。子菜单留着——sync 那两条不可逆的还在
    # 里面，只有整个子菜单空了才该清掉（下一个用例覆盖那个分支）。
    # 备份文件名只精确到秒，先跨过一秒——否则这次的备份会覆盖掉上面安装时那份，
    # 「多出一份备份」的断言就会假失败（pwsh 跑得快时真的撞上过）。
    Start-Sleep -Seconds 1
    Invoke-Installer @('-WithoutCleanApply')
    $doc = Read-ToolsDocument
    Assert-Equal 7 $doc.SelectNodes('//CustomToolDef').Count '别人的一个加我们的六个'
    Assert-True ($null -eq (Get-Tool $doc $CleanApplyTool)) 'APPLY 条目应当被摘掉'
    Assert-Equal 1 $doc.SelectNodes('//CustomToolFolder').Count 'sync 那两条还在，子菜单就该留着'
    Assert-OtherToolIntact $doc
    Assert-OurTool $doc $ReconcileTool '-a -w $c -l %D'
    Assert-OurTool $doc $CleanPreviewTool '--clean -w $c -l %D'
    Assert-OurTool $doc $SyncHistoryApplyTool '-a --sync --force -w $c -l $D --to %S' $FolderPromptText
    Assert-OurTool $doc $SyncFolderApplyTool '-a --sync --force -w $c -l %D --to $D' $ChangelistPromptText
    Assert-Equal ($backupsBefore + 1) (Get-BackupCount) '真摘掉了就该走一次备份 + 保存'

    # 退出口自身幂等：再跑一次，字节不变、不多出备份。跨过一秒的理由同上一条用例。
    $before = [System.IO.File]::ReadAllBytes($ToolsPath)
    Start-Sleep -Seconds 1
    Invoke-Installer @('-WithoutCleanApply')
    Assert-BytesEqual $before ([System.IO.File]::ReadAllBytes($ToolsPath)) '第二次退出口改动了文件'
    Assert-Equal ($backupsBefore + 1) (Get-BackupCount) '没变化就不该产生新的备份'
}

function Test-IrreversibleFolderKeepsForeignTools {
    Reset-Root
    Invoke-Installer

    # 用户往我们的子菜单里塞了自己的工具：退出口只摘我们的（这里三条不可逆的都摘），
    # 子菜单得留着。
    $doc = Read-ToolsDocument
    $folderList = $doc.SelectSingleNode("//CustomToolFolder[Name='$IrreversibleFolder']/CustomToolDefList")
    Assert-True ($null -ne $folderList) '默认安装应当建出子菜单'
    $otherDoc = New-Object System.Xml.XmlDocument
    $otherDoc.LoadXml($OtherToolXml)
    [void]$folderList.AppendChild($doc.ImportNode($otherDoc.SelectSingleNode('//CustomToolDef'), $true))
    $doc.Save($ToolsPath)

    Invoke-Installer @('-WithoutCleanApply', '-WithoutSyncApply')

    $doc = Read-ToolsDocument
    Assert-True ($null -eq (Get-Tool $doc $CleanApplyTool)) 'APPLY 仍然要被摘掉'
    Assert-True ($null -eq (Get-Tool $doc $SyncHistoryApplyTool)) 'sync 那两条也要被摘掉'
    Assert-True ($null -eq (Get-Tool $doc $SyncFolderApplyTool)) 'sync 那两条也要被摘掉'
    Assert-True ($null -ne (Get-Tool $doc $OtherTool)) '塞进去的工具不该被动'
    Assert-Equal 5 $doc.SelectNodes('//CustomToolDef').Count '我们的四条加塞进来的一条'
    Assert-Equal 1 $doc.SelectNodes('//CustomToolFolder').Count '子菜单里还有别人的工具，就该留着'
}

function Test-WithoutSyncApplySkipsSyncApply {
    Reset-Root
    Invoke-Installer @('-WithoutSyncApply')

    $doc = Read-ToolsDocument
    Assert-Equal 5 $doc.SelectNodes('//CustomToolDef').Count '-WithoutSyncApply 时只有五个工具'
    Assert-OurTool $doc $ReconcileTool '-a -w $c -l %D'
    Assert-OurTool $doc $CleanPreviewTool '--clean -w $c -l %D'
    Assert-OurTool $doc $SyncHistoryPreviewTool '--sync --force -w $c -l $D --to %S' $FolderPromptText
    Assert-OurTool $doc $SyncFolderPreviewTool '--sync --force -w $c -l %D --to $D' $ChangelistPromptText
    Assert-True ($null -eq (Get-Tool $doc $SyncHistoryApplyTool)) '不可逆的 sync 不该注册'
    Assert-True ($null -eq (Get-Tool $doc $SyncFolderApplyTool)) '不可逆的 sync 不该注册'
    # 子菜单留着：clean 那条不可逆的还在里面。sync 的退出口不该把邻居带走。
    Assert-Equal 1 $doc.SelectNodes('//CustomToolFolder').Count '装 clean 那条的子菜单还在'
    Assert-True ($null -ne (Get-Tool $doc $CleanApplyTool)) 'clean 那条不受 -WithoutSyncApply 影响'
}

function Test-BothWithoutSwitchesClearTheFolder {
    # 全新机器上给两个开关：子菜单一次都不该被建出来。
    Reset-Root
    Invoke-Installer @('-WithoutCleanApply', '-WithoutSyncApply')
    Assert-Equal 0 (Read-ToolsDocument).SelectNodes('//CustomToolFolder').Count '不该凭空造出子菜单'

    # 先按默认装齐、再三条一起摘：空掉的子菜单这时才该清掉（只摘一条时都要留着，
    # 那几条分支在上面各自的用例里）。
    Reset-Root
    Invoke-Installer
    Assert-Equal 1 (Read-ToolsDocument).SelectNodes('//CustomToolFolder').Count '默认安装应当建出子菜单'

    Invoke-Installer @('-WithoutCleanApply', '-WithoutSyncApply')

    $doc = Read-ToolsDocument
    Assert-Equal 4 $doc.SelectNodes('//CustomToolDef').Count '三条都摘掉后只剩四条安全的'
    Assert-OurTool $doc $SyncHistoryPreviewTool '--sync --force -w $c -l $D --to %S' $FolderPromptText
    Assert-OurTool $doc $SyncFolderPreviewTool '--sync --force -w $c -l %D --to $D' $ChangelistPromptText
    Assert-Equal 0 $doc.SelectNodes('//CustomToolFolder').Count '三条都摘掉后空掉的子菜单应当被清掉'
}

function Test-Uninstall {
    Reset-Root
    Write-Utf8NoBom $ToolsPath $OtherToolXml
    Invoke-Installer
    Assert-Equal 8 (Read-ToolsDocument).SelectNodes('//CustomToolDef').Count '装完应当是他们的一个加我们的七个'

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

# ---- 真注册表：HKCU\Environment\Path ----

# 全套里唯一碰真实用户配置的用例：先快照、finally 还原（断言失败、被 Ctrl+C 打断也会还回去）。
# 中途被强杀会在 PATH 里留一条带旧 PID 的临时目录，不影响下次运行——判据是当前 PID 的目录。

function Get-RealUserPathSnapshot {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $false)
    if (-not $key) { return @{ Exists = $false; Value = $null; Kind = $null } }
    try {
        $raw = $key.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        if ($null -eq $raw) { return @{ Exists = $false; Value = $null; Kind = $null } }
        return @{ Exists = $true; Value = [string] $raw; Kind = $key.GetValueKind('Path') }
    } finally {
        $key.Dispose()
    }
}

function Restore-RealUserPathSnapshot($Snapshot) {
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
    try {
        if ($Snapshot.Exists) {
            $key.SetValue('Path', $Snapshot.Value, $Snapshot.Kind)
        } else {
            $key.DeleteValue('Path', $false)
        }
    } finally {
        $key.Dispose()
    }
}

# 与 install.ps1 的 Test-PathEntry 同一套判据：大小写不敏感、忽略尾反斜杠。刻意另写一份
# （同上面 prompt 文本的处理）：测试要是从 install.ps1 里读，改坏了也测不出来。
function Test-PathValueHasEntry([string] $PathValue, [string] $Dir) {
    foreach ($entry in @($PathValue -split ';')) {
        if ($entry -and $entry.TrimEnd('\') -ieq $Dir.TrimEnd('\')) {
            return $true
        }
    }
    return $false
}

function Test-UserPathIsManagedByDefault {
    $snapshot = Get-RealUserPathSnapshot
    try {
        # 1) -WithoutPath 而条目本来就不在：注册表逐字符不动（用 -ceq：-ne 不分大小写）
        Reset-Root
        Invoke-Installer
        Assert-True ($snapshot.Value -ceq (Get-RealUserPathSnapshot).Value) '-WithoutPath 改动了注册表里的 PATH'

        # 2) 默认安装 + -WhatIf：只说不做。这里必须 -WithPath，否则走不到 PATH 那条分支。
        #    （上一句是真装，铺出了安装目录，先重置现场再验「不该创建」。）
        Reset-Root
        Invoke-Installer @('-WhatIf') -WithPath
        Assert-True (-not (Test-Path -LiteralPath $InstallDir)) '-WhatIf 不该创建安装目录'
        Assert-True ($snapshot.Value -ceq (Get-RealUserPathSnapshot).Value) '-WhatIf 改动了注册表里的 PATH'

        # 3) 默认安装：条目出现、别人的条目一条不丢、值类型不变。顺带抓子进程输出断言广播没
        #    报错——那段 C# 没有任何编译期校验，只有这条能拦住「装完静默退化」。匹配 ASCII 的
        #    WM_SETTINGCHANGE 而不是中文：CI 上中文过管道会被打散，那样断言等于永远通过。
        $output = (& $HostExe @(Get-InstallerArguments -WithPath) 2>&1 | Out-String)
        Assert-Equal 0 $LASTEXITCODE "默认安装该成功：$output"
        Assert-True ($output -notlike '*WM_SETTINGCHANGE*') "安装不该在广播上报错：$output"

        $after = Get-RealUserPathSnapshot
        Assert-True (Test-PathValueHasEntry $after.Value $InstallDir) '默认安装应当把安装目录写进用户级 PATH'
        foreach ($original in @($snapshot.Value -split ';' | Where-Object { $_ -ne '' })) {
            Assert-True (Test-PathValueHasEntry $after.Value $original) "原有的 PATH 条目不该丢：$original"
        }
        if ($snapshot.Exists) {
            Assert-Equal $snapshot.Kind $after.Kind 'PATH 的值类型不该被改动'
        }

        # 4) 幂等：已经有那条就不重写
        Invoke-Installer -WithPath
        Assert-True ($after.Value -ceq (Get-RealUserPathSnapshot).Value) 'PATH 里已有那条时不该重写'

        # 5) 卸载总会摘掉自己那条，别人的原样留着
        Invoke-Installer @('-Uninstall') -WithPath
        Assert-True (-not (Test-Path -LiteralPath $InstallDir)) '卸载该删掉安装目录'
        $final = Get-RealUserPathSnapshot
        Assert-True (-not (Test-PathValueHasEntry $final.Value $InstallDir)) '卸载应当把安装目录从 PATH 里摘掉'
        foreach ($original in @($snapshot.Value -split ';' | Where-Object { $_ -ne '' })) {
            Assert-True (Test-PathValueHasEntry $final.Value $original) "卸载不该带走别人的 PATH 条目：$original"
        }

        # 6) -WithoutPath 的「确保不在」：已经在里面就摘掉
        Invoke-Installer -WithPath
        Assert-True (Test-PathValueHasEntry (Get-RealUserPathSnapshot).Value $InstallDir) '先决条件：这次安装该把条目加回去'
        Invoke-Installer
        Assert-True (-not (Test-PathValueHasEntry (Get-RealUserPathSnapshot).Value $InstallDir)) '-WithoutPath 应当把已有的那条摘掉'
    } finally {
        Restore-RealUserPathSnapshot $snapshot
    }
}

$cases = @(
    'Test-InstallLayoutIsSelfContained',
    'Test-FreshInstall',
    'Test-ExePathInstallsThatExe',
    'Test-MissingExePathFails',
    'Test-KeepsForeignToolsAndIsIdempotent',
    'Test-UpdatesATamperedDefinition',
    'Test-IrreversibleToolsUseASubmenu',
    'Test-SyncToolsWireUpBothEntryPoints',
    'Test-WithoutCleanApplySkipsCleanApply',
    'Test-WithoutCleanApplyRemovesRegisteredCleanApply',
    'Test-WithoutSyncApplySkipsSyncApply',
    'Test-BothWithoutSwitchesClearTheFolder',
    'Test-IrreversibleFolderKeepsForeignTools',
    'Test-Uninstall',
    'Test-WhatIfChangesNothing'
)

# 唯一碰真注册表的用例，放最后：万一进程被强杀没走到 finally，也不带偏前面的用例。
# （用 -SkipRealPath 时整条不进列表，`$cases` 里就没有它。）
if (-not $SkipRealPath) {
    $cases += 'Test-UserPathIsManagedByDefault'
}

$failed = 0
Write-Host "install.ps1 测试（$([System.IO.Path]::GetFileName($HostExe))）"
if ($SkipRealPath) {
    Write-Host '  skip Test-UserPathIsManagedByDefault（-SkipRealPath：这条要动真实用户 PATH）'
}

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
