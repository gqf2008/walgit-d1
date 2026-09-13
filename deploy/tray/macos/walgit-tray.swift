// walgit-tray — macOS 菜单栏托盘,管理本机 walgit 服务
// 功能:启停(walgit service)、新版本自动检测(每 30 分钟 fetch 比对,
//      打开 Web UI = 直接开页面,与其他平台一致;
//      发现新版本仅在菜单/通知里提示,由用户点击后才升级:
//      ff-merge main → cargo 构建 → 热换 → 健康验证,失败回滚)、
//      退出(仅退出托盘,服务不受影响)。
// 路径约定:部署目录 ~/.walgit(二进制 walgit + walgit.toml),
//          源码仓库默认 /Volumes/Workspace/GitHub/walgit
//          (可用 `defaults write com.walgit.tray repoPath <路径>` 覆盖)。
// 日志:~/.walgit/tray.log

import AppKit

let deployDir: String = {
    // 测试/多部署覆盖:WALGIT_DEPLOY_DIR 环境变量优先,defaults 其次,默认 ~/.walgit
    // (NSString 的 tilde 展开走 passwd 主目录,不认 HOME 环境变量)。
    if let d = ProcessInfo.processInfo.environment["WALGIT_DEPLOY_DIR"], !d.isEmpty { return d }
    if let d = UserDefaults.standard.string(forKey: "deployDir"), !d.isEmpty { return d }
    return NSString(string: "~/.walgit").expandingTildeInPath
}()
/// 部署目录 walgit.toml 行扫描 → (listen, backend, memoryIntentional);
/// 注释行/行尾注释跳过,找不到回退 127.0.0.1:8081(#73:探活/开页与配置
/// 同源,改 listen 不再使状态行恒「已停止」、升级健康验证恒失败)。
func deployConfig() -> (listen: String, backend: String, memoryIntentional: Bool) {
    var listen = ""
    var backend = ""
    var intentional = false
    let path = "\(deployDir)/walgit.toml"
    if let text = try? String(contentsOfFile: path, encoding: .utf8) {
        for raw in text.split(separator: "\n") {
            // TOML 行尾注释(#115 审查修正):仓库模板全是
            // `listen = "127.0.0.1:8081"  # 注释` 风格,先剥掉 # 起头部分。
            var line = raw.trimmingCharacters(in: .whitespaces)
            if let hash = line.firstIndex(of: "#") {
                line = String(line[..<hash]).trimmingCharacters(in: .whitespaces)
            }
            if line.isEmpty { continue }
            if line.hasPrefix("listen = \""), line.hasSuffix("\"") {
                listen = String(line.dropFirst("listen = \"".count).dropLast(1))
            } else if line.hasPrefix("backend = \""), line.hasSuffix("\"") {
                backend = String(line.dropFirst("backend = \"".count).dropLast(1))
            } else if line.hasPrefix("memory_backend_intentional = ") {
                intentional = line.hasSuffix("true")
            }
        }
    }
    if listen.isEmpty { listen = "127.0.0.1:8081" }
    return (listen, backend, intentional)
}

var healthURL: URL { URL(string: "http://\(deployConfig().listen)/healthz")! }
var webURL: URL {
    // walgit.localhost 解析回本机,浏览器 UI 惯用名不变,端口随配置走。
    let port = deployConfig().listen.split(separator: ":").last.map(String.init) ?? "8081"
    return URL(string: "http://walgit.localhost:\(port)/")!
}
let logPath = "\(deployDir)/tray.log"

/// 服务存活是 **walgit 二进制**的职责(`walgit service …`),托盘只是调用方:
/// 不再有 walgit-ensure 这种独立 shell 监督脚本。配置路径固定为部署目录下的
/// walgit.toml,与探活/开页同源。
func serviceCmd(_ verb: String) -> String {
    "'\(deployDir)/walgit' service \(verb) --config '\(deployDir)/walgit.toml'"
}

func logLine(_ s: String) {
    let line = "[\(DateFormatter.localizedString(from: Date(), dateStyle: .none, timeStyle: .medium))] \(s)\n"
    if let fh = FileHandle(forWritingAtPath: logPath) {
        fh.seekToEndOfFile()
        fh.write(line.data(using: .utf8)!)
        fh.closeFile()
    } else {
        try? line.write(toFile: logPath, atomically: true, encoding: .utf8)
    }
}

/// 跑一条 login-shell 命令(继承用户 PATH:cargo/rustup 等),返回 (code, out)。
func sh(_ command: String) -> (Int32, String) {
    let p = Process()
    p.executableURL = URL(fileURLWithPath: "/bin/zsh")
    p.arguments = ["-lc", command]
    let pipe = Pipe()
    p.standardOutput = pipe
    p.standardError = pipe
    do {
        try p.run()
    } catch {
        return (-1, "\(error)")
    }
    let data = pipe.fileHandleForReading.readDataToEndOfFile()
    p.waitUntilExit()
    return (p.terminationStatus, String(data: data, encoding: .utf8) ?? "")
}

func installedAppVersion() -> String {
    if let value = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String,
       !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
        return stripVersionPrefix(value)
    }
    if let value = try? String(contentsOfFile: "\(deployDir)/.skeleton-version", encoding: .utf8),
       !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
        return stripVersionPrefix(value)
    }
    return "0.0.0"
}

