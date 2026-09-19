# Walgit service lifecycle smoke (D48) — runs on the windows CI leg, which is an
# administrator, so `schtasks` really creates and runs a task.
#
# Everything here is a shape the old "the tray spawns, the pidfile lies" design
# got wrong, asserted against the real port and the real task:
#   * `start` forked a *second* server while one already held the port;
#   * `stop` reported success while the port stayed served, so `/healthz` kept
#     answering with the pre-upgrade build ("reinstalled, still old version");
#   * a server nobody supervises (an older install, a stray `walgit.exe serve`)
#     could not be stopped at all;
#   * D48's `cmd /c` action made the interactive task show a console window.
$ErrorActionPreference = 'Stop'

$root = Join-Path $env:TEMP "walgit-service-smoke-$PID"
New-Item -ItemType Directory -Force -Path $root | Out-Null
# A random high port: a fixed one collides with whatever else runs on the host.
$port = Get-Random -Minimum 20000 -Maximum 60000
$listen = "127.0.0.1:$port"
$cfg = Join-Path $root 'walgit.toml'

# A deliberate in-memory store (the flag D43 requires) keeps the smoke free of any
# bucket or credential; nothing here touches the user's real repositories.
@"
[server]
listen = "$listen"
roles = []

[server.auth]
mode = "none"

[store]
backend = "memory"
memory_backend_intentional = true

[cache]
dir = "$($root -replace '\\','/')/cache"
"@ | Set-Content -Path $cfg -Encoding ascii

$bin = (Resolve-Path 'target/debug/walgit.exe').Path
$task = 'walgit'   # the name `walgit service` uses; created and removed by this run
$log = Join-Path $env:USERPROFILE '.walgit\server.log'

function Get-Healthz {
  try { return (Invoke-WebRequest -TimeoutSec 2 "http://$listen/healthz").Content }
  catch { return $null }
}
function Get-Listeners {
  # Distinct *processes*: walgit binds the v4 address and its `::1` twin, so the
  # socket rows are two while the server is one.
  return @(Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue |
    Select-Object -ExpandProperty OwningProcess -Unique).Count
}

# The D48 regression was a *shape* bug that a healthy port could not see: the
# task ran `cmd /c ...` in the interactive session, so Windows showed its
# console while the service itself worked. Keep this assertion on the scheduler
# object and exercise it against the old shape in the positive control below.
function Assert-HiddenServiceTask {
  param(
    [Parameter(Mandatory)] $TaskObject,
    [Parameter(Mandatory)] [string] $ExpectedExe,
    [Parameter(Mandatory)] [string] $ExpectedConfig,
    [Parameter(Mandatory)] [string] $ExpectedLog
  )

  if (-not [bool] $TaskObject.Settings.Hidden) {
    throw "the task's Hidden setting is false; Windows may show its console"
  }
  $actions = @($TaskObject.Actions)
  if ($actions.Count -ne 1) {
    throw "expected exactly one task action, got $($actions.Count)"
  }
  $action = $actions[0]
  $actionExe = [System.IO.Path]::GetFileName([string] $action.Execute)
  if ($actionExe -ine 'powershell.exe') {
    throw "the task action runs '$actionExe', not powershell.exe"
  }
  if ([string] $action.Arguments -notmatch '(?i)(?:^|\s)-WindowStyle\s+Hidden(?:\s|$)') {
    throw "the task action does not contain '-WindowStyle Hidden': $($action.Arguments)"
  }
  foreach ($flag in @('-NoLogo', '-NoProfile', '-NonInteractive')) {
    if ([string] $action.Arguments -notmatch "(?i)(?:^|\s)$([regex]::Escape($flag))(?:\s|$)") {
      throw "the task action does not contain '$flag': $($action.Arguments)"
    }
  }
  if ([string] $action.Arguments -notmatch '(?i)(?:^|\s)-EncodedCommand\s+([A-Za-z0-9+/=]+)(?:\s|$)') {
    throw "the task action does not carry an encoded command: $($action.Arguments)"
  }
  try {
    $script = [System.Text.Encoding]::Unicode.GetString([System.Convert]::FromBase64String($Matches[1]))
  } catch {
    throw "the task action's encoded command cannot be decoded: $_"
  }
  foreach ($needle in @(
    'walgit-service-task-v1',
    'cmd /c',
    'serve --config',
    '>>',
    '2>&1',
    'CreateNoWindow = $true',
    $ExpectedExe,
    $ExpectedConfig,
    $ExpectedLog
  )) {
    if ($script.IndexOf($needle, [System.StringComparison]::OrdinalIgnoreCase) -lt 0) {
      throw "the hidden task action is missing '$needle': $script"
    }
  }
}
# "Free" is decided by *connecting*, not by a socket listing: a failing
# Get-NetTCPConnection returns an empty set, and reading that as "nothing
# listens" is how a smoke (or a stop) can green-light a port that is still served.
function Test-PortFree {
  # Both families: the server binds the v4 address *and* its `::1` twin, so a
  # v4-only check would call the port free while the twin still listens.
  foreach ($addr in @('127.0.0.1', '::1')) {
    try {
      $c = [System.Net.Sockets.TcpClient]::new()
      $c.Connect($addr, $port)
      $c.Close()
      return $false
    } catch {
      # Nothing on that address (or no IPv6 at all) — try the next family.
    }
  }
  return $true
}
function Wait-Free {
  for ($i = 0; $i -lt 20; $i++) {
    if (Test-PortFree) { return $true }
    Start-Sleep -Milliseconds 500
  }
  return (Test-PortFree)
}

