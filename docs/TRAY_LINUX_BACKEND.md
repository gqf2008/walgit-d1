# Linux 托盘后端评估：消除 gtk/glib（tray-icon 的 ksni feature vs 自建 ksni 模块）

状态：**已实施**（2026-09-21，`cc-ai-tray-ksni-impl`：tray-icon 0.25 + Linux `ksni` feature，
`.deb` 依赖已去，三平台 check 与托盘测试绿；下方评估内容保留为当时依据）。原评估结论见下，
首轮复审 request_changes 已按 `345cc9c6` 意见更正，见 §7）。

## 1. 动机与更正后的前提

- `deploy/tray/tray-rs` 在 Linux 上的 glib 0.18 是**传递依赖**：`tray-icon 0.14` 的默认 feature
  `["muda-libxdo", "libappindicator"]` → `libappindicator 0.9` / `gtk 0.18` / `libxdo` → `glib 0.18`。
  代码里没有 `use gtk`/`glib`。
- Dependabot medium 告警（GHSA-wrw7-89jp-8q8g）的修复版 glib 0.20 需要 gtk-rs 0.20 栈；**旧依赖形态下无解**。
- **更正（首轮复审指出）**：`tray-icon` 0.25.1 提供一等公民 Linux/BSD SNI 后端 feature ——
  `ksni = ["dep:ksni", "muda-snapshot"]`；README 明确 `ksni` 后端不引入
  libappindicator/gtk/libxdo 系统库（除非同时启用 muda 的 GTK 后端）。**「上游无解」不成立**：
  升级 `tray-icon` 到 0.25 + 只开 `ksni` feature 就能去掉 glib。
- Slint 不是解：它是 GUI 工具包，没有系统托盘 API；托盘要的 `NSStatusItem`/`Shell_NotifyIcon`/
  `StatusNotifierItem` 正是 `tray-icon` 这层。换 UI 工具包既去不掉 Linux 托盘后端的 gtk，还净增依赖。

## 2. 现状：托盘在 Linux 实际用到的能力（`deploy/tray/tray-rs/src/main.rs`）

- 图标：`Icon::from_rgba(icon_rgba(32, color), 32, 32)`，随服务状态换色（`set_icon`）。
- 菜单：`status`(禁用) / `toggle` / sep / `upgrade` / sep / `web` / sep / `quit`。
- 动态更新：`set_text`（status/toggle/upgrade）、`set_enabled`（toggle/upgrade/quit）。
- 事件：`MenuEvent::receiver()` 按 id 分发；`with_menu_on_left_click(true)`。
- **tooltip 已用**：`.with_tooltip("walgit — 仓库活在桶上")`（`main.rs:1921`）。
- 未用：`TrayIconEvent`（无左键动作）、子菜单、勾选项。

## 3. 候选与实测

### 候选 A（推荐）：升级 `tray-icon` 0.25 + `ksni` feature

依赖形状（目标分平台；mac/win 不需要 ksni）：

```toml
[target.'cfg(target_os = "linux")'.dependencies]
tray-icon = { version = "0.25", default-features = false, features = ["ksni"] }

[target.'cfg(not(target_os = "linux"))'.dependencies]
tray-icon = { version = "0.25", default-features = false }
```

**实测（本机，真实托盘源码，零代码改动）**：
- `cargo check --target x86_64-unknown-linux-gnu` ✅（tray-icon 0.25 + ksni）
- `cargo check --target aarch64-apple-darwin` ✅
- `cargo check --target x86_64-pc-windows-msvc` ✅
- Linux 依赖树（同口径，含 winit）：**158 包，gtk/gdk/glib/libappindicator/libxdo = 0**（现状 164 / 20）。
- 单行写法（不按 target 分，所有平台都开 ksni）同样三平台编译通过，但 mac/win 会多带 ksni+zbus，不推荐。
- 复审另测的「仅 tray-icon 子树」口径：0.25+ksni ≈ 90 包；本表口径是**整个托盘应用**（含 winit），两者不矛盾。

### 候选 B（备选/后续优化）：Linux 自建 ksni 模块

- 原型（本次评估一次性草稿）：`Tray::icon_pixmap()->Vec<Icon>`（ARGB32）、
  `StandardItem{label,enabled,activate}`、`MenuItem::Separator`、`Handle::update`、
  `MENU_ON_ACTIVATE=true`、`tool_tip()` —— §2 的能力可逐项映射，无损失。
- 注意：`blocking` feature **不启用** zbus 运行时（feature 表 `blocking: []`），必须配 `async-io`
  （或 `default` 的 tokio）；`cargo check --target x86_64-unknown-linux-gnu` ✅。
- 规模：**75 包、0 gtk**（不含 winit）；若保留 winit 约 75+19≈94。
- 代价：Linux 专用模块 + `main()` 平台分叉（macOS 的 NSApplication / Windows 消息泵保持原样）；
  上游 ksni 后端**自管工作线程**，自建模块要自己承担这层（不是「顺手」的改动）。只有当我们要顺带
  在 Linux 去掉 winit 时才值得。