/// 菜单里的版本语义:upgrade 行判断的是**托盘 app 版本**(checkForUpdates 用
/// installedAppVersion),所以显示也用 app 版本;服务进程版本另附,避免
/// 「已是最新」旁边印着更旧的服务版本(#170)。
func menuVersionLine(appVersion: String, serviceVersion: String) -> String {
    let app = stripVersionPrefix(appVersion)
    let service = stripVersionPrefix(serviceVersion)
    guard !service.isEmpty else { return "版本 \(app)" }
    return "版本 \(app) · 服务 \(service)"
}

func upgradeLine(state: UpdateState, appVersion: String, serviceVersion: String,
                 release: ReleaseInfo?, sourceSha: String, busyNote: String) -> String {
    let versionText = menuVersionLine(appVersion: appVersion, serviceVersion: serviceVersion)
    let app = stripVersionPrefix(appVersion)
    switch state {
    case .idle:
        return "\(versionText) · 检查更新…"
    case .checking:
        return "\(versionText) · 正在检查更新…"
    case .latest:
        return "\(versionText) · 已是最新 ✓(点击重查)"
    case .available:
        if let release {
            return "⬆️ 下载并升级到 v\(release.version)(当前 \(app))"
        }
        return "⬆️ 从源码升级到 \(sourceSha)(当前 \(app))"
    case .installing:
        return "升级中…\(busyNote.isEmpty ? "" : " · " + busyNote)"
    case .failed:
        return "上次升级失败(点击下载发布页)"
    }
}

/// 跨线程传值容器:URLSession 回调与调用方用锁同步,规避并发捕获变量。
final class Locked<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: T?
    func set(_ newValue: T) {
        lock.lock(); value = newValue; lock.unlock()
    }
    func get() -> T? {
        lock.lock(); defer { lock.unlock() }; return value
    }
}

/// GitHub latest release. Tests may point `WALGIT_RELEASE_FIXTURE` at a local JSON
/// document; production uses the public API and only trusts a sha256 digest.
func latestRelease() -> ReleaseInfo? {
    if let fixture = ProcessInfo.processInfo.environment["WALGIT_RELEASE_FIXTURE"], !fixture.isEmpty {
        return try? fixtureReleaseCheck(path: fixture, currentVersion: installedAppVersion())
    }
    let endpoint = ProcessInfo.processInfo.environment["WALGIT_RELEASE_API"]
        ?? "https://api.github.com/repos/gqf2008/walgit-d1/releases/latest"
    guard let url = URL(string: endpoint) else { return nil }
    var request = URLRequest(url: url)
    request.timeoutInterval = 10
    request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
    request.setValue("walgit-tray", forHTTPHeaderField: "User-Agent")
    let semaphore = DispatchSemaphore(value: 0)
    let box = Locked<Data>()
    let task = URLSession.shared.dataTask(with: request) { body, response, _ in
        if let http = response as? HTTPURLResponse, http.statusCode == 200, let body {
            box.set(body)
        }
        semaphore.signal()
    }
    task.resume()
    guard semaphore.wait(timeout: .now() + 15) == .success else {
        task.cancel()
        return nil
    }
    guard let result = box.get() else { return nil }
    return try? parseLatestRelease(result)
}

func sourceUpdateSha() -> String? {
    guard hasSourceRepoPath() else { return nil }
    let repo = repoPathValue()
    _ = sh("git -C '\(repo)' fetch origin main 2>&1")
    let (c1, localOut) = sh("git -C '\(repo)' rev-parse HEAD")
    let (c2, remoteOut) = sh("git -C '\(repo)' rev-parse origin/main")
    let local = localOut.trimmingCharacters(in: .whitespacesAndNewlines)
    let remote = remoteOut.trimmingCharacters(in: .whitespacesAndNewlines)
    guard c1 == 0, c2 == 0, !local.isEmpty, !remote.isEmpty, local != remote else { return nil }
    return String(remote.prefix(7))
}

func hasSourceRepoPath() -> Bool {
    FileManager.default.fileExists(atPath: "\(repoPathValue())/.git")
}

func repoPathValue() -> String {
    UserDefaults.standard.string(forKey: "repoPath") ?? "/Volumes/Workspace/GitHub/walgit"
}

/// 从 /healthz 的 JSON 里取出 version 字段(与 release-install.sh 的
/// health_version 同口径)。用 JSON 解析而不是字符串包含:v0.5.1 不能匹配
/// v0.5.10。解析失败返回空串。
func healthVersion(_ body: String) -> String {
    guard let data = body.data(using: .utf8),
          let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
          let v = obj["version"] as? String
    else { return "" }
    return v
}

/// 手动装 DMG 只换文件,不会重启已在跑的服务进程 —— 进程仍拿着旧二进制,
/// /healthz 继续报旧版本,菜单看起来"升完级还是旧版"(#170)。只处理「本来
/// 就在跑」的服务(用户主动停掉的不拉起)。放到后台队列执行,避免拖住主线程。
private func restartServiceAfterUpgrade(bundledVersion: String, done: @Sendable @escaping () -> Void = {}) {
    let fm = FileManager.default
    let ok = sh("curl -sf --max-time 2 '\(healthURL)' 2>/dev/null").0 == 0
    guard ok else { done(); return }
    guard fm.isExecutableFile(atPath: "\(deployDir)/walgit") else { done(); return }
    _ = sh("\(serviceCmd("restart")) >/dev/null 2>&1 || true")
    let want = "v\(bundledVersion)"
    for _ in 0..<20 {
        let (hc, hout) = sh("curl -sf --max-time 2 '\(healthURL)' 2>/dev/null || true")
        if hc == 0, healthVersion(hout) == want {
            logLine("bootstrap: 服务已重启到 \(want)")
            done()
            return
        }
        Thread.sleep(forTimeInterval: 0.5)
    }
    logLine("bootstrap: 服务重启后未确认到 \(want),请在托盘里重启服务")
    done()
}

