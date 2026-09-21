# Linux 托盘后端评估：tray-icon(gtk) → ksni(StatusNotifierItem)

状态：**评估完成，建议迁移**（2026-09-21，线程 `cc-ai-tray-linux-ksni-eval`；实施另开卡）。
决策记录：见 AGENTS.md —— 本文只评估，不改变行为。

## 1. 动机

- `deploy/tray/tray-rs` 在 Linux 上的 glib 0.18 是**传递依赖**：`tray-icon 0.14` / `muda 0.13`
  → `libappindicator 0.9` / `gtk 0.18` → `glib 0.18`。代码里没有任何 `use gtk`/`glib`。
- Dependabot 的 medium 告警（GHSA-wrw7-89jp-8q8g）修复版是 glib 0.20，需要 gtk-rs 0.20 栈；
  上游 `tray-icon` 截至最新 0.25.1 仍要求 `gtk ^0.18` —— 在我们这一侧无解。
- **Slint 不是这个问题的解**：Slint 是 GUI 工具包，不提供系统托盘 API；托盘要的是
  `NSStatusItem` / `Shell_NotifyIcon` / `StatusNotifierItem` 三件 OS 集成，`tray-icon` 正是这层。
  换 UI 工具包既去不掉 glib（Linux 托盘后端仍是 gtk 或 SNI），还会净增依赖。

目标：把 Linux 后端换成 **ksni**（纯 Rust 的 SNI + DBusMenu 客户端），macOS / Windows 保持
`tray-icon`（那两条链路本来就没有 glib）。

## 2. 现状：托盘在 Linux 实际用到的能力（`deploy/tray/tray-rs/src/main.rs`）

- 图标：`Icon::from_rgba(icon_rgba(32, color), 32, 32)`，随服务状态换色（`set_icon`）。
- 菜单（1 条禁用状态行 + 4 个动作 + 3 个分隔符）：
  `status`(禁用)、`toggle`、sep、`upgrade`、sep、`web`、sep、`quit`。
- 动态更新：`set_text`（status/toggle/upgrade）、`set_enabled`（toggle/upgrade/quit）。
- 事件：`MenuEvent::receiver()` 按 id 分发；`with_menu_on_left_click(true)`。
- 未用：`TrayIconEvent`（无左键动作）、tooltip、子菜单、勾选项。

## 3. 候选：ksni 0.3.6

### 3.1 能力对照（逐项）

| 现在（tray-icon / muda） | ksni 对应 | 结论 |
|---|---|---|
| `Icon::from_rgba` + `set_icon` | `Tray::icon_pixmap() -> Vec<Icon>`（ARGB32）+ `Handle::update` | 等价；RGBA→ARGB 交换通道即可 |
| `MenuItem::with_id(id, label, enabled, None)` | `MenuItem::Standard(StandardItem{label, enabled, ..})` | 等价 |
| `PredefinedMenuItem::separator()` | `MenuItem::Separator` | 等价 |
| `MenuEvent` 按 id 分发 | 每个 item 自带 `activate: Fn(&mut T)` 回调 | 更直接；把 id 状态机改为按项绑定动作 |
| `set_text` / `set_enabled` | `Handle::update(\|tray\| …)`（发 DBus 变更） | 等价 |
| `with_menu_on_left_click(true)` | `const MENU_ON_ACTIVATE: bool = true` | 等价 |
| 无 tooltip | `tool_tip()`（可选） | 未用 |
| winit 事件泵 | 回调在 ksni 服务线程 → `std::sync::mpsc` → 主线程循环 | Linux 可**完全去掉 winit**（macOS/Windows 保留） |

### 3.2 原型与编译验证

原型（`/Volumes/DataExt/tmp/ksni-proto`，一次性草稿）：同一菜单结构 + 动态更新 + RGBA→ARGB
图标 + `blocking` API（`ksni = { version = "0.3", default-features = false, features = ["blocking", "async-io"] }`）。

```sh
cargo check --target x86_64-unknown-linux-gnu   # Finished，无错误
```

注意：`blocking` feature 自身**不启用** zbus 的运行时（feature 表里 `blocking: []`），
必须同时给 `async-io`（否则 zbus 编译失败）；`default`（tokio）是另一条可选路径。

