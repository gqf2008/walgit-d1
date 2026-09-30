<#
.SYNOPSIS
  只读断言:这些 Windows 可执行文件里**真的有**图标资源。

.DESCRIPTION
  线程 win-tray-no-embedded-icon:walgit 的 Windows 二进制曾经连资源目录都没有,shell 只能给
  桌面/开始菜单/任务栏画系统默认占位图。构建侧修好之后,这条断言就是回归门禁——"图标没编
  进去"必须在 CI 里红,而不是等用户再看到一次白图标。

  直接解析 PE 头(DataDirectory[2] → 资源目录树 → RT_GROUP_ICON / RT_ICON),不依赖任何外部
  工具。它也不是 PrivateExtractIcons 那种"拿不到就当没有"的探测:那条路会被图标缓存与调用
  进程影响,而这里要断言的正是 exe 自身的资源字节。

.EXAMPLE
  # 以文件形式跑(-File):走位置参数,一个文件一个实参
  pwsh -File deploy/windows/icon-check.ps1 target/release/walgit-tray.exe target/release/walgit-upgrade-helper.exe

.EXAMPLE
  # PowerShell 提示符里也可以用具名数组
  ./deploy/windows/icon-check.ps1 -Path target/release/walgit-tray.exe, target/release/walgit-upgrade-helper.exe