### 数字对照（同口径：`cargo tree --target x86_64-unknown-linux-gnu --prefix none | sed 's/ (\*)$//' | sort -u`）

| 指标 | 现状 0.14/gtk | 候选 A 0.25/ksni | 候选 B 自建 ksni |
|---|---|---|---|
| 去重包数（整个托盘） | 164 | **158** | 75（无 winit）/ 约 94（含） |
| gtk 系（glib/gtk/gdk/cairo/pango/atk/gio/libappindicator/libxdo） | 20 | **0** | 0 |
| `.deb` 运行时依赖 | libgtk-3-0, libayatana-appindicator3-1, libxdo3 | **无**（仅桌面 DBus） | 无 |
| 代码改动 | — | **零**（实测三平台编译通过） | Linux 模块 + main 分叉 |
| glib 告警 | 有 | **消除** | 消除 |

## 4. 风险与未验证项

1. **SNI 宿主可用性**：图标只在有 `org.kde.StatusNotifierWatcher` 宿主时显示（KDE/Ubuntu 默认有；
   GNOME 需 AppIndicator 扩展）。现状的 `libappindicator` 同为 SNI 客户端，宿主依赖不是新增；
   但**行为等价性**（含 GNOME 无扩展、tooltip 在各宿主是否渲染）需实施卡的真机矩阵确认。
2. **旧式回退**：libappindicator 历史上是否存在 GtkStatusIcon/XEmbed 回退未核实；ksni 后端没有。
   若矩阵发现「无宿主时现状有回退、ksni 没有」，需决定接受（现代桌面均有宿主）或保留 gtk 可选 feature。
3. **行为回归面**：0.14→0.25 跨多个大版本（muda 0.13→0.20），API 编译通过但**交互行为**
   （左键开菜单、tooltip、图标更新、菜单动态文案）要在真机矩阵里逐项验收；macOS/Windows 也要跑一遍
   现有托盘验收（升级通道/服务开关）。
4. **二进制体积**：包数不等于体积（gtk 动态链接 vs ksni 静态 Rust 依赖）；实施卡在 Linux CI 记录
   `--release` 体积对比。
5. **测试面**：SNI/DBus 无法无头端到端；可测的是菜单状态映射等纯函数（候选 A 不改代码，此项主要是回归）。

## 5. 建议与实施草案

**走候选 A**：升级 `tray-icon` 到 0.25 + Linux 开 `ksni` feature + `.deb` 依赖更新。
理由：零代码改动（三平台 check 已过）、去掉 20 个 gtk 系包与三个运行时系统依赖、消除 glib 告警；
候选 B 只在「顺带在 Linux 去掉 winit」时才划算，作为后续优化项记录。

实施卡步骤：
1. `deploy/tray/tray-rs/Cargo.toml`：按 §3 目标分平台改 `tray-icon` 依赖；锁文件更新。
2. `deploy/linux/build-deb.sh`：`Depends` 去掉 `libgtk-3-0, libayatana-appindicator3-1, libxdo3`。
3. 三平台 `cargo check` + 现有 tray CI（windows/macos/ubuntu）绿；记录 Linux `--release` 体积。
4. 真机矩阵：KDE / Ubuntu 默认 / GNOME 无扩展（图标、四菜单项、动态文案与禁用态、tooltip）。
5. 回归：macOS/Windows 托盘升级通道与服务开关 smoke 不变。

## 6. 复现命令

```sh
# 现状
cd deploy/tray/tray-rs
cargo tree --target x86_64-unknown-linux-gnu --prefix none | sed 's/ (\*)$//' | sort -u | wc -l   # 164
# 候选 A（一次性草稿，真实源码副本 + 目标分平台依赖）
cd /Volumes/DataExt/tmp/tray-bump-proto
for t in x86_64-unknown-linux-gnu aarch64-apple-darwin x86_64-pc-windows-msvc; do
  CARGO_TARGET_DIR=/Volumes/DataExt/tmp/tray-bump-target cargo check --target "$t"
done
CARGO_TARGET_DIR=/Volumes/DataExt/tmp/tray-bump-target cargo tree --target x86_64-unknown-linux-gnu --prefix none | sed 's/ (\*)$//' | sort -u | wc -l  # 158
# 候选 B 原型
cd /Volumes/DataExt/tmp/ksni-proto && cargo check --target x86_64-unknown-linux-gnu   # 75 包
```

## 7. 首轮复审（agent-codex）指正与处理

- **前提错误**：原文「上游无解」→ 已更正为「0.25.1 的 `ksni` feature 上游可直接切换」（§1/§3 候选 A）。
- **tooltip 事实错误**：原文记「未用」→ 已更正（`main.rs:1921`），并加入风险矩阵（§2/§4.1）。
- **winit 栈计数口径**：原文「19」与复审实测 18（含 cursor-icon/dpi/raw-window-handle 为 21）→
  已从主对照表移除该行，候选 B 的 75/94 说明中口径写明。
- **main 分叉表述**：已限定为「自建模块（候选 B）才需要」，并注明上游 ksni 后端自管工作线程（§3 候选 B）。
