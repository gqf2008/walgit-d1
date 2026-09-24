# Bind-probed free-port picker, shared by the CI service steps
# (service-smoke.ps1 and the task-ownership step in ci.yml).
#
# A random port can land in a Hyper-V/WSL excluded range; Windows reports that
# as WSAEACCES (os error 10013) when the server binds it — a flake that showed
# up as "service start exited 1" on the windows CI leg (thread
# cc-ai-win-smoke-port-flake). A port is therefore *usable* only when this
# script can actually bind it on the v4 loopback: the excluded-range failure is
# a bind failure, and a port listing cannot see it.
#
# The kernel's own choice (bind port 0, read the assigned port) is never an
# excluded range, so the fallback cannot hit 10013 — it can only lose a race
# against another process, which the validation bind on the next attempt
# catches. Only 127.0.0.1 is probed: the server binds its `::1` twin
# best-effort and keeps serving without it (crates/walgit-server/src/lib.rs,
# `TcpAccept::bind`), so the twin is not part of the availability question.

function Get-WalgitFreePort {
  <#
  .SYNOPSIS
    Return a TCP port that can be bound on 127.0.0.1 right now.

  .PARAMETER Initial
    Optional candidate to try first. An un-bindable candidate (excluded range,
    already in use) is skipped; the caller's holder of that port is untouched.

  .EXAMPLE
    $port = Get-WalgitFreePort
    $port = Get-WalgitFreePort -Initial 40000
  #>
  param([int] $Initial = 0)

  $candidates = @()
  if ($Initial -gt 0) { $candidates = @($Initial) }

  for ($attempt = 0; $attempt -lt 10; $attempt++) {
    foreach ($candidate in $candidates) {
      $listener = $null
      try {
        $listener = [System.Net.Sockets.TcpListener]::new(
          [System.Net.IPAddress]::Parse('127.0.0.1'), $candidate)
        $listener.Start()
        return $candidate
      } catch {
        # excluded range (10013) or in use (10048): the next candidate
      } finally {
        if ($listener) { $listener.Stop() }
      }
    }

    # Ask the kernel: a port it just assigned us is by construction not in an
    # excluded range. It is re-validated by the next loop iteration's bind.
    $picker = [System.Net.Sockets.TcpListener]::new(
      [System.Net.IPAddress]::Parse('127.0.0.1'), 0)
    $picker.Start()
    $osPort = ([System.Net.IPEndPoint] $picker.LocalEndpoint).Port
    $picker.Stop()
    $candidates = @($osPort)
  }
  throw 'no bindable port found after 10 attempts'
}
