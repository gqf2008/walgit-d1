# Walgit service lifecycle smoke (D48) — runs on the windows CI leg, which is an
# administrator, so `schtasks` really creates and runs a task.
#
# The two failures this shape replaced were only visible end-to-end:
#   * `start` forked a *second* server while one already held the port, and wrote
#     the dying child's pid into the pidfile;
#   * `stop` then reported success while the port stayed served, so `/healthz`
#     kept answering with the pre-upgrade build ("reinstalled, still old version").
# Both are asserted below against the real port and the real task.
$ErrorActionPreference = 'Stop'

$root = Join-Path $env:TEMP "walgit-service-smoke-$PID"
New-Item -ItemType Directory -Force -Path $root | Out-Null
$port = 18099
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

function Get-Healthz {
  try { return (Invoke-WebRequest -TimeoutSec 2 "http://$listen/healthz").Content }
  catch { return $null }
}
function Get-Listeners {
  return @(Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue).Count
}

$failed = $null
try {
  Write-Host '--- service start'
  & $bin service start --config $cfg
  if (-not (Get-Healthz)) { throw 'start did not bring /healthz up' }
  if ((Get-Listeners) -ne 1) { throw "expected exactly 1 listener after start, got $(Get-Listeners)" }

  Write-Host '--- service start again (must not fork)'
  & $bin service start --config $cfg
  Start-Sleep -Seconds 2
  if ((Get-Listeners) -ne 1) {
    throw "a second start forked another server: $(Get-Listeners) listeners"
  }
  if (-not (Get-Healthz)) { throw 'still one listener, but /healthz stopped answering' }

  Write-Host '--- service status'
  $status = (& $bin service status --config $cfg | Out-String)
  Write-Host $status
  if ($status -notmatch 'running') { throw "status did not report a running service: $status" }

  Write-Host '--- service stop (the port must really go quiet)'
  & $bin service stop --config $cfg
  if (Get-Healthz) { throw 'stop reported success but /healthz still answers' }
  if ((Get-Listeners) -ne 0) { throw "stop left $(Get-Listeners) listener(s) behind" }

  Write-Host 'service smoke: OK'
}
catch {
  $failed = $_
  Write-Host "service smoke FAILED: $_"
}
finally {
  & $bin service stop --config $cfg 2>$null
  Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
}
if ($failed) { exit 1 }
