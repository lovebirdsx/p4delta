#Requires -Version 5.1
<#
.SYNOPSIS
    把 p4delta 注册成 P4V 的自定义工具。

.DESCRIPTION
    两件事：把 p4delta.exe 铺到安装目录，再把工具定义写进 P4V 的自定义工具文件
    （默认 %USERPROFILE%\.p4qt\customtools.xml），用户不用碰 Manage Tools 对话框。

    写入是幂等的：只动自己那几个节点，用户已有的其它自定义工具原样保留；内容没变时
    整个文件都不重写。写之前会备份一份。

    装完要重启 P4V —— 它只在启动时读那个文件。

.PARAMETER InstallDir
    exe 的安装目录，默认 %LOCALAPPDATA%\Programs\p4delta（per-user，不需要管理员）。

.PARAMETER CustomToolsPath
    P4V 的自定义工具文件，默认 %USERPROFILE%\.p4qt\customtools.xml。

.PARAMETER ExePath
    要安装的 p4delta.exe。默认取与本脚本同目录的那一份（release 包解压出来就是这个布局）。
    自编译的产物用它可以指到别处，例如 target\release\p4delta.exe；scripts\install-local.ps1
    走的就是这条路。

.PARAMETER WithoutCleanApply
    不注册「clean 实际清理」。它**不可逆**（删 depot 里没有的文件、丢弃未打开文件的
    本地改动），默认会注册，但放在单独的子菜单里，免得和安全的那个挨着被误点；
    用这个开关时，以前注册过的会被摘掉。

.PARAMETER Uninstall
    卸载：摘掉工具定义、清掉安装目录。不会动摘要缓存（%LOCALAPPDATA%\FastReconcile）。

.PARAMETER Force
    P4V 正在运行时也照样写。默认中止——P4V 退出时可能用它内存里的工具列表覆盖这次写入。

.PARAMETER AddToPath
    把安装目录加进用户级 PATH，方便在命令行里直接敲 p4delta。默认不加：P4V 那边用的是
    绝对路径，PATH 只对命令行用户有意义。

.PARAMETER NoP4Check
    跳过 p4 可用性检查（CI 与自动化测试用）。

.PARAMETER Quiet
    只输出警告与错误。

.EXAMPLE
    .\install.ps1
    装到默认位置，注册三个工具（reconcile + clean 预演 + clean 实际清理）。

.EXAMPLE
    .\install.ps1 -WithoutCleanApply
    不注册不可逆的「clean 实际清理」；以前注册过的会被摘掉。

.EXAMPLE
    .\install.ps1 -Uninstall
    卸载。
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string] $InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\p4delta'),
    [string] $CustomToolsPath = (Join-Path $env:USERPROFILE '.p4qt\customtools.xml'),
    [string] $ExePath,
    [switch] $WithoutCleanApply,
    [switch] $Uninstall,
    [switch] $Force,
    [switch] $AddToPath,
    [switch] $NoP4Check,
    [switch] $Quiet
)

$ErrorActionPreference = 'Stop'

$ExeName = 'p4delta.exe'
$ScriptName = 'install.ps1'

# 工具的显示名。它们是幂等写入的**识别键**：改了名字，下次安装会变成「新增」而不是
# 「更新」，旧节点会留在菜单里；删除（-WithoutCleanApply、卸载）也按这套名字找节点。
# 全部用 ASCII，免得菜单字体出意外。
$ReconcileTool = 'p4delta Reconcile'
$CleanPreviewTool = 'p4delta Clean (preview)'
$CleanApplyTool = 'p4delta Clean (APPLY - irreversible)'
$CleanApplyFolder = 'p4delta (irreversible)'
$OurToolNames = @($ReconcileTool, $CleanPreviewTool, $CleanApplyTool)

function Write-Info([string] $Message) {
    if (-not $Quiet) {
        Write-Host $Message
    }
}

# ---- P4V 的自定义工具文件 ----

function New-CustomToolsDocument {
    $doc = New-Object System.Xml.XmlDocument
    [void]$doc.AppendChild($doc.CreateXmlDeclaration('1.0', 'UTF-8', $null))
    # P4V 自己导出的文件里就有这一行，照抄。
    [void]$doc.AppendChild($doc.CreateComment('perforce-xml-version=1.0'))
    $root = $doc.CreateElement('CustomToolDefList')
    $root.SetAttribute('varName', 'customtooldeflist')
    [void]$doc.AppendChild($root)
    return $doc
}