/// 首次启动 bootstrap:从 app bundle Resources 落盘 ~/walgit 部署骨架。
/// 托管文件(walgit 二进制)按 bundle 内
/// skeleton.version 覆盖更新——DMG 覆盖安装即升级;用户文件(walgit.toml)
/// 永不覆盖(配置与凭证安全)。开发构建(bundle 里没有 walgit 资源)跳过。
func bootstrapDeploy(onServiceRestart: @Sendable @escaping () -> Void = {}) {
    let fm = FileManager.default
    guard let res = Bundle.main.resourceURL?.path,
        fm.fileExists(atPath: "\(res)/walgit")
    else {
        logLine("bootstrap: bundle 无 walgit 资源(开发构建),跳过")
        onServiceRestart()
        return
    }
    do {
        try fm.createDirectory(atPath: deployDir, withIntermediateDirectories: true)
    } catch {
        logLine("bootstrap: 建 ~/.walgit 失败: \(error)")
        onServiceRestart()
        return
    }
    let bundledVersion = (try? String(contentsOfFile: "\(res)/skeleton.version", encoding: .utf8))
        .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) } ?? ""
    let marker = "\(deployDir)/.skeleton-version"
    let installedVersion = (try? String(contentsOfFile: marker, encoding: .utf8))
        .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) } ?? ""
    // 托管文件:版本不同则整体覆盖(覆盖安装 DMG = 升级路径)。先写
    // 临时名再原子替换,避免运行中的服务二进制被 remove+copy 的半状态
    // 捕获;只有三项全部成功且新二进制版本核验通过才写 marker。
    // `walgit-ensure` 只是个转发壳(逻辑在 walgit 二进制),仍随部署一起装;
    // run-walgit.sh 同理保留给 tray-rs/手工部署。
    let managed = ["walgit", "run-walgit.sh", "walgit-ensure"]
    // marker 不一致 → 整体换装;marker 一致但某个托管文件被删 → 只补缺失项。
    let versionChanged = !bundledVersion.isEmpty && bundledVersion != installedVersion
    let toWrite = managed.filter { f in
        versionChanged || !fm.fileExists(atPath: "\(deployDir)/\(f)")
    }
    var managedOK = true
    // 先把要写的全部落临时文件并核验版本;提升阶段逐项备份旧文件,任一失败
    // 回滚已替换项。避免「walgit 新、ensure 旧、marker 未写」的混合骨架,
    // 也避免 marker 一致但文件被删后不再自愈。
    if !toWrite.isEmpty && !bundledVersion.isEmpty {
        var staged: [String: String] = [:]
        for f in toWrite {
            let dst = "\(deployDir)/\(f)"
            let tmp = "\(dst).new-\(ProcessInfo.processInfo.processIdentifier)"
            do {
                try? fm.removeItem(atPath: tmp)
                try fm.copyItem(atPath: "\(res)/\(f)", toPath: tmp)
                try fm.setAttributes([.posixPermissions: NSNumber(value: 0o755)], ofItemAtPath: tmp)
                staged[f] = tmp
            } catch {
                managedOK = false
                logLine("bootstrap: 预置 \(f) 失败: \(error)")
            }
        }
        if managedOK, let stagedWalgit = staged["walgit"] {
            let (vc, versionOut) = sh("'\(stagedWalgit)' --version 2>&1")
            let token = versionOut.trimmingCharacters(in: .whitespacesAndNewlines)
                .split(separator: " ").last.map(String.init) ?? ""
            if vc != 0 || token != "v\(bundledVersion)" {
                managedOK = false
                logLine("bootstrap: walgit 版本核验失败: \(versionOut.trimmingCharacters(in: .whitespacesAndNewlines))")
            }
        }
        var installed: [(dst: String, backup: String?)] = []
        if managedOK {
            for f in toWrite {
                guard let tmp = staged[f] else { managedOK = false; break }
                let dst = "\(deployDir)/\(f)"
                let bak = "\(dst).old-\(ProcessInfo.processInfo.processIdentifier)"
                do {
                    var backup: String?
                    if fm.fileExists(atPath: dst) {
                        try? fm.removeItem(atPath: bak)
                        try fm.moveItem(atPath: dst, toPath: bak)
                        backup = bak
                    }
                    do {
                        try fm.moveItem(atPath: tmp, toPath: dst)
                    } catch {
                        if let backup { try? fm.moveItem(atPath: backup, toPath: dst) }
                        managedOK = false
                        logLine("bootstrap: 写入 \(f) 失败: \(error)")
                        break
                    }
                    installed.append((dst, backup))
                    logLine("bootstrap: 写入 \(f)")
                } catch {
                    managedOK = false
                    logLine("bootstrap: 备份 \(f) 失败: \(error)")
                    break
                }
            }
        }
        if !managedOK {
            for item in installed.reversed() {
                try? fm.removeItem(atPath: item.dst)
                if let backup = item.backup { try? fm.moveItem(atPath: backup, toPath: item.dst) }
            }
            for (_, tmp) in staged { try? fm.removeItem(atPath: tmp) }
            logLine("bootstrap: 托管文件更新未完成,已回滚到旧骨架")
        } else {
            for item in installed { if let backup = item.backup { try? fm.removeItem(atPath: backup) } }
        }
    }
    if versionChanged && !managedOK {
        onServiceRestart()
        return
    }
    // 用户文件:永不覆盖
    let userFile = "\(deployDir)/walgit.toml"
    if !fm.fileExists(atPath: userFile) {
        do {
            try fm.copyItem(atPath: "\(res)/walgit.toml", toPath: userFile)
            logLine("bootstrap: 写入 walgit.toml")
        } catch {
            logLine("bootstrap: walgit.toml 失败: \(error)")
        }
    }
    if !bundledVersion.isEmpty && installedVersion != bundledVersion {
        do {
            try bundledVersion.write(toFile: marker, atomically: true, encoding: .utf8)
            logLine("bootstrap: 骨架版本 \(installedVersion.isEmpty ? "新建" : "\(installedVersion) → \(bundledVersion)")")
        } catch {
            logLine("bootstrap: 写版本标记失败: \(error)")
        }
    }
    if versionChanged && managedOK {
        DispatchQueue.global(qos: .utility).async {
            restartServiceAfterUpgrade(bundledVersion: bundledVersion, done: onServiceRestart)
        }
    } else {
        onServiceRestart()
    }
    // CLI 软链:让终端里的 `walgit` 直达部署二进制(/usr/local/bin 归用户所有,
    // 无需管理员)。测试用 WALGIT_CLI_LINK 覆盖——必须与 WALGIT_DEPLOY_DIR
    // 成对设置,且 deployDir 须为绝对路径(软链目标按字面解析,不做 tilde
    // 展开;非绝对路径会改指真实 /usr/local/bin/walgit)。
    if !deployDir.hasPrefix("/") {
        logLine("bootstrap: deployDir 非绝对路径,跳过 CLI 软链")
    } else {
        let cliLink = ProcessInfo.processInfo.environment["WALGIT_CLI_LINK"].flatMap { $0.isEmpty ? nil : $0 }
            ?? "/usr/local/bin/walgit"
        do {
            if let attrs = try? fm.attributesOfItem(atPath: cliLink) {
                if attrs[.type] as? FileAttributeType == .typeSymbolicLink {
                    if (try? fm.destinationOfSymbolicLink(atPath: cliLink)) != "\(deployDir)/walgit" {
                        try fm.removeItem(atPath: cliLink)
                        try fm.createSymbolicLink(atPath: cliLink, withDestinationPath: "\(deployDir)/walgit")
                        logLine("bootstrap: 更新 CLI 软链 \(cliLink)")
                    }
                } else {
                    logLine("bootstrap: \(cliLink) 已存在且非软链(用户自己的文件),不覆盖")
                }
            } else {
                let parent = (cliLink as NSString).deletingLastPathComponent
                if !parent.isEmpty && parent != "." {
                    try fm.createDirectory(atPath: parent, withIntermediateDirectories: true)
                }
                try fm.createSymbolicLink(atPath: cliLink, withDestinationPath: "\(deployDir)/walgit")
                logLine("bootstrap: 建 CLI 软链 \(cliLink)")
            }
        } catch {
            logLine("bootstrap: CLI 软链失败: \(error)")
        }
    }
    logLine("bootstrap: 部署骨架就绪(\(deployDir))")
}