#>
[CmdletBinding()]
param(
    # Position = 0 与 ValueFromRemainingArguments **要成对**:只挂后者时,`pwsh -File` 路径下的
    # 第二个实参没有位置参数可绑,直接报"找不到接受自变量的位置参数"——人手照文档跑就废了
    # (独立审查 walgit-reviewer-2 条目 4e4273da 实测点出);两个都给,三种调用形式都能收进 $Path。
    [Parameter(Mandatory = $true, Position = 0, ValueFromRemainingArguments = $true)]
    [string[]] $Path
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RT_ICON = 3
$RT_GROUP_ICON = 14

function Convert-RvaToOffset {
    param([object[]] $Sections, [uint32] $Rva)
    foreach ($s in $Sections) {
        $span = [Math]::Max($s.VirtualSize, $s.RawSize)
        if ($Rva -ge $s.VirtualAddress -and $Rva -lt ($s.VirtualAddress + $span)) {
            return [int]($s.RawPointer + ($Rva - $s.VirtualAddress))
        }
    }
    throw ('RVA 0x{0:x} does not fall inside any section' -f $Rva)
}

function Read-PeHeader {
    param([byte[]] $Bytes)
    $pe = [BitConverter]::ToUInt32($Bytes, 0x3c)
    $coff = $pe + 4
    $sections = [BitConverter]::ToUInt16($Bytes, $coff + 2)
    $optionalSize = [BitConverter]::ToUInt16($Bytes, $coff + 16)
    $optional = $coff + 20
    $magic = [BitConverter]::ToUInt16($Bytes, $optional)
    $directories = $optional + $(if ($magic -eq 0x20b) { 112 } else { 96 })
    $table = @()
    for ($i = 0; $i -lt $sections; $i++) {
        $s = $optional + $optionalSize + 40 * $i
        $table += [pscustomobject]@{
            VirtualSize    = [BitConverter]::ToUInt32($Bytes, $s + 8)
            VirtualAddress = [BitConverter]::ToUInt32($Bytes, $s + 12)
            RawSize        = [BitConverter]::ToUInt32($Bytes, $s + 16)
            RawPointer     = [BitConverter]::ToUInt32($Bytes, $s + 20)
        }
    }
    return [pscustomobject]@{ Sections = $table; DataDirectories = $directories }
}

# 资源目录是三层树(type → name → language),叶子指向数据条目。
function Get-ResourceLeaf {
    param([byte[]] $Bytes, [int] $DirectoryOffset, [int] $ResourceBase, [string[]] $Path)

    $named = [BitConverter]::ToUInt16($Bytes, $DirectoryOffset + 12)
    $ids = [BitConverter]::ToUInt16($Bytes, $DirectoryOffset + 14)
    for ($i = 0; $i -lt ($named + $ids); $i++) {
        $entry = $DirectoryOffset + 16 + 8 * $i
        $nameId = [BitConverter]::ToUInt32($Bytes, $entry)
        $child = [BitConverter]::ToUInt32($Bytes, $entry + 4)
        $label = if (($nameId -band 0x80000000) -ne 0) {
            $at = $ResourceBase + [int]($nameId -band 0x7fffffff)
            $length = [BitConverter]::ToUInt16($Bytes, $at)
            [Text.Encoding]::Unicode.GetString($Bytes, $at + 2, 2 * $length)
        } else {
            [string]$nameId
        }
        if (($child -band 0x80000000) -ne 0) {
            Get-ResourceLeaf -Bytes $Bytes -DirectoryOffset ($ResourceBase + [int]($child -band 0x7fffffff)) `
                -ResourceBase $ResourceBase -Path ($Path + @($label))
        } else {
            $data = $ResourceBase + [int]$child
            [pscustomobject]@{
                Type     = if ($Path.Count -ge 1) { $Path[0] } else { '' }
                Name     = if ($Path.Count -ge 2) { $Path[1] } else { '' }
                DataRva  = [BitConverter]::ToUInt32($Bytes, $data)
                DataSize = [BitConverter]::ToUInt32($Bytes, $data + 4)
            }
        }
    }
}

function Test-IconResource {
    param([string] $File)

    $bytes = [System.IO.File]::ReadAllBytes($File)
    if ($bytes.Length -lt 0x40 -or $bytes[0] -ne 0x4d -or $bytes[1] -ne 0x5a) {
        throw 'not a PE image (no MZ header)'
    }
    $header = Read-PeHeader -Bytes $bytes
    $resourceRva = [BitConverter]::ToUInt32($bytes, $header.DataDirectories + 16)
    if ($resourceRva -eq 0) {
        throw 'the PE has no resource directory at all (DataDirectory[2].RVA = 0)'
    }
    $base = Convert-RvaToOffset -Sections $header.Sections -Rva $resourceRva
    $leaves = @(Get-ResourceLeaf -Bytes $bytes -DirectoryOffset $base -ResourceBase $base -Path @())

    $groups = @($leaves | Where-Object { $_.Type -eq [string]$RT_GROUP_ICON })
    if ($groups.Count -eq 0) {
        $types = ($leaves | Select-Object -ExpandProperty Type -Unique) -join ', '
        throw "no RT_GROUP_ICON resource (resource types present: $types)"
    }
    $bitmaps = @($leaves | Where-Object { $_.Type -eq [string]$RT_ICON } |
        ForEach-Object { $_.Name })

    $members = @()
    foreach ($group in $groups) {
        $at = Convert-RvaToOffset -Sections $header.Sections -Rva $group.DataRva
        $count = [BitConverter]::ToUInt16($bytes, $at + 4)
        for ($i = 0; $i -lt $count; $i++) {
            $m = $at + 6 + 14 * $i
            $width = $bytes[$m]
            $height = $bytes[$m + 1]
            $members += [pscustomobject]@{
                Width        = if ($width -eq 0) { 256 } else { [int]$width }
                Height       = if ($height -eq 0) { 256 } else { [int]$height }
                BitsPerPixel = [BitConverter]::ToUInt16($bytes, $m + 6)
                Id           = [BitConverter]::ToUInt16($bytes, $m + 12)
            }
        }
    }
    if ($members.Count -eq 0) { throw 'the icon group has no entries' }

    $missing = @($members | Where-Object { $bitmaps -notcontains ([string]$_.Id) } |
        ForEach-Object { $_.Id })
    if ($missing.Count -gt 0) {
        throw ("icon group references bitmaps that are not in the resource directory: {0}" -f ($missing -join ', '))
    }
    if (-not ($members | Where-Object { $_.Width -eq 256 -and $_.Height -eq 256 })) {
        $sizes = ($members | ForEach-Object { "$($_.Width)x$($_.Height)" }) -join ', '
        throw "no 256x256 entry in the icon group (sizes present: $sizes)"
    }
    return [pscustomobject]@{ Groups = $groups.Count; Members = $members }
}

$failures = @()
foreach ($file in $Path) {
    try {
        $resolved = (Resolve-Path -LiteralPath $file).Path
        $result = Test-IconResource -File $resolved
        $sizes = ($result.Members | ForEach-Object { "$($_.Width)x$($_.Height)" }) -join ', '
        Write-Host ("OK   {0}: {1} icon group(s), {2} entr(ies): {3}" -f (Split-Path $resolved -Leaf), $result.Groups, $result.Members.Count, $sizes)
    } catch {
        $failures += ("{0}: {1}" -f $file, $_.Exception.Message)
        Write-Host ("FAIL {0}: {1}" -f $file, $_.Exception.Message)
    }
}
if ($failures.Count -gt 0) {
    Write-Host ''
    Write-Host 'the Windows binaries must carry the product icon (RT_GROUP_ICON with a 256x256 entry):'
    $failures | ForEach-Object { Write-Host "  $_" }
    exit 1
}
Write-Host ("icon resources present in {0} file(s)" -f $Path.Count)
# 显式成功码:.ps1 不写 exit 时,$LASTEXITCODE 会留着上一条原生命令的值,CI 步骤可能把它当成自己的结论。
exit 0
