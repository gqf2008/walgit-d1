# walgit Windows 安装程序(deploy/windows)

一个 `setup.exe` 装完即有:walgit 服务二进制 + 系统托盘 + 初始配置。
Inno Setup 脚本(`installer.iss`),CI 在 `release.yml` 的 windows leg 打包,
release 附件名 `walgit-setup-<version>-x64.exe`(version = tag 去掉 `v`,
如 tag `v0.1.0` → `walgit-setup-0.1.0-x64.exe`)。

## 装了什么

| 项 | 位置 |
|---|---|
| `walgit.exe`(服务)/ `walgit-tray.exe`(托盘) | `%LOCALAPPDATA%\Programs\walgit` |
| `walgit.toml` 初始配置(`walgit.toml.initial`) | `%USERPROFILE%\.walgit\walgit.toml`;**仅在不存在时生成,卸载不删除** |
| 开始菜单 | 「walgit 托盘」「walgit 配置文件 walgit.toml」 |
| 可选:桌面快捷方式、开机自启(HKCU `Run`,默认勾选自启) | |

- 每用户安装(`PrivilegesRequired=lowest`),不需要管理员。
- 升级 = 再跑一遍 setup:替换二进制前自动结束在跑的托盘与服务
  (配置保留)。
- 卸载:删程序与快捷方式、清自启键；`%USERPROFILE%\.walgit` 下的
  `walgit.toml`、`cache`、`keys`、`tray.log`、`walgit.pid` 保留为用户数据。

## 初始配置与对象存储

初始 `walgit.toml` 是 loopback + 内存后端的「先跑起来」形态:S3/R2 配置
向导(issue #70)落地前,手工把 `[store]` 换成真实桶(文件内有 R2 示例),
托盘菜单里重启服务生效。

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
