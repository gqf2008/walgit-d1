# walgit 托盘应用(deploy/tray)

本机 walgit 服务的系统托盘:启停、新版本自动检测(检测到只提示,**由用户
点击才升级**)、退出仅退托盘(服务保持运行)。三平台共用同一套 Rust 实现
`tray-rs`(issue #183);`macos/` 只负责 App Bundle / DMG 的组装、签名与
公证,以及首次启动的状态目录 bootstrap。

| 目录 | 平台 | 技术 |
|---|---|---|
| `macos/` | macOS | 打包:`build.sh` 组 App Bundle、`build-dmg.sh` 签名+公证+出 DMG;`release-install.sh` 换装回滚 |
| `tray-rs/` | macOS / Windows / Linux | Rust + tray-icon + winit(独立 crate,**不加入** walgit workspace);Windows 更新 helper 也从这里构建并随安装器分发 |

## 菜单(三平台一致)

- **状态行**:`walgit 服务:运行中 · <版本>`(5 秒轮询 /healthz)
- **启动 / 停止服务**:统一走程序二进制里的 `walgit service start|stop|status|restart`
  （存活判断、起停、pidfile、日志轮转都在**二进制**里）。程序来自 App
  Bundle / 安装目录；`~/.walgit` 只放配置、cache、keys、日志和 pidfile。
- **版本升级状态行**(abb 式状态机):
  `版本 <version> · 检查更新…` → `正在检查更新…` → `已是最新 ✓(点击重查)`
  → `⬆️ 下载并升级到 v<version>(当前 <version>)` 或 `⬆️ 从源码升级到 <sha>`
  → `升级中… · 下载/校验/换装` → 成功回「已是最新」/ 失败保留旧版本
- **打开 Web UI**:直接打开页面(三平台一致)
- **退出托盘(服务保持运行)**

> macOS 入口(#197/#200):App Bundle 声明 `LSUIElement=false`,托盘同时驻留
> **Dock** 与菜单栏 —— 菜单栏状态项会被塞满、被刘海挡住在全屏应用下不可见,
> 没有 Dock 入口就等于整个应用找不到(Swift 版有 Dock,本轮迁移曾丢掉,已恢复)。
> **点 Dock 图标 = 打开 Web UI**:macOS 把这件事投递给应用 delegate 的
> `applicationShouldHandleReopen:hasVisibleWindows:`,winit 不转发它,所以托盘在
> 启动时把该方法注入 **winit 的 delegate 类**(它没实现,故不覆盖其行为;实现见
> `deploy/tray/tray-rs/src/main.rs` 的 `install_dock_reopen_hook`)。CI 没有 Dock
> 可点,`test.sh` 用 `WALGIT_REOPEN_SELFTEST=1` 直接调那个选择子作为回归门禁。

## 升级语义

自动的只有「检测」:启动 30 秒后、此后每 30 分钟检查一次。macOS App Bundle
与 Windows 安装目录用已安装 app 版本比对 GitHub latest release(机器上不需要
源码仓库)；开发机与 Linux 比对源码仓库 `HEAD` 与 `origin/main`——源码已
对齐 main 时,新的 Release 仍会提示。
发现更新 → 菜单行变「⬆️ 下载并升级」或「⬆️ 从源码升级」+ 系统通知；
**升级必须由用户点击**。

macOS Release 升级管线:下载 DMG → 校验 GitHub `sha256`(精确等值)→ 校验
签名/公证/版本(精确 token)→ 停服务 → 备份并替换 App Bundle → 从新
Bundle 启动服务 → 健康验证；失败恢复旧 App Bundle。`~/.walgit` 里的
配置、cache、keys、日志和凭证不参与换装或回滚。

Windows Release 升级管线:从安装目录运行且 `walgit.exe --version` 可读时进入
Release 通道(不需要源码仓库)。只选择 `walgit-setup-<version>-x64.exe`；
GitHub API 的 `sha256` digest 缺失/格式错误/与下载文件不等值时拒绝安装。
下载新版和当前版本两个安装器后，托盘把同目录的 `walgit-upgrade-helper.exe`
复制到 `%USERPROFILE%\.walgit\update\<pid>`，启动 helper 后退出。helper
等待旧托盘 PID 退出 → 用旧安装目录的 `walgit service stop`（D48 任务收尾）→ 静默运行新版安装器
(`/VERYSILENT /SUPPRESSMSGBOXES /NORESTART`) → 校验安装目录的二进制版本
→ `walgit service start` → 轮询 `/healthz` 的 `version` 等于目标版本。任一步
失败都运行旧版本安装器回滚，再启动旧服务、校验旧版本健康并重启旧托盘；
全过程写入 `%USERPROFILE%\.walgit\tray.log`。当前安装目录是
`%LOCALAPPDATA%\Programs\walgit`，同时识别旧 `%USERPROFILE%\walgit` 布局；安装器在
`{app}` 写的 `.walgit-install` 标记也覆盖自定义安装目录。
升级成功后 helper 会清理 `%USERPROFILE%\.walgit\update\<pid>`（含新版和回滚两份
安装器）；失败时保留该目录与两份安装器，便于排查。

> **Bootstrap 边界**：本机制从**首个包含 helper 的发布版**开始生效；此前已安装的
> Windows 装机版需要手动运行一次该版安装器，之后菜单升级才能生效。旧托盘没有
> Release 通道，无法自举到第一个支持它的版本。
>
> **真机验证边界**：UAC / 杀软 / 未签名告警、真实 Inno 安装器替换正在运行的
> 文件、真实 Task Scheduler 与安装器组合仍需发布后在真 Windows 上验证；当前
> `tray-rs` 集成测试使用临时目录中的假安装器真跑成功与健康失败回滚两条序列。
macOS 0.5.x/0.6.0 升级到新布局时，新 tray 会临时建立
`~/.walgit/walgit -> App Bundle/walgit` 和 `.skeleton-version`，让旧 helper
完成升级；5 分钟后自动清理，不保留程序副本。
健康检查与 `[server].listen` 同源,自定义端口不会被误判成服务已停止。

源码升级管线:ff-merge main → `cargo build --release -p walgit-cli` →
备份(`walgit.bak-tray`)→ 停 → 热换 → 起 → 15s 健康验证,失败自动回滚。
源码升级需要本机有 git 与 rustup/cargo。

## 约定

- **程序目录**:macOS 在 `walgit-tray.app/Contents/Resources/walgit`；
  Windows/Linux 在安装目录或 `/usr/bin`。程序不再复制到用户状态目录。
- **状态目录**:`~/.walgit`，只放 `walgit.toml`、`cache/`、`keys/`、
  `server.log`、`walgit.pid`、`.r2-credentials` 等用户状态。macOS 首次
  启动只初始化缺失的 `walgit.toml`，并优先建 `/usr/local/bin/walgit` 软链
  指向 App Bundle 内的程序；该位置不可写时回退到 `~/.local/bin/walgit`。
  显式 `WALGIT_CLI_LINK` 仍是单目标覆盖，失败会明确记录。
- **release 资产一个平台一件安装器(issue #108)**:
  macOS `walgit-<version>-arm64.dmg`(app 拖入 Applications,首次启动
  自动建部署骨架)、Windows `walgit-setup-<version>-x64.exe`
  (`deploy/windows/`,含托盘与本体)、Linux `walgit_<version>_amd64.deb`
  (`deploy/linux/build-deb.sh`,装 /usr/bin 三件 + 示例配置 + 托盘
  .desktop)。裸二进制不再发布。
- 服务地址:`http://127.0.0.1:8081`(托盘探活/开页与 `walgit.toml` 的
  `[server] listen` 同源解析,改端口不再需要改托盘;#73)
- 内存后端:托盘点状态行与 Web 概览页横幅显式标注「数据不落盘」(#73)
- 源码仓库:环境变量 `WALGIT_REPO`,默认 `/Volumes/DataExt/GitHub/walgit`
  (仅源码升级通道需要;macOS Release 升级不需要)
- 日志:`~/.walgit/tray.log`

## 构建

### macOS(DMG)

```bash
./build.sh 0.6.4                                    # 只组 App Bundle(cargo 构建 tray-rs)
./build-dmg.sh 0.6.4                                # 构建/注入版本/签名/公证
NOTARY_KEYCHAIN=/path/to/notary.keychain-db ./build-dmg.sh 0.6.4
./test.sh                                           # tray-rs 单测 + AppleDouble/公证/换装守卫
```

`build.sh` 用 `cargo build --release` 构建 `tray-rs` 并组装 App Bundle
(`WALGIT_TRAY_BIN` 可传预构建产物)；`build-dmg.sh` 自己执行
`WALGIT_BUILD_SHA=v<version> cargo build` 并断言
`walgit --version`；在原生 APFS 临时目录组装/签名，去 AppleDouble 后
用 `ditto --norsrc --noextattr` 提交公证，失败不会覆盖已有 DMG。默认
notary profile 为 `voicecall-notary`；非交互环境可显式传
`NOTARY_KEYCHAIN`。

CI 使用 `APPLE_ID` + `APPLE_TEAM_ID` + `APPLE_APP_PASSWORD` 直传公证凭据，
并设置 `CODESIGN_KEYCHAIN` + `CODESIGN_KEYCHAIN_PASSWORD` 让 `codesign`
显式使用临时钥匙串；三件套缺一即失败，不会悄悄回退到本机 profile。

构建只需要 Rust 工具链(无 Swift/Xcode SDK 依赖)。`build.sh` 把托盘
`walgit-tray`(tray-rs)放进 `Contents/MacOS`,walgit 二进制 +
`release-install.sh` + `walgit.toml.template` 打进 app Resources；首次启动
只在 `~/.walgit` 初始化缺失的状态配置(旧 `~/walgit` 布局则复制状态并留
5 分钟升级桥),产物做 ad-hoc 整体签名,独立
`codesign --verify --deep --strict` 可过。`build-dmg.sh` 走全链:app 签名+
公证+装订 → hdiutil 出 DMG(拖放安装)→ DMG 签名+公证+装订。Dock 品牌
图标:把 `walgit.icns` 放在同目录再跑 build.sh(可选,缺省用通用图标)。
开机自启:系统设置 → 通用 → 登录项 → 添加 walgit-tray.app。

菜单上的版本语义:upgrade 行显示**托盘 app 版本**(`版本 X`),正在跑的服务
版本另附为 `· 服务 Y`——两者不同步时不再把服务版本当成"当前/已最新"
(#170)。

`test.sh` 先跑 tray-rs 单测(Release JSON 解析含 stale asset/缺 digest 负例、
语义版本比较含 build metadata、菜单版本语义),再跑打包链路守卫, fixture 覆盖:

- 真实 App Bundle 布局：程序在 Resources，缺席 `walgit-ensure` /
  `run-walgit.sh` 等旧托管文件；
- `release-install.sh` 成功换装、版本不符时回滚 App Bundle；
- `~/.walgit` 状态初始化、旧布局清理、cache/用户文件保留；
- **升级前服务在跑** → 从新 App Bundle 重启并确认 `/healthz` 到 `v<new>`;
  **升级前停着** → 不拉起(尊重"停止服务"的显式意图);
  新版本;
- 构建/发布路径不再出现 Swift 托盘(源码名或编译命令命中即红)。

全部在临时状态目录内完成,不碰真实 `~/.walgit` 与 launchd/服务。

### Linux(.deb)

```bash
cargo build --release --bin walgit --bin walgit-server   # 先有二进制
cargo build --release --target-dir target --manifest-path deploy/tray/tray-rs/Cargo.toml
deploy/linux/build-deb.sh target/release 0.2.0           # 产物 walgit_0.2.0_amd64.deb
```

`dpkg-deb` 组装,零新依赖:三件二进制 → /usr/bin,`walgit.example.toml` 与
D43 未配置模板 → /usr/share/walgit,托盘 → /usr/share/applications;
postinst 只初始化安装用户的 `~/.walgit/walgit.toml`(幂等不覆盖)；程序
留在 `/usr/bin`，与 mac App Bundle / Windows 安装目录一致。CI 每个 PR 用
debug 二进制校验脚本(release.yml 打 tag 时用 release 二进制)。

### Windows / Linux / macOS(Rust)

```bash
cd deploy/tray/tray-rs && cargo build --release
# 产物 tray-rs/target/release/walgit-tray(.exe),与 walgit 放同一安装目录
```

Linux 需要 `libgtk-3-dev libayatana-appindicator3-dev libxdo-dev`
(tray-icon 走 appindicator,默认 feature 引 libxdo)。Windows 上 release
产物为 GUI 子系统(无控制台)、单实例、首次运行自动创建 `~/.walgit`
状态目录并写 `tray.log`;细节见 `tray-rs/README.md` 的「Windows 说明」。