function Read-CustomToolsDocument([string] $Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        Write-Info "自定义工具文件还不存在，将新建：$Path"
        return New-CustomToolsDocument
    }

    $doc = New-Object System.Xml.XmlDocument
    $doc.Load($Path)
    if (-not $doc.DocumentElement -or $doc.DocumentElement.LocalName -ne 'CustomToolDefList') {
        throw "$Path 的根元素不是 CustomToolDefList，格式不认识，不敢改。"
    }
    return $doc
}

function Save-CustomToolsDocument($doc, [string] $Path) {
    $dir = Split-Path -Parent $Path
    if ($dir -and -not (Test-Path -LiteralPath $dir)) {
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
    }

    $settings = New-Object System.Xml.XmlWriterSettings
    $settings.Indent = $true
    # 不写 BOM：P4V 自己导出的文件就没有，保持字节形态一致。
    $settings.Encoding = New-Object System.Text.UTF8Encoding($false)

    $writer = [System.Xml.XmlWriter]::Create($Path, $settings)
    try {
        $doc.Save($writer)
    } finally {
        $writer.Dispose()
    }
}

function Backup-CustomToolsFile([string] $Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $null
    }
    $backup = "$Path.p4delta-backup-$(Get-Date -Format 'yyyyMMdd-HHmmss')"
    Copy-Item -LiteralPath $Path -Destination $backup -Force
    return $backup
}

function Get-ChildText($Node, [string] $Path) {
    $child = $Node.SelectSingleNode($Path)
    if ($null -eq $child) {
        return $null
    }
    return $child.InnerText
}

function Find-ToolByName($doc, [string] $Name) {
    foreach ($tool in $doc.SelectNodes('//CustomToolDef')) {
        if ((Get-ChildText $tool 'Definition/Name') -eq $Name) {
            return $tool
        }
    }
    return $null
}

# 工具所在的子菜单名；不在任何子菜单里时返回 $null。
function Get-ContainingFolderName($Node) {
    $parent = $Node.ParentNode
    while ($null -ne $parent -and $parent.NodeType -eq [System.Xml.XmlNodeType]::Element) {
        if ($parent.LocalName -eq 'CustomToolFolder') {
            $name = $parent.SelectSingleNode('Name')
            if ($name) {
                return $name.InnerText
            }
            return ''
        }
        $parent = $parent.ParentNode
    }
    return $null
}

# 取子菜单节点；不存在就建一个（只在真要往里放东西时调用，免得白改文件）。
function Get-OrCreateFolder($doc, [string] $Name) {
    foreach ($folder in $doc.SelectNodes('//CustomToolFolder')) {
        $nameNode = $folder.SelectSingleNode('Name')
        if ($nameNode -and $nameNode.InnerText -eq $Name) {
            $list = $folder.SelectSingleNode('CustomToolDefList')
            if ($list) {
                return $list
            }
        }
    }

    $new = $doc.CreateElement('CustomToolFolder')
    $nameElement = $doc.CreateElement('Name')
    $nameElement.InnerText = $Name
    [void]$new.AppendChild($nameElement)
    $list = $doc.CreateElement('CustomToolDefList')
    [void]$new.AppendChild($list)
    [void]$doc.DocumentElement.AppendChild($new)
    return $list
}

# 造一个工具定义节点。
#
# 两个已知的缺口，都**没有臆造元素名**：README 表格里的「Run tool in terminal window」与
# 「Ignore P4CONFIG files」在这里没有对应元素——它们在 P4V 的工具 XML 里叫什么名字，官方
# 文档没写、也没实测过。前者不勾时输出进 P4V 的输出窗格，工具照常可用。要补齐的话：
# 在 GUI 里手工勾上这两项 → Export tools... → 与这里生成的文件对比，把新出现的元素名加进来。
function New-ToolElement($doc, [string] $Name, [string] $Arguments, [string] $ExePath) {
    $tool = $doc.CreateElement('CustomToolDef')

    $definition = $doc.CreateElement('Definition')
    foreach ($pair in @(
            @('Name', $Name),
            @('Command', $ExePath),
            @('Arguments', $Arguments),
            @('Shortcut', '')
        )) {
        $element = $doc.CreateElement($pair[0])
        $element.InnerText = $pair[1]
        [void]$definition.AppendChild($element)
    }
    # Start in = 工作区根目录（P4V 展开 $r）。相对路径的参数与 .p4config 的发现都靠它。
    $initDir = $doc.CreateElement('InitDir')
    $initDir.InnerText = '$r'
    [void]$definition.AppendChild($initDir)
    [void]$tool.AppendChild($definition)

    # 不勾 "Close window upon completion"：留着窗口才能看到输出。
    # 不写 <Prompt> 块 = 不勾 "Prompt for arguments"。
    $console = $doc.CreateElement('Console')
    $closeOnExit = $doc.CreateElement('CloseOnExit')
    $closeOnExit.InnerText = 'false'
    [void]$console.AppendChild($closeOnExit)
    [void]$tool.AppendChild($console)

    # 勾 "Add to applicable context menus" 与 "Refresh Helix P4V upon completion"。
    foreach ($flag in @('AddToContext', 'Refresh')) {
        $element = $doc.CreateElement($flag)
        $element.InnerText = 'true'
        [void]$tool.AppendChild($element)
    }

    return $tool
}

