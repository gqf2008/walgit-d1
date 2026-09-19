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
