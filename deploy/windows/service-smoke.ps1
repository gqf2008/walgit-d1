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
  return @(Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue).Count
}
function Wait-Free {
  for ($i = 0; $i -lt 20; $i++) {
    if ((Get-Listeners) -eq 0) { return $true }
    Start-Sleep -Milliseconds 500
  }
  return ((Get-Listeners) -eq 0)
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
  Start-Sleep -Seconds 2
  if ((Get-Listeners) -ne 1) {
    throw "schtasks /Run started a second instance: $(Get-Listeners) listeners (MultipleInstancesPolicy?)"
  }

  Write-Host '--- status'
  $status = (& $bin service status --config $cfg | Out-String)
  Write-Host $status
  if ($status -notmatch 'running') { throw "status did not report a running service: $status" }

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
  & $bin service stop --config $cfg
  if (-not (Wait-Free)) { throw "stop left the unsupervised server holding $(Get-Listeners) listener(s)" }

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