# 空白文本节点不算子节点。`InnerText = ''` 会造出一个空的 text 节点（比如 `<Shortcut>`），
# 而同样内容的元素从文件里解析出来是零个子节点——两者序列化结果一模一样，比较时不能当成
# 差异，否则每次运行都判成「变了」并重写一遍，幂等性就没了。
function Test-IsIgnorableText($Node) {
    $type = $Node.NodeType
    if ($type -eq [System.Xml.XmlNodeType]::Text -or
        $type -eq [System.Xml.XmlNodeType]::Whitespace -or
        $type -eq [System.Xml.XmlNodeType]::SignificantWhitespace) {
        return ($Node.Value.Trim() -eq '')
    }
    return $false
}

# 比较两个节点是否等价，忽略缩进：谁写的空白、写成什么样都不该算差异。
function Test-SameNode($Expected, $Actual) {
    if ($Expected.NodeType -ne $Actual.NodeType) {
        return $false
    }
    if ($Expected.NodeType -eq [System.Xml.XmlNodeType]::Text) {
        return ($Expected.Value.Trim() -eq $Actual.Value.Trim())
    }
    if ($Expected.LocalName -ne $Actual.LocalName) {
        return $false
    }

    $expectedAttributes = @($Expected.Attributes)
    $actualAttributes = @($Actual.Attributes)
    if ($expectedAttributes.Count -ne $actualAttributes.Count) {
        return $false
    }
    foreach ($attribute in $expectedAttributes) {
        $other = $Actual.Attributes[$attribute.LocalName]
        if ($null -eq $other -or $other.Value -ne $attribute.Value) {
            return $false
        }
    }

    $expectedChildren = @($Expected.ChildNodes | Where-Object { -not (Test-IsIgnorableText $_) })
    $actualChildren = @($Actual.ChildNodes | Where-Object { -not (Test-IsIgnorableText $_) })
    if ($expectedChildren.Count -ne $actualChildren.Count) {
        return $false
    }
    for ($i = 0; $i -lt $expectedChildren.Count; $i++) {
        if (-not (Test-SameNode $expectedChildren[$i] $actualChildren[$i])) {
            return $false
        }
    }
    return $true
}

# 默认注册三条；-WithoutCleanApply 只把不可逆的那条从注册列表里去掉——已经注册过的
# 旧条目由 Remove-CleanApplyTool 在调用点摘掉。
function Get-DesiredTools([switch] $WithoutCleanApply) {
    $specs = @(
        @{ Name = $ReconcileTool; Arguments = '-a -w $c -l %D'; Folder = $null },
        @{ Name = $CleanPreviewTool; Arguments = '--clean -w $c -l %D'; Folder = $null }
    )
    if (-not $WithoutCleanApply) {
        $specs += @{ Name = $CleanApplyTool; Arguments = '-a --clean -w $c -l %D'; Folder = $CleanApplyFolder }
    }
    return $specs
}

function Update-ToolList($doc, $Specs, [string] $ExePath) {
    $changed = $false

    foreach ($spec in $Specs) {
        $existing = Find-ToolByName $doc $spec.Name
        if ($null -ne $existing) {
            $inRightFolder = (Get-ContainingFolderName $existing) -eq $spec.Folder
            $desired = New-ToolElement $doc $spec.Name $spec.Arguments $ExePath
            if ($inRightFolder -and (Test-SameNode $desired $existing)) {
                continue
            }
            [void]$existing.ParentNode.RemoveChild($existing)
        }

        if ($spec.Folder) {
            $parent = Get-OrCreateFolder $doc $spec.Folder
        } else {
            $parent = $doc.DocumentElement
        }
        [void]$parent.AppendChild((New-ToolElement $doc $spec.Name $spec.Arguments $ExePath))
        $changed = $true
    }

    return $changed
}

