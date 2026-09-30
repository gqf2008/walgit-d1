<#
.SYNOPSIS
  真机巡检:托盘进程的 GUI 线程是不是卡在 Windows 菜单循环里(那会吃掉整机的激活点击)。

.DESCRIPTION
  线程 win-tray-menu-capture-stuck:托盘的弹出菜单是**模态循环**(muda 的 TrackPopupMenu),
  循环期间持有全局鼠标捕获——任何窗口都收不到激活点击。用户症状就是「点哪儿都不聚焦、
  键盘也进不去」,而托盘是唯一常驻入口,旧版本只能杀进程自救。

  这条巡检只读采样 GetGUIThreadInfo:托盘任一 GUI 线程的 flags 里出现
  GUI_INMENUMODE(0x4)/GUI_POPUPMENUMODE(0x10) 且**连续**超过 -StuckSeconds 判 FAIL,
  并打印可以立即执行的处置命令(向菜单宿主投 WM_CANCELMODE)。

  托盘自己也有自愈(见 deploy/tray/tray-rs/src/main.rs 的 menu watchdog,90s 阈值),
  所以正常机器上这条巡检应当永远 PASS。它的用处:① 用户报「点不动」时一眼判定是不是这个;
  ② 自愈失效或跑的是旧版托盘时,给出可执行的处置。

  -SelfTest 不起托盘、不需要真机故障:它把判据(Test-MenuHold,纯函数)喂几组采样,
  断言「连续菜单模式超阈值 → 失败」「用户浏览几秒 → 不失败」「菜单关掉后重新计时」。
  CI 的 windows leg 跑这一条(见 .github/workflows/ci.yml)。

.EXAMPLE
  pwsh -File deploy/windows/tray-input-health.ps1 -SelfTest
  pwsh -File deploy/windows/tray-input-health.ps1 -Seconds 300 -StuckSeconds 60