$failed = $null
try {
  Write-Host '--- start'
  & $bin service start --config $cfg
  if ($LASTEXITCODE -ne 0) { throw "service start exited $LASTEXITCODE" }
  if (-not (Get-Healthz)) { throw 'start did not bring /healthz up' }
  if ((Get-Listeners) -ne 1) { throw "expected exactly 1 listener after start, got $(Get-Listeners)" }

  Write-Host '--- start again (the port must stay served by exactly one process)'
  & $bin service start --config $cfg
  if ($LASTEXITCODE -ne 0) { throw "service start (second) exited $LASTEXITCODE" }
  Start-Sleep -Seconds 2
  if ((Get-Listeners) -ne 1) { throw "a second start forked another server: $(Get-Listeners) listeners" }
  if (-not (Get-Healthz)) { throw 'still one listener, but /healthz stopped answering' }

  Write-Host '--- schtasks /Run again (IgnoreNew must refuse the duplicate)'
  # `walgit service start` returns early on a healthy port, so it never reaches
  # /Run: only asking the scheduler directly proves the policy holds.
  schtasks /Run /TN $task | Out-Host
  if ($LASTEXITCODE -ne 0) { throw "schtasks /Run failed with exit code $LASTEXITCODE" }
  Start-Sleep -Seconds 2
  # The policy itself, not just its effect: a forked child would die on the bind
  # anyway, so the listener count alone cannot tell IgnoreNew from Parallel.
  #
  # Asked of the scheduled-task *object*: `schtasks /Query /XML` emits UTF-16,
  # which PowerShell decodes with the console encoding (mojibake), and the
  # `MultipleInstances` property is an enum — unaffected by either.
  $taskObj = Get-ScheduledTask -TaskName $task -ErrorAction Stop
  $settings = $taskObj.Settings
  if ($settings.MultipleInstances -ne 'IgnoreNew') {
    throw "the task's MultipleInstancesPolicy is $($settings.MultipleInstances), not IgnoreNew"
  }
  # The default would kill a long-lived server after 72 hours.
  if ($settings.ExecutionTimeLimit -ne 'PT0S') {
    throw "the task's ExecutionTimeLimit is $($settings.ExecutionTimeLimit), not PT0S (unlimited)"
  }
  Assert-HiddenServiceTask -TaskObject $taskObj -ExpectedExe $bin -ExpectedConfig $cfg -ExpectedLog $log
  Write-Host "task settings: MultipleInstances=$($settings.MultipleInstances) ExecutionTimeLimit=$($settings.ExecutionTimeLimit) LogonType=$($taskObj.Principal.LogonType) Hidden=$($settings.Hidden)"
  if ($taskObj.Principal.LogonType -notin @('Interactive', 'InteractiveToken')) {
    throw "the task's LogonType is $($taskObj.Principal.LogonType), not the logged-on user's token"
  }
  if ((Get-Listeners) -ne 1) {
    throw "schtasks /Run started a second instance: $(Get-Listeners) listeners (MultipleInstancesPolicy?)"
  }

  # Positive control: the old visible `cmd /c` shape must fail both halves of
  # the new assertion. If either object is accepted, this guard is decorative.
  $oldVisible = [pscustomobject] @{
    Settings = [pscustomobject] @{ Hidden = $false }
    Actions = @([pscustomobject] @{ Execute = 'cmd.exe'; Arguments = '/c ""C:\walgit.exe" serve' })
  }
  $oldCmdWithHiddenSetting = [pscustomobject] @{
    Settings = [pscustomobject] @{ Hidden = $true }
    Actions = @([pscustomobject] @{ Execute = 'cmd.exe'; Arguments = '/c ""C:\walgit.exe" serve' })
  }
  foreach ($case in @(
    [pscustomobject] @{ Name = 'Hidden=false + cmd /c'; Object = $oldVisible },
    [pscustomobject] @{ Name = 'Hidden=true + cmd /c'; Object = $oldCmdWithHiddenSetting }
  )) {
    $rejected = $false
    try {
      Assert-HiddenServiceTask -TaskObject $case.Object -ExpectedExe $bin -ExpectedConfig $cfg -ExpectedLog $log
    } catch {
      $rejected = $true
    }
    if (-not $rejected) {
      throw "positive control failed: $($case.Name) passed the hidden-console assertion"
    }
  }
  Write-Host 'positive control: old visible cmd task shapes rejected'

  Write-Host '--- status'
  $status = (& $bin service status --config $cfg | Out-String)
  $statusExit = $LASTEXITCODE
  Write-Host $status
  if ($statusExit -ne 0) { throw "service status exited $statusExit" }
  # Anchored: `walgit: not running` also contains the word "running".
  if ($status -notmatch '(?m)^walgit: running\b') {
    throw "status did not report a running service: $status"
  }

  Write-Host '--- stop (the port must really go quiet)'
  & $bin service stop --config $cfg
  if ($LASTEXITCODE -ne 0) { throw "service stop exited $LASTEXITCODE" }
  if (-not (Wait-Free)) { throw "stop left $(Get-Listeners) listener(s) behind" }

  Write-Host '--- an unsupervised server (not the task) must still be stoppable'
  # Exactly the "an older install is still holding the port" shape: a walgit
  # server nobody supervises. `service stop` has to fall back to the port owner.
  $orphan = Start-Process -FilePath $bin -ArgumentList @('serve', '--config', $cfg) -PassThru
  $up = $false
  for ($i = 0; $i -lt 20; $i++) {
    if (Get-Healthz) { $up = $true; break }
    Start-Sleep -Milliseconds 500
  }
  if (-not $up) { throw 'the orphan server never came up; cannot test the port fallback' }
  if ($orphan.HasExited) { throw 'the orphan server exited before the stop — the fallback was not exercised' }
  & $bin service stop --config $cfg
  if ($LASTEXITCODE -ne 0) { throw "service stop (orphan) exited $LASTEXITCODE" }
  if (-not (Wait-Free)) { throw "stop left the unsupervised server holding $(Get-Listeners) listener(s)" }
  # The point of the scenario: `service stop` killed a server it never started.
  if (-not $orphan.HasExited) { throw "the unsupervised server (pid $($orphan.Id)) survived 'service stop'" }

  Write-Host '--- restart (stop + start in one command)'
  & $bin service restart --config $cfg
  if ($LASTEXITCODE -ne 0) { throw "service restart exited $LASTEXITCODE" }
  if (-not (Get-Healthz)) { throw 'restart did not bring the service back' }
  & $bin service stop --config $cfg
  if ($LASTEXITCODE -ne 0) { throw "service stop (after restart) exited $LASTEXITCODE" }
  if (-not (Wait-Free)) { throw 'stop after restart left the port busy' }

  Write-Host 'service smoke: OK'
}
catch {
  $failed = $_
  Write-Host "service smoke FAILED: $_"
}
finally {
  & $bin service stop --config $cfg 2>$null
  if (Get-Process -Name 'walgit' -ErrorAction SilentlyContinue) {
    Stop-Process -Name 'walgit' -Force -ErrorAction SilentlyContinue
  }
  schtasks /Delete /TN $task /F 2>$null | Out-Null
  Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
}
if ($failed) { exit 1 }
