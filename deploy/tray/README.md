# walgit 托盘应用(deploy/tray)

本机 walgit 服务的系统托盘:启停、新版本自动检测(检测到只提示,**由用户
点击才升级**)、退出仅退托盘(服务保持运行)。macOS Swift 托盘同时感知
GitHub Release 与源码仓库；Rust 托盘保留源码仓库检测与安装器路径:

| 目录 | 平台 | 技术 |
|---|---|---|
| `macos/` | macOS | Swift + AppKit(菜单栏 NSStatusItem + Dock 双驻留,彩色状态图标,品牌 Dock 图标,点击 Dock 聚焦已开 Web UI 标签) |
| `tray-rs/` | macOS / Windows / Linux | Rust + tray-icon + winit(独立 crate,**不加入** walgit workspace) |

## 菜单(两个实现一致)

- **状态行**:`walgit 服务:运行中 · <版本>`(5 秒轮询 /healthz)
- **启动 / 停止服务**:统一走 `walgit service start|stop|status|restart`（存活判断、
  起停、pidfile、日志轮转都在**二进制**里；`walgit-ensure` 现在只是转发壳）。
  历史上 macOS 曾用 `walgit-ensure` + screen 保活，Windows/Linux
  分离进程启动部署目录下的 `walgit(.exe) serve`,pid 写 `walgit.pid`
- **版本升级状态行**(abb 式状态机):
  `版本 <version> · 检查更新…` → `正在检查更新…` → `已是最新 ✓(点击重查)`
  → `⬆️ 下载并升级到 v<version>(当前 <version>)` 或 `⬆️ 从源码升级到 <sha>`
  → `升级中… · 下载/校验/换装` → 成功回「已是最新」/ 失败保留旧版本
- **打开 Web UI**:直接打开页面(三平台一致)
- **退出托盘(服务保持运行)**

## 升级语义

自动的只有「检测」:启动 30 秒后、此后每 30 分钟检查一次。macOS Swift
托盘同时比较已安装 app 版本与 GitHub latest release,以及源码仓库
`HEAD` 与 `origin/main`；开发机即使源码已对齐 main,新 Release 仍会提示。
发现更新 → 菜单行变「⬆️ 下载并升级」或「⬆️ 从源码升级」+ 系统通知；
**升级必须由用户点击**。

Release 升级管线:下载 DMG → 校验 GitHub `sha256`(精确等值)→ 校验
签名/公证/版本(精确 token)→ 停服务 → 备份旧 app 与 `~/walgit` 托管文件
→ 替换 app → 新 tray bootstrap 原子更新托管文件 → 启动服务 → 健康验证,
失败恢复旧 app/托管文件并终止已启动的新托盘(不留「新进程 + 旧 bundle」)。
健康检查与 `[server].listen` 同源,自定义端口不会被误判成服务已停止。

源码升级管线:ff-merge main → `cargo build --release -p walgit-cli` →
备份(`walgit.bak-tray`)→ 停 → 热换 → 起 → 15s 健康验证,失败自动回滚。
源码升级需要本机有 git 与 rustup/cargo。

## 约定

- 部署目录:`$HOME/walgit`(Windows:`%USERPROFILE%\walgit`),内含
  `walgit(.exe)` + `walgit.toml`（部署家目录 `~/.walgit`）;macOS 另需 `walgit-ensure` 与
  `run-walgit.sh`(release DMG 的托盘首次启动会自动落盘这四件骨架,
  已存在的文件不覆盖;凭证 `~/walgit/.r2-credentials` 由使用者自填)。
  macOS 首次启动还会幂等建 `/usr/local/bin/walgit` 软链 → 部署二进制,
  终端直接可用(测试用 `WALGIT_CLI_LINK` 覆盖)。