function Test-PathUnder([string] $Path, [string] $Dir) {
    try {
        $full = [System.IO.Path]::GetFullPath($Path)
        $dirFull = [System.IO.Path]::GetFullPath($Dir).TrimEnd('\') + '\'
        return $full.StartsWith($dirFull, [System.StringComparison]::OrdinalIgnoreCase)
    } catch {
        return $false
    }
}

# 按显示名删工具节点。返回是否真删了。
#
# 只按名字认人是刻意的：三条工具的 Command 完全一样（都指向安装目录里的 exe），
# 拿 Command 区分不出该删哪条。
function Remove-ToolsByName($doc, [string[]] $Names) {
    $changed = $false

    foreach ($tool in @($doc.SelectNodes('//CustomToolDef'))) {
        if ($Names -contains (Get-ChildText $tool 'Definition/Name')) {
            [void]$tool.ParentNode.RemoveChild($tool)
            $changed = $true
        }
    }

    return $changed
}

# -WithoutCleanApply 的退出口：摘掉 APPLY 那条，并清掉因此变空的子菜单目录。
#
# 子菜单只在名字对上、且里面一条工具都不剩时才删：用户往里放了自己的东西就留着，
# 别的空目录也一概不碰——这条路径只该动我们自己的节点。
function Remove-CleanApplyTool($doc) {
    $changed = Remove-ToolsByName $doc @($CleanApplyTool)

    foreach ($folder in @($doc.SelectNodes('//CustomToolFolder'))) {
        if ((Get-ChildText $folder 'Name') -eq $CleanApplyFolder -and
                $folder.SelectNodes('.//CustomToolDef').Count -eq 0) {
            [void]$folder.ParentNode.RemoveChild($folder)
            $changed = $true
        }
    }

    return $changed
}

function Remove-OurTools($doc, [string] $InstallDir) {
    $changed = Remove-ToolsByName $doc $OurToolNames

    # Command 落在安装目录下是兜底，用户改过名字时也能删掉。
    foreach ($tool in @($doc.SelectNodes('//CustomToolDef'))) {
        $command = Get-ChildText $tool 'Definition/Command'
        if ($command -and (Test-PathUnder $command $InstallDir)) {
            [void]$tool.ParentNode.RemoveChild($tool)
            $changed = $true
        }
    }

    foreach ($folder in @($doc.SelectNodes('//CustomToolFolder'))) {
        if ($folder.SelectNodes('.//CustomToolDef').Count -eq 0) {
            [void]$folder.ParentNode.RemoveChild($folder)
            $changed = $true
        }
    }

    return $changed
}

# ---- 环境检查 ----

function Assert-P4vNotRunning {
    $p4v = Get-Process -Name p4v -ErrorAction SilentlyContinue
    if (-not $p4v) {
        return
    }

    if ($Force) {
        Write-Warning 'P4V 正在运行。P4V 退出时可能用它内存里的工具列表覆盖这次写入，装完请到 Manage Tools 里确认一下。'
        return
    }
    throw 'P4V 正在运行，它退出时可能覆盖这次写入。请先关掉 P4V 再跑一次，或者加 -Force 强行继续。'
}

function Find-P4 {
    # 顺序与 p4delta 自己的定位一致：P4_EXE → PATH → P4V 安装目录。
    if ($env:P4_EXE) {
        if (Test-Path -LiteralPath $env:P4_EXE -PathType Leaf) {
            return @{ Path = $env:P4_EXE }
        }
        return @{ Error = "P4_EXE 指向的 $($env:P4_EXE) 不存在。" }
    }

    $onPath = Get-Command p4 -ErrorAction SilentlyContinue
    if ($onPath) {
        return @{ Path = $onPath.Source }
    }

    foreach ($base in @($env:ProgramFiles, ${env:ProgramFiles(x86)})) {
        if (-not $base) { continue }
        foreach ($relative in @('Perforce\DVCS\p4.exe', 'Perforce\p4.exe')) {
            $candidate = Join-Path $base $relative
            if (Test-Path -LiteralPath $candidate -PathType Leaf) {
                return @{ Path = $candidate }
            }
        }
    }

    return $null
}

function Test-P4Availability {
    $found = Find-P4

    if ($found -and $found.Error) {
        Write-Warning $found.Error
        return
    }
    if (-not $found) {
        # P4V 的安装器把命令行客户端作为可选组件，没勾那一项就没有 p4 —— 这是常见且可恢复的
        # 状态，不该拦住安装：p4delta 运行时也会去 P4V 的安装目录找。
        Write-Warning '没找到 p4。要么在 P4V 安装器里补装 Command-Line Client (P4)，要么把 P4_EXE 指向 p4.exe，否则 p4delta 跑不起来。'
        return
    }

    try {
        $version = (& $found.Path -V 2>&1 | Select-Object -First 1)
        Write-Info "p4: $($found.Path)  ($version)"
    } catch {
        Write-Warning "p4 找到了（$($found.Path)）但跑不起来：$($_.Exception.Message)"
    }
}

# ---- 用户级 PATH ----

function Get-UserPathEntry {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $false)
    if (-not $key) { return $null }
    try {
        return [string] $key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    } finally {
        $key.Dispose()
    }
}