enum UpdateState {
    case idle, checking, latest, available, installing, failed
}

final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuDelegate {
    var statusItem: NSStatusItem!
    var pollTimer: Timer?
    var autoTimer: Timer?
    var serviceState = "checking"   // running | stopped | checking
    var serviceVersion = ""
    var transitioning = false
    /// 升级菜单状态机(abb 同款):idle 未查 / checking 检查中 / latest 已最新 /
    /// available 有新版本 / installing 安装中 / failed 失败(点击重查)。
    var updateState = UpdateState.idle
    var sourceAvailableSha = ""
    var releaseInfo: ReleaseInfo?
    var lastNotifiedKey = ""
    var busyNote = ""

    func applicationDidFinishLaunching(_ n: Notification) {
        NSApp.setActivationPolicy(.regular)   // Dock 驻留 + 菜单栏双驻留
        NSApp.applicationIconImage = renderDockIcon(256)
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        statusItem.button?.image = makeIcon(color: .systemYellow)
        statusItem.button?.title = ""
        let menu = NSMenu()
        menu.delegate = self
        statusItem.menu = menu
        rebuildMenu()
        bootstrapDeploy()
        pollTimer = Timer.scheduledTimer(withTimeInterval: 5, repeats: true) { _ in self.poll() }
        poll()
        // 启动 30 秒后做一次升级检查;此后每 30 分钟。
        DispatchQueue.global().asyncAfter(deadline: .now() + 30) { self.autoCheck() }
        autoTimer = Timer.scheduledTimer(withTimeInterval: 1800, repeats: true) { _ in
            DispatchQueue.global().async { self.autoCheck() }
        }
        logLine("tray launched")
    }

    /// Dock 图标:深色圆角方 + 绿色品牌标
    func renderDockIcon(_ side: CGFloat) -> NSImage {
        let img = NSImage(size: NSSize(width: side, height: side))
        img.lockFocus()
        let s = side / 256.0
        NSColor(calibratedRed: 0.10, green: 0.11, blue: 0.13, alpha: 1).setFill()
        NSBezierPath(roundedRect: NSRect(x: 0, y: 0, width: side, height: side),
                     xRadius: 58*s, yRadius: 58*s).fill()
        let green = NSColor(calibratedRed: 0.18, green: 0.63, blue: 0.26, alpha: 1)
        green.setStroke(); green.setFill()
        func dot(_ cx: CGFloat, _ cy: CGFloat, _ r: CGFloat) {
            NSBezierPath(ovalIn: NSRect(x: (cx-r)*s, y: (cy-r)*s, width: 2*r*s, height: 2*r*s)).fill()
        }
        dot(78, 200, 17); dot(186, 200, 17)
        let p = NSBezierPath()
        p.move(to: NSPoint(x: 78*s, y: 180*s))
        p.line(to: NSPoint(x: 78*s, y: 88*s))
        p.lineWidth = 16*s; p.lineCapStyle = .round; p.stroke()
        let b = NSBezierPath()
        b.move(to: NSPoint(x: 186*s, y: 180*s))
        b.curve(to: NSPoint(x: 78*s, y: 138*s),
                controlPoint1: NSPoint(x: 186*s, y: 152*s),
                controlPoint2: NSPoint(x: 78*s, y: 152*s))
        b.lineWidth = 16*s; b.lineCapStyle = .round; b.stroke()
        for y in [22.0, 50.0] {
            NSBezierPath(roundedRect: NSRect(x: 52*s, y: y*s, width: 152*s, height: 24*s),
                         xRadius: 12*s, yRadius: 12*s).fill()
        }
        img.unlockFocus()
        return img
    }

    /// 点击 Dock 图标 = 打开 Web UI
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        if !flag { openWeb() }   // 点击 Dock = 直接打开 Web UI(与菜单一致)
        return true
    }

    // MARK: - 状态轮询

    func poll() {
        var req = URLRequest(url: healthURL)
        req.timeoutInterval = 3
        URLSession.shared.dataTask(with: req) { data, resp, _ in
            DispatchQueue.main.async {
                if let http = resp as? HTTPURLResponse, http.statusCode == 200,
                   let d = data,
                   let obj = try? JSONSerialization.jsonObject(with: d) as? [String: Any] {
                    self.serviceState = "running"
                    self.serviceVersion = (obj["version"] as? String) ?? ""
                } else {
                    self.serviceState = "stopped"
                    self.serviceVersion = ""
                }
                self.refreshButton()
            }
        }.resume()
    }

    /// walgit 品牌图标:两个提交点经弧线汇入干线,落入两层「桶」板。
    /// 分状态着色(abb 同款思路):运行绿 / 升级橙 / 切换黄 / 停止灰。
    func makeIcon(color: NSColor) -> NSImage {
        let side: CGFloat = 18
        let img = NSImage(size: NSSize(width: side, height: side))
        img.lockFocus()
        color.setStroke()
        color.setFill()
        let s = side / 256.0
        func dot(_ cx: CGFloat, _ cy: CGFloat, _ r: CGFloat) {
            NSBezierPath(ovalIn: NSRect(x: (cx-r)*s, y: (cy-r)*s, width: 2*r*s, height: 2*r*s)).fill()
        }
        dot(78, 200, 17); dot(186, 200, 17)
        let p = NSBezierPath()
        p.move(to: NSPoint(x: 78*s, y: 180*s))
        p.line(to: NSPoint(x: 78*s, y: 88*s))
        p.lineWidth = 15*s; p.lineCapStyle = .round; p.stroke()
        let b = NSBezierPath()
        b.move(to: NSPoint(x: 186*s, y: 180*s))
        b.curve(to: NSPoint(x: 78*s, y: 138*s),
                controlPoint1: NSPoint(x: 186*s, y: 152*s),
                controlPoint2: NSPoint(x: 78*s, y: 152*s))
        b.lineWidth = 15*s; b.lineCapStyle = .round; b.stroke()
        for y in [22.0, 50.0] {
            NSBezierPath(roundedRect: NSRect(x: 52*s, y: y*s, width: 152*s, height: 24*s),
                         xRadius: 12*s, yRadius: 12*s).fill()
        }
        img.unlockFocus()
        return img
    }

    func refreshButton() {
        let color: NSColor
        if updateState == .installing { color = .systemOrange }
        else if transitioning { color = .systemYellow }
        else if serviceState == "running" { color = .systemGreen }
        else { color = .systemGray }
        statusItem.button?.image = makeIcon(color: color)
        statusItem.button?.title = ""
        rebuildMenu()
    }

    func menuNeedsUpdate(_ menu: NSMenu) { rebuildMenu() }

    func rebuildMenu() {
        let menu = statusItem.menu!
        menu.removeAllItems()

        let stateText: String
        // 运行层警示(#73):**有意选择**的 memory 后端数据不落盘——状态行显式
        // 标注。未配置态(无 intentional 标志)由 D43 向导接管,不显示。
        let cfg = deployConfig()
        let backendNote = cfg.backend == "memory" && cfg.memoryIntentional ? " · 内存后端(数据不落盘)" : ""
        switch serviceState {
        case "running": stateText = "运行中 \(serviceVersion.isEmpty ? "" : "· \(serviceVersion)")\(backendNote)"
        case "stopped": stateText = "已停止\(backendNote)"
        default: stateText = "检查中…"
        }
        let head = NSMenuItem(title: "walgit 服务:\(stateText)", action: nil, keyEquivalent: "")
        head.isEnabled = false
        menu.addItem(head)

        if transitioning {
            let t = NSMenuItem(title: "切换中…", action: nil, keyEquivalent: "")
            t.isEnabled = false
            menu.addItem(t)
        } else if serviceState == "running" {
            menu.addItem(NSMenuItem(title: "停止服务", action: #selector(stopService), keyEquivalent: "s"))
        } else {
            menu.addItem(NSMenuItem(title: "启动服务", action: #selector(startService), keyEquivalent: "r"))
        }

        menu.addItem(.separator())

        let title = upgradeLine(state: updateState, appVersion: installedAppVersion(),
                                serviceVersion: serviceVersion, release: releaseInfo,
                                sourceSha: sourceAvailableSha, busyNote: busyNote)
        let up: NSMenuItem
        switch updateState {
        case .idle, .latest:
            up = NSMenuItem(title: title, action: #selector(checkUpdateNow), keyEquivalent: "")
        case .checking, .installing:
            up = NSMenuItem(title: title, action: nil, keyEquivalent: "")
            up.isEnabled = false
        case .available:
            up = NSMenuItem(title: title,
                            action: releaseInfo == nil ? #selector(doUpgradeNow) : #selector(doReleaseUpgrade),
                            keyEquivalent: "")
        case .failed:
            up = NSMenuItem(title: title, action: #selector(openReleases), keyEquivalent: "")
        }
        menu.addItem(up)

        menu.addItem(.separator())
        menu.addItem(NSMenuItem(title: "打开 Web UI", action: #selector(openWeb), keyEquivalent: "w"))
        menu.addItem(.separator())
        let quit = NSMenuItem(title: "退出托盘(服务保持运行)", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        menu.addItem(quit)
    }

    // MARK: - 动作

    @objc func startService() {
        transitioning = true; refreshButton()
        DispatchQueue.global().async {
            let (code, out) = sh("\(serviceCmd("start")) 2>&1")
            logLine("start rc=\(code): \(out.suffix(200))")
            DispatchQueue.main.async {
                self.transitioning = false
                self.poll()
            }
        }
    }

    @objc func stopService() {
        transitioning = true; refreshButton()
        DispatchQueue.global().async {
            let (code, out) = sh("\(serviceCmd("stop")) 2>&1")
            logLine("stop rc=\(code): \(out.suffix(200))")
            DispatchQueue.main.async {
                self.transitioning = false
                self.poll()
            }
        }
    }

    @objc func openWeb() { NSWorkspace.shared.open(webURL) }

    func hasSourceRepo() -> Bool { return hasSourceRepoPath() }

    @objc func openReleases() {
        NSWorkspace.shared.open(URL(string: "https://github.com/gqf2008/walgit-d1/releases")!)
    }

    @objc func checkUpdateNow() {
        updateState = .checking
        refreshButton()
        checkForUpdates(notifyWhenNew: false)
    }

    @objc func doUpgradeNow() {
        guard !sourceAvailableSha.isEmpty else { openReleases(); return }
        updateState = .installing; busyNote = ""; refreshButton()
        DispatchQueue.global().async { self.upgradePipeline() }
    }

    @objc func doReleaseUpgrade() {
        guard let release = releaseInfo else { openReleases(); return }
        updateState = .installing
        busyNote = "下载中…"
        refreshButton()
        notify("walgit 正在升级", "下载 v\(release.version) 安装包")
        DispatchQueue.global().async {
            do {
                try self.releaseUpgrade(release)
            } catch {
                DispatchQueue.main.async {
                    self.updateState = .failed
                    self.busyNote = ""
                    self.refreshButton()
                }
                self.notify("walgit 升级失败", "\(error)")
                logLine("release: upgrade failed: \(error)")
            }
        }
    }

    func autoCheck() {
        guard updateState != .installing, updateState != .checking, !transitioning else { return }
        guard updateState == .idle || updateState == .latest || updateState == .failed else { return }
        checkForUpdates(notifyWhenNew: true)
    }

    private func checkForUpdates(notifyWhenNew: Bool) {
        DispatchQueue.global().async {
            let current = installedAppVersion()
            let release = latestRelease()
            let releaseNewer = release.map { isVersionNewer($0.version, than: current) } ?? false
            let sourceSha = sourceUpdateSha()
            DispatchQueue.main.async {
                self.releaseInfo = releaseNewer ? release : nil
                self.sourceAvailableSha = self.releaseInfo == nil ? (sourceSha ?? "") : ""
                if let release = self.releaseInfo {
                    self.updateState = .available
                    let key = "release:\(release.version)"
                    if notifyWhenNew && self.lastNotifiedKey != key {
                        self.lastNotifiedKey = key
                        self.notify("walgit 发现新版本", "v\(release.version) — 点托盘菜单下载升级")
                    }
                } else if let sourceSha = sourceSha {
                    self.updateState = .available
                    let key = "source:\(sourceSha)"
                    if notifyWhenNew && self.lastNotifiedKey != key {
                        self.lastNotifiedKey = key
                        self.notify("walgit 发现新源码", "\(sourceSha) — 点托盘菜单升级")
                    }
                } else {
                    self.updateState = .latest
                }
                self.refreshButton()
            }
            logLine("detect: installed=\(current) release=\(release.map { "v\($0.version)" } ?? "none") source=\(sourceSha ?? "none")")
        }
    }

    private func releaseUpgrade(_ release: ReleaseInfo) throws {
        let cache = NSHomeDirectory() + "/Library/Caches/walgit"
        try FileManager.default.createDirectory(atPath: cache, withIntermediateDirectories: true)
        let dmg = "\(cache)/\(release.asset.name)"
        try? FileManager.default.removeItem(atPath: dmg)
        try download(release.asset.url, to: URL(fileURLWithPath: dmg))
        let (hashCode, hashOut) = sh("shasum -a 256 '\(dmg)'")
        let gotHash = hashOut.split(whereSeparator: { $0 == " " || $0 == "\t" }).first
            .map { String($0).lowercased() } ?? ""
        guard hashCode == 0, gotHash == release.asset.sha256.lowercased() else {
            throw NSError(domain: "walgit-release", code: 1,
                          userInfo: [NSLocalizedDescriptionKey: "SHA-256 校验失败"])
        }
        let mount = "\(cache)/mount-\(release.version)-$PID"
        try? FileManager.default.removeItem(atPath: mount)
        try FileManager.default.createDirectory(atPath: mount, withIntermediateDirectories: true)
        let (attachCode, attachOut) = sh("hdiutil attach -nobrowse -readonly -mountpoint '\(mount)' '\(dmg)'")
        guard attachCode == 0 else { throw NSError(domain: "walgit-release", code: 2,
            userInfo: [NSLocalizedDescriptionKey: "挂载 DMG 失败: \(attachOut.suffix(200))"]) }
        do {
            let staged = "\(mount)/walgit-tray.app"
            let (plistCode, plistOut) = sh("/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' '\(staged)/Contents/Info.plist'")
            guard plistCode == 0, plistOut.trimmingCharacters(in: .whitespacesAndNewlines) == release.version else {
                throw NSError(domain: "walgit-release", code: 3,
                              userInfo: [NSLocalizedDescriptionKey: "DMG 内 app 版本不匹配"])
            }
            let (signCode, signOut) = sh("codesign --verify --deep --strict '\(staged)' && spctl --assess --type execute '\(staged)'")
            guard signCode == 0 else { throw NSError(domain: "walgit-release", code: 4,
                userInfo: [NSLocalizedDescriptionKey: "DMG 内 app 签名校验失败: \(signOut.suffix(200))"]) }
            let script = Bundle.main.resourceURL!.appendingPathComponent("release-install.sh").path
            guard FileManager.default.fileExists(atPath: script) else {
                throw NSError(domain: "walgit-release", code: 5,
                              userInfo: [NSLocalizedDescriptionKey: "缺少 release-install.sh"])
            }
            let log = FileHandle(forWritingAtPath: logPath) ?? {
                FileManager.default.createFile(atPath: logPath, contents: nil)
                return FileHandle(forWritingAtPath: logPath)!
            }()
            log.seekToEndOfFile()
            let proc = Process()
            proc.executableURL = URL(fileURLWithPath: "/bin/bash")
            proc.arguments = [script, dmg, mount, Bundle.main.bundlePath, release.version, String(ProcessInfo.processInfo.processIdentifier)]
            var env = ProcessInfo.processInfo.environment
            env["WALGIT_DEPLOY_DIR"] = deployDir
            proc.environment = env
            proc.standardOutput = log
            proc.standardError = log
            proc.standardInput = FileHandle.nullDevice
            try proc.run()
            logLine("release: updater spawned for v\(release.version)")
            DispatchQueue.main.async { NSApp.terminate(nil) }
        } catch {
            _ = sh("hdiutil detach '\(mount)' >/dev/null 2>&1 || true")
            throw error
        }
    }

    private func download(_ url: URL, to destination: URL) throws {
        var request = URLRequest(url: url)
        request.timeoutInterval = 120
        // 全部落在 completion 内(无跨线程共享变量):成功搬到 destination,
        // 失败/超时由文件是否存在判定,超时同时取消任务。
        try? FileManager.default.removeItem(at: destination)
        let semaphore = DispatchSemaphore(value: 0)
        let task = URLSession.shared.downloadTask(with: request) { temp, response, error in
            defer { semaphore.signal() }
            guard error == nil,
                  let http = response as? HTTPURLResponse, http.statusCode == 200,
                  let temp
            else { return }
            try? FileManager.default.removeItem(at: destination)
            try? FileManager.default.moveItem(at: temp, to: destination)
        }
        task.resume()
        guard semaphore.wait(timeout: .now() + 180) == .success else {
            task.cancel()
            throw NSError(domain: "walgit-release", code: 8,
                          userInfo: [NSLocalizedDescriptionKey: "下载超时"])
        }
        guard FileManager.default.fileExists(atPath: destination.path) else {
            throw NSError(domain: "walgit-release", code: 7,
                          userInfo: [NSLocalizedDescriptionKey: "下载失败"])
        }
    }

    func repoPath() -> String {
        let custom = UserDefaults.standard.string(forKey: "repoPath")
        return custom ?? "/Volumes/Workspace/GitHub/walgit"
    }

    /// 升级管线(仅由用户点击触发):fetch → ff-merge main → 构建 → 备份 →
    /// 停 → 换 → 起 → 健康验证,失败回滚。
    func upgradePipeline() {
        defer {
            DispatchQueue.main.async {
                self.refreshButton()
                self.poll()
            }
        }
        let repo = repoPath()
        let bin = "\(deployDir)/walgit"
        let note: (String) -> Void = { m in
            DispatchQueue.main.async { self.busyNote = m; self.refreshButton() }
        }

        // 0. 对齐到 origin/main(不快进就不构建——否则会重建旧版本)。
        note("对齐 main…")
        _ = sh("git -C '\(repo)' fetch origin main 2>&1")
        let (mc, mout) = sh("cd '\(repo)' && git merge --ff-only origin/main 2>&1")
        guard mc == 0 else {
            logLine("upgrade: ff-merge FAILED \(mout.suffix(300))")
            note("main 无法快进(本地有分叉?),见 tray.log")
            notify("walgit 升级中止", "本地 main 无法快进到 origin/main")
            return
        }

        logLine("upgrade: build begin")
        note("构建中…")
        let (bc, bout) = sh("cd '\(repo)' && RUSTUP_TOOLCHAIN=1.98.0 cargo build --release -p walgit-cli 2>&1")
        guard bc == 0 else {
            logLine("upgrade: build FAILED \(bout.suffix(400))")
            note("构建失败(见 tray.log)")
            notify("walgit 升级失败", "cargo 构建失败,旧版本继续运行")
            return
        }
        let (_, shaOut) = sh("cd '\(repo)' && git rev-parse --short=7 HEAD")
        let sha = shaOut.trimmingCharacters(in: .whitespacesAndNewlines)

        note("换装中…")
        _ = sh("cp '\(bin)' '\(bin).bak-tray'")
        let (sc, sout) = sh("\(serviceCmd("stop")) 2>&1")
        logLine("upgrade: stop rc=\(sc) \(sout.suffix(120))")
        _ = sh("cp '\(repo)/target/release/walgit' '\(bin)'")
        let (rc, rout) = sh("\(serviceCmd("start")) 2>&1")
        logLine("upgrade: start rc=\(rc) \(rout.suffix(120))")

        // 健康验证 ≤15s,失败回滚——与探活同源的地址(#115 审查修正:
        // 硬编码 8081 会让改端口后的升级在验证步恒失败、误回滚)。
        let health = "http://\(deployConfig().listen)/healthz"
        var ok = false
        for _ in 0..<15 {
            let (hc, hout) = sh("curl -sf --max-time 2 \(health) 2>&1 || true")
            if hc == 0, hout.contains("ok"), hout.contains(sha) { ok = true; break }
            sleep(1)
        }
        if ok {
            logLine("upgrade: success \(sha)")
            DispatchQueue.main.async {
                self.updateState = .latest
                self.refreshButton()
            }
            notify("walgit 已升级", "版本 \(sha),服务已重启")
        } else {
            logLine("upgrade: health FAIL — rollback")
            _ = sh("cp '\(bin).bak-tray' '\(bin)'")
            _ = sh("\(serviceCmd("restart")) 2>&1")
            DispatchQueue.main.async { self.updateState = .failed }
            notify("walgit 升级失败", "健康检查未过,已回滚旧版本")
        }
    }

    func notify(_ title: String, _ body: String) {
        let t = title.replacingOccurrences(of: "\"", with: "'")
        let b = body.replacingOccurrences(of: "\"", with: "'")
        _ = sh("osascript -e 'display notification \"\(b)\" with title \"\(t)\"' 2>&1")
    }
}

@main
struct WalgitTrayMain {
    static func main() {
        // 测试钩子:只跑部署骨架 bootstrap 后退出(不启动 NSApplication)。
        if ProcessInfo.processInfo.environment["WALGIT_BOOTSTRAP_ONLY"] == "1" {
            let done = DispatchSemaphore(value: 0)
            bootstrapDeploy(onServiceRestart: { done.signal() })
            _ = done.wait(timeout: .now() + 20)
            exit(0)
        }
        // 测试钩子:解析一段 /healthz JSON(验证 version 取值的精确性)。
        if let body = ProcessInfo.processInfo.environment["WALGIT_HEALTH_TEST"] {
            print(healthVersion(body))
            exit(0)
        }
        // 测试钩子:只打印菜单 upgrade 行(验证版本语义,不启动 NSApplication)。
        if let appV = ProcessInfo.processInfo.environment["WALGIT_MENU_TEST"] {
            let svcV = ProcessInfo.processInfo.environment["WALGIT_MENU_SERVICE"] ?? ""
            let state = ProcessInfo.processInfo.environment["WALGIT_MENU_STATE"] ?? "latest"
            let releaseV = ProcessInfo.processInfo.environment["WALGIT_MENU_RELEASE"]
            let release = releaseV.flatMap { v -> ReleaseInfo? in
                guard let url = URL(string: "https://example.invalid/x.dmg") else { return nil }
                return ReleaseInfo(tag: "v\(v)", version: v,
                                   asset: ReleaseAsset(name: "walgit-\(v)-arm64.dmg", url: url,
                                                       sha256: String(repeating: "a", count: 64)))
            }
            let st: UpdateState
            switch state {
            case "available": st = .available
            case "checking": st = .checking
            case "installing": st = .installing
            case "failed": st = .failed
            case "latest": st = .latest
            default: st = .idle
            }
            print(upgradeLine(state: st, appVersion: appV, serviceVersion: svcV,
                              release: release,
                              sourceSha: ProcessInfo.processInfo.environment["WALGIT_MENU_SOURCE"] ?? "",
                              busyNote: ProcessInfo.processInfo.environment["WALGIT_MENU_BUSY"] ?? ""))
            exit(0)
        }
        let app = NSApplication.shared
        let delegate = AppDelegate()
        app.delegate = delegate
        app.run()
    }
}
