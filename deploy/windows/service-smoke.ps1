# Walgit service lifecycle smoke (D48) — runs on the windows CI leg, which is an
# administrator, so `schtasks` really creates and runs a task.
#
# Everything here is a shape the old "the tray spawns, the pidfile lies" design
# got wrong, asserted against the real port and the real task:
#   * `start` forked a *second* server while one already held the port;
#   * `stop` reported success while the port stayed served, so `/healthz` kept
#     answering with the pre-upgrade build ("reinstalled, still old version");
#   * a server nobody supervises (an older install, a stray `walgit.exe serve`)
#     could not be stopped at all.
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
  if (-not (Get-Healthz)) { throw 'start did not bring /healthz up' }
  if ((Get-Listeners) -ne 1) { throw "expected exactly 1 listener after start, got $(Get-Listeners)" }

  Write-Host '--- start again (the port must stay served by exactly one process)'
  & $bin service start --config $cfg
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
  Write-Host "task settings: MultipleInstances=$($settings.MultipleInstances) ExecutionTimeLimit=$($settings.ExecutionTimeLimit) LogonType=$($taskObj.Principal.LogonType)"
  if ($taskObj.Principal.LogonType -ne 'Interactive') {
    throw "the task's LogonType is $($taskObj.Principal.LogonType), not the logged-on user's token"
  }
  if ((Get-Listeners) -ne 1) {
    throw "schtasks /Run started a second instance: $(Get-Listeners) listeners (MultipleInstancesPolicy?)"
  }

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
  if (-not (Wait-Free)) { throw "stop left the unsupervised server holding $(Get-Listeners) listener(s)" }
  # The point of the scenario: `service stop` killed a server it never started.
  if (-not $orphan.HasExited) { throw "the unsupervised server (pid $($orphan.Id)) survived `service stop`" }

  Write-Host '--- restart (stop + start in one command)'
  & $bin service restart --config $cfg
  if (-not (Get-Healthz)) { throw 'restart did not bring the service back' }
  & $bin service stop --config $cfg
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