function Test-PathEntry($Entries, [string] $Dir) {
    foreach ($entry in $Entries) {
        if ($entry -and $entry.TrimEnd('\') -ieq $Dir.TrimEnd('\')) {
            return $true
        }
    }
    return $false
}

# ---- 主流程 ----

function Invoke-Install {
    $sourceExe = if ($ExePath) { $ExePath } else { Join-Path $PSScriptRoot $ExeName }
    if (-not (Test-Path -LiteralPath $sourceExe -PathType Leaf)) {
        if ($ExePath) {
            throw "-ExePath 指向的 $sourceExe 不存在。"
        }
        throw "没找到 $ExeName，它应当和本脚本在同一个目录（$PSScriptRoot）。请从解压出来的目录里运行，或用 -ExePath 指向别处的那一份。"
    }

    $version = Get-ExeVersion $sourceExe
    if ($version) {
        Write-Info "p4delta $version"
    }

    Assert-P4vNotRunning

    $targetExe = [System.IO.Path]::GetFullPath((Join-Path $InstallDir $ExeName))
    # Application 字段不展开环境变量，只能写死绝对路径。路径里带空格时能否直接用还没实测过，
    # 先按所有真实导出文件的写法（不加引号）写，同时把话说明白。
    if ($targetExe -match '\s') {
        Write-Warning "安装路径里有空格：$targetExe`nP4V 的 Application 字段能否直接接受带空格的路径尚未验证。装完后请先跑一次确认；不行就用 -InstallDir 换一个不含空格的目录重装。"
    }

    if ($PSCmdlet.ShouldProcess($InstallDir, '安装 p4delta.exe')) {
        New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
        Copy-Item -LiteralPath $sourceExe -Destination $targetExe -Force
        # 自留一份脚本：日后从安装目录就能卸载，不必找回 release 包。
        if ($PSCommandPath -and ([System.IO.Path]::GetFullPath($PSCommandPath) -ne [System.IO.Path]::GetFullPath((Join-Path $InstallDir $ScriptName)))) {
            Copy-Item -LiteralPath $PSCommandPath -Destination (Join-Path $InstallDir $ScriptName) -Force
        }
        Write-Info "已安装到 $targetExe"
    }

    $doc = Read-CustomToolsDocument $CustomToolsPath
    $changed = Update-ToolList $doc (Get-DesiredTools -WithoutCleanApply:$WithoutCleanApply) $targetExe

    # 退出口：以前注册过 APPLY 的话把它摘掉。结果要并进 $changed——只删不加时，
    # 下面那扇保存的门只认 $changed。
    $removedCleanApply = $false
    if ($WithoutCleanApply) {
        $removedCleanApply = Remove-CleanApplyTool $doc
        if ($removedCleanApply) {
            $changed = $true
        }
    }

    if (-not $changed) {
        Write-Info '工具定义已经是最新的，未改动。'
    } elseif ($PSCmdlet.ShouldProcess($CustomToolsPath, '写入 P4V 自定义工具定义')) {
        $backup = Backup-CustomToolsFile $CustomToolsPath
        Save-CustomToolsDocument $doc $CustomToolsPath
        Write-Info "已更新 $CustomToolsPath"
        if ($removedCleanApply) {
            Write-Info "已摘掉不可逆的「$CleanApplyTool」条目。"
        }
        if ($backup) {
            Write-Info "改动前的备份：$backup"
        }
    }

    if (-not $NoP4Check) {
        Test-P4Availability
    }

    if ($AddToPath) {
        $entry = [System.IO.Path]::GetFullPath($InstallDir)
        $current = Get-UserPathEntry
        $entries = @($current -split ';')
        if (Test-PathEntry $entries $entry) {
            Write-Info "PATH 里已经有 $entry"
        } elseif ($PSCmdlet.ShouldProcess('HKCU:\Environment', "把 $entry 加进用户级 PATH")) {
            $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
            try {
                $updated = (@($entries | Where-Object { $_ -ne '' }) + $entry) -join ';'
                $key.SetValue('Path', $updated, [Microsoft.Win32.RegistryValueKind]::ExpandString)
            } finally {
                $key.Dispose()
            }
            Write-Info "已加入用户级 PATH：$entry（新开的终端才会看到）"
        }
    }

    Write-Info ''
    if ($changed) {
        Write-Info '装好了。工具定义有改动，重启 P4V 后生效——它只在启动时读那个文件。'
    } else {
        # 只换 exe（scripts\install-local.ps1 装本地构建就是这种）时不必重启：Command 是
        # 绝对路径，P4V 每次点菜单都新起一个进程。
        Write-Info '装好了。工具定义没变，P4V 不用重启：下次点菜单用的就是这份 exe。'
    }
    if ($WithoutCleanApply) {
        Write-Info '（按 -WithoutCleanApply 没注册「clean 实际清理」；它不可逆，以前装过的话这次已经摘掉。）'
    } else {
        Write-Info "（「clean 实际清理」不可逆，放在「$CleanApplyFolder」子菜单里；动手前先跑一遍预演。）"
    }
}

function Invoke-Uninstall {
    $removedTools = $false
    $doc = Read-CustomToolsDocument $CustomToolsPath
    $changed = Remove-OurTools $doc $InstallDir
    if ($changed) {
        if ($PSCmdlet.ShouldProcess($CustomToolsPath, '摘掉 P4V 自定义工具定义')) {
            $backup = Backup-CustomToolsFile $CustomToolsPath
            Save-CustomToolsDocument $doc $CustomToolsPath
            $removedTools = $true
            Write-Info "已从 $CustomToolsPath 摘掉 p4delta 的工具定义"
            if ($backup) {
                Write-Info "改动前的备份：$backup"
            }
        }
    } else {
        Write-Info '工具定义里没有 p4delta，无需改动。'
    }

    if ($AddToPath) {
        $entry = [System.IO.Path]::GetFullPath($InstallDir)
        $current = Get-UserPathEntry
        $entries = @($current -split ';')
        if (Test-PathEntry $entries $entry) {
            if ($PSCmdlet.ShouldProcess('HKCU:\Environment', "把 $entry 从用户级 PATH 里摘掉")) {
                $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
                try {
                    $updated = (@($entries | Where-Object { $_ -ne '' -and ($_.TrimEnd('\') -ine $entry.TrimEnd('\')) }) -join ';')
                    $key.SetValue('Path', $updated, [Microsoft.Win32.RegistryValueKind]::ExpandString)
                } finally {
                    $key.Dispose()
                }
                Write-Info "已从用户级 PATH 里摘掉 $entry"
            }
        }
    }

    if (Test-Path -LiteralPath $InstallDir) {
        if ($PSCmdlet.ShouldProcess($InstallDir, '删除安装目录')) {
            try {
                Remove-Item -LiteralPath $InstallDir -Recurse -Force
                Write-Info "已删除 $InstallDir"
            } catch {
                # 脚本自己就住在这个目录里时，个别系统上可能删不掉正在被读的文件。
                Write-Warning "没删干净 $InstallDir：$($_.Exception.Message)  手工删掉即可。"
            }
        }
    } elseif (-not $removedTools) {
        Write-Info '没有找到安装目录。'
    }

    Write-Info ''
    Write-Info '卸载完成。重启 P4V 后菜单项就没了。'
    Write-Info '摘要缓存留在 %LOCALAPPDATA%\FastReconcile，下次装回来还能直接用；要清掉请手工删。'
}

function Get-ExeVersion([string] $Path) {
    try {
        $info = (Get-Item -LiteralPath $Path).VersionInfo
        if ($info -and $info.ProductVersion) {
            return $info.ProductVersion
        }
    } catch {
        # 假 exe（测试用的）没有版本资源，读不到是正常的。
    }
    return $null
}

try {
    if ($Uninstall) {
        Invoke-Uninstall
    } else {
        Invoke-Install
    }
} catch {
    Write-Host "error: $($_.Exception.Message)" -ForegroundColor Red
    exit 1
}

exit 0
