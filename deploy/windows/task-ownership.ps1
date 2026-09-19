# Ownership probe shared by the installer and CI. The status is written by
# PowerShell itself as BOM-less ASCII: Windows PowerShell's `>` redirection
# emits UTF-16LE, which Inno's LoadStringsFromFile cannot reliably read.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$OutFile,
    [string]$TaskName = 'walgit'
)

$ErrorActionPreference = 'Stop'

function Test-WindowsPath {
    param([string]$Token)

    if ($Token -match '^[A-Za-z]:[\\/]') { return $true }
    if ($Token.StartsWith('\\')) { return $true }
    if ($Token.StartsWith('.\') -or $Token.StartsWith('./')) { return $true }
    if ($Token.StartsWith('..\') -or $Token.StartsWith('../')) { return $true }
    return $false
}

function Test-OurImage {
    param([string]$Token)

    if ([string]::IsNullOrWhiteSpace($Token)) { return $false }
    $trimmed = $Token.Trim().Trim([char[]]@('"', "'"))
    if ([string]::IsNullOrWhiteSpace($trimmed)) { return $false }
    # A quoted command may contain spaces, but it must still look like a path,
    # not a sentence such as `echo C:\tools\walgit.exe`.
    if ($trimmed -match '\s' -and -not (Test-WindowsPath $trimmed)) { return $false }
    $base = ($trimmed -split '[\\/]')[-1]
    return $base -match '^(walgit|walgit-server)\.exe$'
}

function Get-QuotedToken {
    param([string]$Text)

    if (-not $Text.StartsWith('"')) { return $null }
    $end = $Text.IndexOf('"', 1)
    if ($end -lt 1) { return $null }
    return $Text.Substring(1, $end - 1)
}

function Get-FirstCommandToken {
    param([string]$Text)

    $Text = $Text.TrimStart()
    if ($Text.StartsWith('""')) { return Get-QuotedToken ($Text.Substring(1)) }
    if ($Text.StartsWith('"')) { return Get-QuotedToken $Text }
    $match = [regex]::Match($Text, '^(\S+)')
    if (-not $match.Success) { return $null }
    return $match.Groups[1].Value
}

function Get-CmdCommandToken {
    param([string]$Arguments)

    $text = $Arguments.TrimStart()
    while (-not [string]::IsNullOrWhiteSpace($text)) {
        $match = [regex]::Match($text, '^(\S+)(?:\s+(.*))?$', [System.Text.RegularExpressions.RegexOptions]::Singleline)
        if (-not $match.Success) { return $null }
        $token = $match.Groups[1].Value
        $text = $match.Groups[2].Value.TrimStart()
        if ($token -ieq '/c') { return Get-FirstCommandToken $text }
        if (-not $token.StartsWith('/')) { return $null }
    }
    return $null
}

function Get-ImageBase {
    param([string]$Command)

    $trimmed = $Command.Trim().Trim([char[]]@('"', "'"))
    if ([string]::IsNullOrWhiteSpace($trimmed)) { return $null }
    return ($trimmed -split '[\\/]')[-1]
}

function Get-PowerShellEncodedCommand {
    param([string]$Arguments)

    $match = [regex]::Match(
        $Arguments,
        '(?i)(?:^|\s)-EncodedCommand\s+([A-Za-z0-9+/=]+)(?:\s|$)'
    )
    if (-not $match.Success) { return $null }
    return $match.Groups[1].Value
}

function Get-PowerShellScript {
    param([string]$Arguments)

    $encoded = Get-PowerShellEncodedCommand $Arguments
    if ([string]::IsNullOrWhiteSpace($encoded)) { return $null }

    try {
        $bytes = [System.Convert]::FromBase64String($encoded)
    }
    catch {
        return $null
    }
    if (($bytes.Length % 2) -ne 0) { return $null }

    try {
        return [System.Text.Encoding]::Unicode.GetString($bytes)
    }
    catch {
        return $null
    }
}

function Test-OurPowerShellAction {
    param([string]$Arguments)

    $script = Get-PowerShellScript $Arguments
    if ([string]::IsNullOrWhiteSpace($script)) { return $false }

    # Keep the ownership predicate deliberately narrow: the hidden wrapper is
    # ours only if it is the generated cmd /c launcher for one of our exact
    # executables. A PowerShell task that merely mentions our path is foreign.
    if ($script -notmatch '(?i)\bwalgit-service-task-v1\b') { return $false }
    if ($script -notmatch '(?i)\bcmd(?:\.exe)?\s+/c\b') { return $false }
    if ($script -notmatch '(?i)(?<![\w.-])(?:walgit|walgit-server)\.exe(?![\w.-])') { return $false }
    return $true
}

$status = $null
try {
    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction Stop
}
catch {
    if ($_.FullyQualifiedErrorId -like 'CmdletizationQuery_NotFound*') {
        $status = 'absent'
    }
    else {
        $status = 'unknown'
    }
}

if ($null -eq $status) {
    try {
        if ($null -eq $task -or $null -eq $task.Actions) {
            $status = 'unknown'
        }
        else {
            $actions = @($task.Actions)
            if ($actions.Count -eq 0) {
                $status = 'unknown'
            }
            else {
                $ours = $false
                $unknown = $false
                foreach ($action in $actions) {
                    if ($null -eq $action) {
                        $unknown = $true
                        continue
                    }

                    $command = [string]$action.Execute
                    if ([string]::IsNullOrWhiteSpace($command)) {
                        $unknown = $true
                        continue
                    }

                    if (Test-OurImage $command) {
                        $ours = $true
                        continue
                    }

                    $base = Get-ImageBase $command
                    if ($base -ieq 'cmd' -or $base -ieq 'cmd.exe') {
                        $token = Get-CmdCommandToken ([string]$action.Arguments)
                        if ([string]::IsNullOrWhiteSpace($token)) {
                            $unknown = $true
                            continue
                        }
                        if (Test-OurImage $token) {
                            $ours = $true
                            continue
                        }
                    }
                    elseif (@('powershell', 'powershell.exe', 'pwsh', 'pwsh.exe') -contains $base) {
                        if (Test-OurPowerShellAction ([string]$action.Arguments)) {
                            $ours = $true
                            continue
                        }
                    }
                }

                if ($ours) {
                    $status = 'ours'
                }
                elseif ($unknown) {
                    $status = 'unknown'
                }
                else {
                    $status = 'foreign'
                }
            }
        }
    }
    catch {
        $status = 'unknown'
    }
}

if ($null -eq $status) { $status = 'unknown' }
[System.IO.File]::WriteAllText($OutFile, $status, [System.Text.Encoding]::ASCII)
