<#
.SYNOPSIS
  Assert that a Windows scheduled task's action is not a console program.

.DESCRIPTION
  The Task Scheduler's `Exec` action always gives a console-subsystem action a
  console; in an interactive session Windows Terminal (or the Windows 10 default
  terminal) shows it — `-WindowStyle Hidden` only hides it after the first
  visible frame. Measured on a real machine 2026-09-20: a task whose action was
  `powershell.exe -WindowStyle Hidden -File …` flashed a window every 60
  seconds (thread win-sched-task-console, AGENTS.md D53).

  The action must therefore be a GUI-subsystem executable (reference:
  `walgit-service-host.exe`) that spawns the real command without a console.
  This check reads the PE header's Subsystem field of the action executable:
  IMAGE_SUBSYSTEM_WINDOWS_GUI (2) passes, IMAGE_SUBSYSTEM_WINDOWS_CUI (3)
  fails. It is a property check, not a name allowlist, so a custom launcher
  passes while `powershell -WindowStyle Hidden` fails.

.PARAMETER TaskName
  The scheduled task to check, as shown by `Get-ScheduledTask`.

.EXAMPLE
  pwsh -File deploy\windows\task-action-check.ps1 -TaskName walgit
#>
param(
  [Parameter(Mandatory = $true)][string] $TaskName
)

$ErrorActionPreference = 'Stop'

$task = Get-ScheduledTask -TaskName $TaskName
$actions = @($task.Actions)
if ($actions.Count -ne 1) {
  throw "task '$TaskName': expected exactly one action, got $($actions.Count)"
}

$exe = [Environment]::ExpandEnvironmentVariables([string] $actions[0].Execute)
if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
  throw "task '$TaskName': action executable not found: $exe"
}

# PE layout: 'MZ' magic, e_lfanew at 0x3C, optional-header Subsystem at
# e_lfanew + 4 (signature) + 20 (COFF header) + 68.
$stream = [System.IO.File]::OpenRead($exe)
try {
  $reader = New-Object System.IO.BinaryReader($stream)
  if ($reader.ReadUInt16() -ne 0x5A4D) {
    throw "task '$TaskName': action '$exe' is not a PE executable (a script action is a console launch)"
  }
  $stream.Position = 0x3C
  $peOffset = $reader.ReadInt32()
  $stream.Position = $peOffset + 92
  $subsystem = $reader.ReadUInt16()
} finally {
  $stream.Dispose()
}

if ($subsystem -ne 2) {
  throw ("task '$TaskName': action '$exe' is a console image (PE subsystem $subsystem); the " +
    'scheduler gives it a console and the terminal shows a window. Launch it through a ' +
    'GUI-subsystem launcher (see deploy/windows/README.md).')
}

Write-Host "OK: task '$TaskName' action '$exe' is GUI-subsystem (no console)"