#>
[CmdletBinding()]
param(
    [string] $ProcessName = 'walgit-tray',
    [int] $Seconds = 120,
    [int] $IntervalSeconds = 5,
    [int] $StuckSeconds = 60,
    [string] $LogPath = "$env:USERPROFILE\.walgit\tray.log",
    [switch] $SelfTest
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$GUI_INMENUMODE = 0x00000004
$GUI_POPUPMENUMODE = 0x00000010
$WM_CANCELMODE = 0x001F

# 判据(纯函数):采样序列里,连续处于菜单模式的时间是否达到阈值。
# 抽出来是为了能在 CI 里被断言——真机上诱发一次菜单循环卡死不可控(触发条件未知)。
function Test-MenuHold {
    param(
        [bool[]] $InMenu,
        [double] $StepSeconds,
        [int] $StuckSeconds
    )
    $run = 0.0
    foreach ($sample in $InMenu) {
        if ($sample) { $run += $StepSeconds } else { $run = 0.0 }
        if ($run -ge $StuckSeconds) { return $true }
    }
    return $false
}

if ($SelfTest) {
    $cases = @(
        @{ name = 'no menu mode at all is never stuck';     samples = @($false, $false, $false);        step = 5; stuck = 5;  expect = $false },
        @{ name = 'a menu browsed for a few seconds is fine'; samples = @($true, $true, $true, $false); step = 5; stuck = 60; expect = $false },
        @{ name = 'menu mode held past the threshold is stuck'; samples = @($false, $true, $true, $true, $true); step = 5; stuck = 20; expect = $true },
        @{ name = 'a closed menu resets the run';           samples = @($true, $true, $false, $true, $true); step = 5; stuck = 20; expect = $false }
    )
    $bad = 0
    foreach ($case in $cases) {
        $got = Test-MenuHold -InMenu $case.samples -StepSeconds $case.step -StuckSeconds $case.stuck
        if ($got -ne $case.expect) {
            Write-Host ("FAIL selftest: {0} -> got {1}, want {2}" -f $case.name, $got, $case.expect)
            $bad++
        } else {
            Write-Host ("ok   {0}" -f $case.name)
        }
    }
    if ($bad -gt 0) { exit 1 }
    Write-Host 'tray-input-health selftest passed'
    exit 0
}

if (-not ('WalgitTrayHealth.Native' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

namespace WalgitTrayHealth {
    [StructLayout(LayoutKind.Sequential)]
    public struct GuiThreadInfo {
        public int cbSize;
        public int flags;
        public IntPtr hwndActive;
        public IntPtr hwndFocus;
        public IntPtr hwndCapture;
        public IntPtr hwndMenuOwner;
        public IntPtr hwndMoveSize;
        public IntPtr hwndCaret;
        public int rcCaretLeft, rcCaretTop, rcCaretRight, rcCaretBottom;
    }

    public static class Native {
        [DllImport("user32.dll", SetLastError = true)]
        public static extern bool GetGUIThreadInfo(uint idThread, ref GuiThreadInfo lpgui);

        [DllImport("user32.dll")]
        public static extern bool PostMessage(IntPtr hWnd, uint Msg, IntPtr wParam, IntPtr lParam);
    }
}
'@
}

$tray = Get-Process -Name $ProcessName -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $tray) {
    Write-Host "FAIL: no '$ProcessName' process is running (is the tray installed/started?)"
    exit 2
}
$trayId = $tray.Id
Write-Host ("watching '$ProcessName' pid {0}; FAIL if a thread holds the menu loop for >= {1}s (sampling {2}s over {3}s)" -f $trayId, $StuckSeconds, $IntervalSeconds, $Seconds)

$samples = New-Object System.Collections.Generic.List[bool]
$stuckThread = 0
$menuOwner = [IntPtr]::Zero
$capture = [IntPtr]::Zero
$deadline = (Get-Date).AddSeconds($Seconds)
$failed = $false

while ((Get-Date) -lt $deadline) {
    $proc = Get-Process -Id $trayId -ErrorAction SilentlyContinue
    if (-not $proc) { Write-Host 'the tray exited while watching — nothing to judge'; exit 2 }
    $menuNow = $false
    foreach ($thread in $proc.Threads) {
        $info = [WalgitTrayHealth.GuiThreadInfo]::new()
        # SizeOf 吃实例,不吃 Type 对象(PowerShell 把 [type] 当托管对象 → "cannot be marshaled")。
        $info.cbSize = [Runtime.InteropServices.Marshal]::SizeOf($info)
        if ([WalgitTrayHealth.Native]::GetGUIThreadInfo([uint32]$thread.Id, [ref]$info)) {
            if (($info.flags -band ($GUI_INMENUMODE -bor $GUI_POPUPMENUMODE)) -ne 0) {
                $menuNow = $true
                $stuckThread = $thread.Id
                $menuOwner = $info.hwndMenuOwner
                $capture = $info.hwndCapture
            }
        }
    }
    $samples.Add($menuNow)
    if (Test-MenuHold -InMenu $samples.ToArray() -StepSeconds $IntervalSeconds -StuckSeconds $StuckSeconds) {
        $failed = $true
        break
    }
    Start-Sleep -Seconds $IntervalSeconds
}

if (-not $failed) {
    Write-Host ("OK: no tray thread held the menu loop for >= {0}s ({1} samples)" -f $StuckSeconds, $samples.Count)
    exit 0
}

$held = 0
for ($i = $samples.Count - 1; $i -ge 0 -and $samples[$i]; $i--) { $held++ }
Write-Host ("FAIL: pid {0} thread {1} has held the menu loop for ~{2}s (menuOwner=0x{3:x} capture=0x{4:x})." -f $trayId, $stuckThread, ($held * $IntervalSeconds), $menuOwner.ToInt64(), $capture.ToInt64())
Write-Host 'This is the "clicks go nowhere" failure: the menu loop owns the global mouse capture, so no window can be activated.'
$target = if ($menuOwner -ne [IntPtr]::Zero) { $menuOwner } else { $capture }
if ($target -ne [IntPtr]::Zero) {
    Write-Host 'Recover now (no admin needed; run in this session):'
    Write-Host ("  [WalgitTrayHealth.Native]::PostMessage([IntPtr]0x{0:x}, 0x001F, [IntPtr]::Zero, [IntPtr]::Zero)" -f $target.ToInt64())
}
if (Test-Path -LiteralPath $LogPath) {
    $watchdog = Select-String -LiteralPath $LogPath -Pattern 'menu watchdog' | Select-Object -Last 3
    if ($watchdog) {
        Write-Host 'tray.log said (self-heal attempts):'
        $watchdog | ForEach-Object { Write-Host ('  ' + $_.Line) }
    } else {
        Write-Host "tray.log has no 'menu watchdog' line: this tray build predates the self-heal, or the watchdog never fired."
    }
}
exit 1
