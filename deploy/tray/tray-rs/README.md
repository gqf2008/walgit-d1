# walgit-tray — 跨平台系统托盘

一套 Rust 代码,管理本机 walgit 服务(macOS / Windows / Linux 系统托盘)。

## 菜单

- **状态行**:`walgit 服务:运行中/已停止`(5 秒轮询 `/healthz`)
- **启动 / 停止服务**:macOS 与 Windows 都直接调用安装目录里的 `walgit service …`
  ——Windows 上那个进程归**任务计划程序**里的具名任务 `walgit`（D48），托盘不再自己
  spawn/supervise，也就没有可写坏的 pidfile。Linux 没有等价的桌面调度器，仍由托盘的
  supervisor 启动 `walgit serve`（setup wizard 保存后 exit 75 需要立即重启），pidfile
  写到 `~/.walgit`。程序文件不复制到状态目录。
- **⬆️ 发现新版本 — 点击升级**:升级**只由用户点击触发**。macOS App Bundle
  走 Release 管线:下载 DMG → 校验 sha256/签名/公证/版本 → 交给
  `release-install.sh` 换装(失败回滚旧 bundle)。Windows 安装目录走 Release
  管线:只选 `walgit-setup-<version>-x64.exe`，精确校验 GitHub sha256，下载
  新版+当前版本安装器，复制 `walgit-upgrade-helper.exe` 到状态目录后由 helper
  完成停服务、静默安装、起服务、`/healthz` 版本校验和失败回滚。开发机与
  Linux 走源码管线:ff-merge main → `cargo build --release -p walgit-cli` →
  备份(`walgit.bak-tray`)→ 停 → 热换 → 起服务 → 15s 健康验证,失败回滚
- **自动检测新版本:开/关**:开着时每 30 分钟(+启动 30 秒)`fetch` 比对;
  发现新版本仅提示(菜单 ⬆️ 项 + 图标状态),不自动升级
- **更新检查失败**:Release 与源码两条检测通道都失败时显示
  「更新检查失败(点击重试)」,不会伪装成「已是最新」;失败原因写 tray.log
- **打开 Web UI** / **退出托盘(服务保持运行)**

## 构建

```bash
cargo build --release        # 产物 target/release/walgit-tray(.exe)
```

依赖:Rust(含 std);Linux 需要 gtk3 + appindicator + xdo 开发库
(`libgtk-3-dev libayatana-appindicator3-dev libxdo-dev`)——tray-icon 在
Linux 走 appindicator,默认 feature 引 libxdo。macOS/Windows 无额外系统依赖。

- macOS:产物可直接运行；正式 DMG 打包由 `deploy/tray/macos/`(`build.sh` /
  `build-dmg.sh`)负责，App Bundle 的托盘本体就是这个二进制(#183)。
  首次启动会 bootstrap `~/.walgit`(旧 `~/walgit` 布局自动复制迁移并留 5
  分钟升级桥)，并优先在 `/usr/local/bin/walgit` 建 CLI 软链；该位置不可写时
  回退到 `~/.local/bin/walgit`，不再把正常的回退写成权限错误。显式
  `WALGIT_CLI_LINK` 仍保持单目标、失败即报错
- Windows:在 Windows 主机上 `cargo build --release`；安装目录放
  `walgit.exe` + `walgit-tray.exe` + `walgit-upgrade-helper.exe`，状态目录是
  `%USERPROFILE%\.walgit`
- Linux:同 Windows 形态，状态目录是 `~/.walgit`，桌面环境需支持 appindicator

### Windows 说明(issue #68 修复后的行为)

- **release 产物是 GUI 子系统**:双击无控制台黑窗,静默驻留托盘
  (debug 构建保留控制台,便于排查;panic 均落 `tray.log`)。
- **单实例**:命名互斥,双击多次第二个实例静默退出,不叠图标。
- **托盘图标默认可能藏在通知区溢出**(`^` 折叠面板):把它拖到任务栏,
  或设置 → 个性化 → 任务栏 → 通知区域,设为始终显示。
- 产物在本目录 `target/release/`——`.cargo/config.toml` 钉住了
  target-dir,不会被仓库根的 cargo 配置重定向到 `walgit/target/`。
- **子进程不经 shell**:Windows 上 git/cargo/taskkill 一律 argv 直传
  (`Command::args`,无引号拼装),且带 `CREATE_NO_WINDOW`(GUI 进程 spawn
  控制台程序不带它就闪黑窗);「打开 Web UI」用 `explorer <url>`——cmd 的
  `start` 经 Rust 参数转义后嵌套引号全灭,实测挂起事件循环 2 分钟以上。

## 约定

- 程序目录：安装目录 / App Bundle；状态目录：`~/.walgit`
  (Windows: `%USERPROFILE%\.walgit`)
- 源码仓库:`$WALGIT_REPO`,默认 `/Volumes/DataExt/GitHub/walgit`(仅源码
  升级通道需要;macOS Release 升级不需要本机 checkout)
- 服务地址:`http://127.0.0.1:8081`(healthz)
- 日志:`~/.walgit/tray.log`

## 已知边界

- 升级构建需要本机有 rustup/cargo(1.98.0 toolchain)与 git
- Linux:服务若在托盘之外启动且没有 pidfile,「停止服务」可能无法纳管,先在托盘里
  启动一次即可。Windows 没有这个问题 —— `walgit service stop` 以**端口**为准,pidfile
  已退役(D48)。
- `block` crate(上游 objc 依赖)有 future-incompat 提示,不影响功能
