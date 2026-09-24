# walgit Windows 安装程序(deploy/windows)

一个 `setup.exe` 装完即有:walgit 服务二进制 + 系统托盘 + 初始配置。
Inno Setup 脚本(`installer.iss`),CI 在 `release.yml` 的 windows leg 打包,
release 附件名 `walgit-setup-<version>-x64.exe`(version = tag 去掉 `v`,
如 tag `v0.1.0` → `walgit-setup-0.1.0-x64.exe`)。

## 装了什么

| 项 | 位置 |
|---|---|
| `walgit.exe`(服务)/ `walgit-service-host.exe`(计划任务的无窗口 launcher,GUI 子系统)/ `walgit-tray.exe`(托盘)/ `walgit-upgrade-helper.exe`(独立升级 helper)/ `.walgit-install`(安装目录标记) | `%LOCALAPPDATA%\Programs\walgit`(可在向导中自定义 `{app}`) |
| `walgit.toml` 初始配置(`walgit.toml.initial`) | `%USERPROFILE%\.walgit\walgit.toml`;**仅在不存在时生成,卸载不删除** |
| 开始菜单 | 顶层 `walgit`(直接出现在「所有应用」里)+ 文件夹里的「walgit 托盘」「walgit 配置文件 walgit.toml」 |
| 桌面快捷方式 | **总是创建**(不做成可选项:Inno 的 `checkedonce` 只在首次安装生效,升级会沿用上次选择,老机器永远补不上) |
| 开机自启(HKCU `Run`) | 默认勾选,可取消 |

- 每用户安装(`PrivilegesRequired=lowest`),不需要管理员。
- 升级 = 再跑一遍 setup:替换二进制前自动结束在跑的托盘与服务
  (配置保留)。托盘菜单升级会先下载并校验新/旧两个安装器，再把
  `walgit-upgrade-helper.exe` 复制到 `%USERPROFILE%\.walgit\update\<pid>`
  后由它执行静默安装、健康校验和失败回滚。
- 卸载:删程序与快捷方式、清自启键、注销任务计划程序里的 `walgit` 任务；
  `%USERPROFILE%\.walgit` 下的 `walgit.toml`、`cache`、`keys`、`tray.log` 保留为
  用户数据。（Windows 已无 `walgit.pid` —— 服务归任务计划程序，D48。）

## 初始配置与对象存储

初始 `walgit.toml` 是 loopback + 内存后端的「先跑起来」形态：首次启动会进 Web UI 的
S3/R2 配置向导（D43），保存后在托盘菜单里重启服务生效（Windows 的服务归任务计划程序，
没有会替你 respawn 的 supervisor，D48）。也可以直接改 `[store]`（文件内有 R2 示例）。

## 定时任务:action 不得是控制台程序

**硬约束:任何计划任务的 action 都不得是控制台映像**（`cmd.exe`/`powershell.exe`/`pwsh.exe`，或自带控制台的 exe）。
任务计划程序的 `Exec` action 总是先给控制台程序一个控制台；在交互会话里 Windows Terminal（或 Win10 的默认终端）会把它显示出来——`-WindowStyle Hidden` 只能「随后隐藏」，hide 之前那一帧仍然可见，任务栏还可能留下可恢复的按钮。
2026-09-20 真机实测：一个每 60 秒运行、action 为 `powershell.exe -WindowStyle Hidden -File …` 的镜像任务，每轮都在桌面闪一次黑框；同一台机器上 `\walgit` 服务任务换成 GUI launcher 后不再出现窗口。

**做法:action 指向 GUI 子系统的 launcher，由它无窗口地拉起真正的命令。**

- 本仓参考实现：`crates/walgit-cli/src/bin/walgit-service-host.rs`（安装为 `walgit-service-host.exe`；`#![windows_subsystem = "windows"]`，用 `cmd /d /s /c` + `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP` 启动命令、等它结束并回传退出码，`>> log 2>&1` 重定向仍由 `cmd` 持有）。不要加 `DETACHED_PROCESS`：脱离控制台的进程没有可继承的控制台，console 子进程会重新分配一个，窗口就回来了。
- 自定义 launcher 的最小写法：一个只加 `#![windows_subsystem = "windows"]` 的 Rust bin（或任何 GUI 子系统可执行文件），`Command::new("cmd").args(["/d", "/s", "/c", cmd]).creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP).status()`。不要用 `powershell -WindowStyle Hidden` 交差。

注册一条「每 N 分钟跑一次」的任务可照抄（命令经 `-EncodedCommand` 传 UTF-16LE base64，带空格/引号的路径不会被二次解析）：

```powershell
$hostExe = Join-Path $env:LOCALAPPDATA 'Programs\walgit\walgit-service-host.exe'
$command = '"C:\path\to\job.exe" --once 1>> "C:\logs\job.log" 2>&1'
$encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($command))
$action  = New-ScheduledTaskAction -Execute $hostExe -Argument "-EncodedCommand $encoded"
$trigger = New-ScheduledTaskTrigger -Once -At (Get-Date) -RepetitionInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName 'my-job' -Action $action -Trigger $trigger -Force
```

**注册后自查**（消费方也可用；读 action 可执行文件的 PE 头断言子系统为 GUI(2)，不是名字白名单——`powershell.exe` 这类控制台映像会直接报错）：

```powershell
pwsh -File deploy\windows\task-action-check.ps1 -TaskName my-job
```

CI 里 `service-smoke.ps1` 对 `\walgit` 任务除自身同类断言外，也直接调用这条自查（D53）。

`service-smoke.ps1` 与 CI 的 task-ownership 步骤共用 `free-port.ps1` 选端口（绑定探测，
避开 Hyper-V/WSL 保留段与已被占用的端口——随机端口落保留段会以 os error 10013 假红，
见线程 cc-ai-win-smoke-port-flake）。

## 本机构建

需要 [Inno Setup **6.4+**](https://jrsoftware.org/isinfo.php)(`ISCC` 在 PATH;
脚本用 `x64compatible` 架构值,随库中文语言包为 UTF-8 无 BOM,均需 6.3+,
isl 自述面向 6.4;GitHub Actions windows runner 已预装)。在仓库根:

```powershell
cargo build --release --bin walgit
cargo build --release --target-dir target --manifest-path deploy/tray/tray-rs/Cargo.toml
ISCC -DMyAppVersion=0.1.0 deploy\windows\installer.iss
# 产物 deploy/windows/Output/walgit-setup-0.1.0-x64.exe
```

版本号 CI 以 tag 覆盖(`-DMyAppVersion=<tag 去掉 v>`),本地缺省
`0.0.0-dev`。构建产物目录 `deploy/windows/Output/` 已 gitignore。

> 不要「以管理员身份运行」安装器：程序与状态目录按当前用户解析，提权
> 运行会装进管理员的 profile，当前用户的托盘将找不到程序与配置。