### 3.3 依赖与运行时对比（同一口径：`cargo tree --target x86_64-unknown-linux-gnu --prefix none | sed 's/ (\*)$//' | sort -u`）

| 指标 | 现状（tray-icon/muda） | ksni 变体 | 差 |
|---|---|---|---|
| 去重包数 | **164** | **75**（原型，不含 winit） | −89 |
| 其中 gtk 系（glib/gtk/gdk/cairo/pango/atk/gio/libappindicator） | **20** | **0** | −20 |
| winit 栈（winit/x11/wayland/smithay/calloop/sctk…） | 19 | Linux 可 0（见 §3.1） | −19 |
| `.deb` 运行时依赖 | `libgtk-3-0, libayatana-appindicator3-1, libxdo3` | 无（仅需桌面 DBus） | −3 |
| 依赖形态 | C 库（gtk 系）+ dylib | 纯 Rust（zbus 5 / async-io；无 C 依赖） | — |
| glib 告警 | 有（无上游修复） | 无 | 消除 |

保留 winit 的保守变体约 75+19 ≈ 94 包，仍比现状少一个 gtk 栈与三个系统依赖。

## 4. 风险与未验证项

1. **SNI 宿主可用性**：图标只在有 `org.kde.StatusNotifierWatcher` 宿主时显示
   （KDE/Ubuntu 默认有；GNOME 需 AppIndicator 扩展）。现状的 `libappindicator` 也走同一协议，
   **宿主依赖不是 ksni 新增的**；但 libappindicator 历史上是否存在 GtkStatusIcon/XEmbed 旧式回退
   未在本机核实——实施卡要用真机矩阵确认（GNOME 无扩展场景），必要时在 `watcher_offline()` 记日志。
2. **二进制体积/内存**：包数下降不等于体积下降（gtk 是动态链接，ksni 是静态 Rust 依赖）。
   需在实施卡的 Linux CI 用 `--release` 实测两者二进制大小后记录。
3. **ksni 维护面**：0.3.6，小社区 crate；API 面小、协议冻结，必要时的兜底是 vendor。
4. **测试面**：DBus/SNI 无法在无头 CI 里端到端验证；可测的是**纯映射**（状态 → 菜单项
   label/enabled/动作）与回调分发，作为单测；现状的 `menu` 构造也未被单测覆盖，迁移时应顺带补。
5. **事件循环重构**：Linux 去 winit 需要 `main()` 平台分叉（macOS 的 NSApplication、Windows 的
   消息泵保持原样）；这是本迁移里最需要小心的一块。

## 5. 建议

**迁移**（Linux → ksni，macOS/Windows 不动）。理由：消掉 20 个 gtk 系包、三个 `.deb` 系统依赖
与唯一的 medium 安全告警；SNI 宿主依赖与现状同源；能力完全可映射（§3.1 逐项等价）。

**实施草案**（另开卡）：
- 结构：`tray` 抽象出小接口，Linux 用 `ksni` 模块（cfg-gated），其余平台保留 tray-icon；
  菜单状态（label/enabled/图标色）收敛为一个纯结构体，便于单测。
- 步骤：① 抽象 + Linux ksni 模块 → ② main 循环分叉（Linux 去 winit）→ ③ `.deb` 依赖更新
  （`deploy/linux/build-deb.sh` 去掉三个 Depends）→ ④ 单测（菜单映射/回调）→ ⑤ Linux CI 构建 +
  体积对比记录 → ⑥ 真机矩阵（KDE / Ubuntu 默认 / GNOME 无扩展）验收。
- 验收：Linux 功能与现状一致（图标状态色、菜单四项可点、动态文案/禁用态）；无 gtk/glib；
  `cargo tree` 目标数 ≤ 100；`.deb` 无 gtk/appindicator/xdo 依赖；glib 告警消失。
- 风险出口：若 GNOME 无扩展场景判定为必须回退（§4.1），保留 gtk 后端为可选 feature 或暂缓。

## 6. 复现命令

```sh
# 现状（gtk）
cd deploy/tray/tray-rs
cargo tree --target x86_64-unknown-linux-gnu --prefix none | sed 's/ (\*)$//' | sort -u | wc -l
# ksni 原型（仓库外草稿，源码见线程附件/§3.2 依赖行）
cd /Volumes/DataExt/tmp/ksni-proto && cargo check --target x86_64-unknown-linux-gnu
```