- **release 资产一个平台一件安装器(issue #108)**:
  macOS `walgit-<version>-arm64.dmg`(app 拖入 Applications,首次启动
  自动建部署骨架)、Windows `walgit-setup-<version>-x64.exe`
  (`deploy/windows/`,含托盘与本体)、Linux `walgit_<version>_amd64.deb`
  (`deploy/linux/build-deb.sh`,装 /usr/bin 三件 + 示例配置 + 托盘
  .desktop)。裸二进制不再发布。
- 服务地址:`http://127.0.0.1:8081`(托盘探活/开页与 `walgit.toml` 的
  `[server] listen` 同源解析,改端口不再需要改托盘;#73)
- 内存后端:托盘点状态行与 Web 概览页横幅显式标注「数据不落盘」(#73)
- 源码仓库:环境变量 `WALGIT_REPO`,默认 `/Volumes/Workspace/GitHub/walgit`
- 日志:`<部署目录>/tray.log`

## 构建

### macOS(Swift)

```bash
./build-dmg.sh 0.2.0                                # 自动构建/注入版本/签名/公证
NOTARY_KEYCHAIN=/path/to/notary.keychain-db ./build-dmg.sh 0.2.0
./test.sh                                           # Release 解析/版本/AppleDouble 守卫
```

`build-dmg.sh` 自己执行 `WALGIT_BUILD_SHA=v<version> cargo build` 并断言
`walgit --version`；在原生 APFS 临时目录组装/签名，去 AppleDouble 后
用 `ditto --norsrc --noextattr` 提交公证，失败不会覆盖已有 DMG。默认
notary profile 为 `voicecall-notary`；非交互环境可显式传
`NOTARY_KEYCHAIN`。

需要 Xcode Command Line Tools(swiftc)。`build.sh` 把 walgit 二进制 +
`run-walgit.sh` + `walgit-ensure`(转发壳) + `release-install.sh` + `walgit.toml.template`
打进 app Resources——首次启动 bootstrap 到 `~/walgit`(幂等),产物做
ad-hoc 整体签名,独立 `codesign --verify --deep --strict` 可过。`build-dmg.sh`
走全链:app 签名+公证+装订 → hdiutil 出 DMG(拖放安装)→ DMG 签名+公证+
装订;`swiftc` 用 `MACOSX_DEPLOYMENT_TARGET=14.0`。Dock 品牌图标:把
`walgit.icns` 放在同目录再跑 build.sh(可选,缺省用通用图标)。开机自启:
系统设置 → 通用 → 登录项 → 添加 walgit-tray.app。

菜单上的版本语义:upgrade 行显示**托盘 app 版本**(`版本 X`),正在跑的服务
版本另附为 `· 服务 Y`——两者不同步时不再把服务版本当成"当前/已最新"
(#170)。

`test.sh` 覆盖 Release JSON 解析(含 stale asset/缺 digest 负例)、
语义版本比较(含 build metadata)、AppleDouble/损坏 zip 守卫,并用 fixture 覆盖:

- `release-install.sh` 成功换装、故障回滚(partial bootstrap 后四项托管文件
  一起还原)、非 8081 listen 探活;
- **升级前服务在跑** → 重启并确认 `/healthz` 到 `v<new>`;**升级前停着** →
  不拉起(尊重"停止服务"的显式意图);
- **手动装 DMG**(`bootstrapDeploy`)换完托管文件后,同样把在跑的服务重启到
  新版本;
- 菜单 upgrade 行在 idle/checking/latest/available(Release + 源码)/installing/
  failed 各状态的文案,以及 /healthz 版本字段的精确解析(v0.5.1 不匹配 v0.5.10)。

全部在临时部署目录内完成,不碰真实 `~/walgit` 与 launchd/服务。

### Linux(.deb)

```bash
cargo build --release --bin walgit --bin walgit-server   # 先有二进制
cargo build --release --target-dir target --manifest-path deploy/tray/tray-rs/Cargo.toml
deploy/linux/build-deb.sh target/release 0.2.0           # 产物 walgit_0.2.0_amd64.deb
```

`dpkg-deb` 组装,零新依赖:三件二进制 → /usr/bin,`walgit.example.toml` 与
D43 未配置模板 → /usr/share/walgit,托盘 → /usr/share/applications;
postinst 为安装用户落盘 `~/walgit` 骨架(幂等不覆盖,与 mac DMG /
Windows 安装器一致)。CI 每个 PR 用 debug 二进制校验脚本(release.yml
打 tag 时用 release 二进制)。

### Windows / Linux / macOS(Rust)

```bash
cd deploy/tray/tray-rs && cargo build --release
# 产物 tray-rs/target/release/walgit-tray(.exe),放到部署目录运行即可
```

Linux 需要 `libgtk-3-dev libayatana-appindicator3-dev libxdo-dev`
(tray-icon 走 appindicator,默认 feature 引 libxdo)。Windows 上 release
产物为 GUI 子系统(无控制台)、单实例、首次运行自动创建部署目录并写
`tray.log`;细节见 `tray-rs/README.md` 的「Windows 说明」。
