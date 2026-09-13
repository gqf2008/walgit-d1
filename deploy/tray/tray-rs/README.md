# walgit-tray — 跨平台系统托盘

一套 Rust 代码,管理本机 walgit 服务(macOS / Windows / Linux 系统托盘)。

## 菜单

- **状态行**:`walgit 服务:运行中/已停止`(5 秒轮询 `/healthz`)
- **启动 / 停止服务**:调用 `walgit service …`（`walgit-ensure` 已是转发壳）;Windows/Linux
  以分离进程启动部署目录下的 `walgit(.exe) serve --config walgit.toml`,
  pid 写 `walgit.pid`,停止 = kill 该 pid
- **⬆️ 发现新版本 — 点击升级** / **立即升级(拉 main 重建)**:升级**只由用户
  点击触发**——ff-merge main → `cargo build --release -p walgit-cli` →
  备份(`walgit.bak-tray`)→ 停 → 热换 → 起服务 → 15s 健康验证,
  失败自动回滚
- **自动检测新版本:开/关**:开着时每 30 分钟(+启动 30 秒)`fetch` 比对;
  发现新版本仅提示(菜单 ⬆️ 项 + 图标状态),不自动升级
- **打开 Web UI** / **退出托盘(服务保持运行)**

## 构建

```bash
cargo build --release        # 产物 target/release/walgit-tray(.exe)
```

依赖:Rust(含 std);Linux 需要 gtk3 + appindicator + xdo 开发库
(`libgtk-3-dev libayatana-appindicator3-dev libxdo-dev`)——tray-icon 在
Linux 走 appindicator,默认 feature 引 libxdo。macOS/Windows 无额外系统依赖。

- macOS:产物可直接运行;若要打包成 .app(LSUIElement,登录项可见),
  参考 `~/walgit/tray`(Swift 版)的 bundle 结构
- Windows:在 Windows 主机上 `cargo build --release`;部署目录
  `%USERPROFILE%\walgit` 放 `walgit.exe` + `walgit.toml`
- Linux:同 Windows 形态(`~/walgit/`),桌面环境需支持 appindicator

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

- 部署目录:`$HOME/walgit`(Windows:`%USERPROFILE%\walgit`;release 的
  `walgit-setup-<version>-x64.exe` 安装器——`deploy/windows/`——会装好全套)
- 源码仓库:`$WALGIT_REPO`,默认 `/Volumes/Workspace/GitHub/walgit`
- 服务地址:`http://127.0.0.1:8081`(healthz)
- 日志:`~/walgit/tray.log`

## 已知边界

- 升级构建需要本机有 rustup/cargo(1.98.0 toolchain)与 git
- Windows/Linux 的启动/停止用 pidfile;服务若在托盘之外启动(无 pidfile),
  「停止服务」会报 no pidfile——先在托盘里启动一次即可纳管
- `block` crate(上游 objc 依赖)有 future-incompat 提示,不影响功能
