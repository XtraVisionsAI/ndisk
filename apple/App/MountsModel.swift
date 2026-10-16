import AppKit
import FileProvider
import Foundation
import NdiskKit
import os
import ServiceManagement

/// 管理挂载登记、全局设置与 File Provider domain。
///
/// App 不为已登记的挂载打开会话：同一挂载的会话只由扩展持有，避免两个写回队列重复上传。
/// 测试连接时使用独立的临时目录。
@MainActor
final class MountsModel: ObservableObject {
    @Published private(set) var mounts: [SharedStore.Mount] = []
    @Published private(set) var statuses: [String: SharedStore.MountStatus] = [:]
    @Published private(set) var disconnected: Set<String> = []
    @Published var lastError: String?

    let coreVersion = NdiskKit.coreVersion()
    private let log = Logger(subsystem: "ai.xtravisions.ndisk", category: "App")
    let store: SharedStore?
    private var ticker: Timer?

    /// 唤醒扩展工作集的间隔：驱动远端变更上报与 TTL 驱逐。
    static let signalInterval: TimeInterval = 10 * 60

    init() {
        initLogging(filter: "info")
        do {
            store = try SharedStore.fromBundle()
            mounts = store?.mounts() ?? []
        } catch {
            store = nil
            lastError = "\(error)"
        }
        ticker = Timer.scheduledTimer(withTimeInterval: Self.signalInterval, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.signalAll() }
        }
        Task { await refreshDomains() }
    }

    private func domain(_ m: SharedStore.Mount) -> NSFileProviderDomain {
        NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(m.id), displayName: m.displayName)
    }

    // MARK: 挂载增删改

    /// 新建或保存挂载。编辑时 `secret` 为空表示保留原密钥。
    func save(spec: S3MountSpec, secret: String?) async {
        guard let store else { return }
        do {
            let toml = try renderS3Config(spec: spec)
            let mount = SharedStore.Mount(id: spec.id, displayName: spec.displayName)
            try store.save(mount: mount, configToml: toml, secret: secret)
            // 同 id 再次 add 会更新显示名；首次 add 注册 domain。
            try await NSFileProviderManager.add(domain(mount))
            mounts = store.mounts()
            lastError = nil
            // 扩展在下个轮询周期检测到配置变化后自行重开会话；这里顺手唤醒一次。
            try? await NSFileProviderManager(for: domain(mount))?.signalEnumerator(for: .workingSet)
        } catch {
            lastError = "保存失败：\(error)"
        }
    }

    func existingSpec(_ m: SharedStore.Mount) -> S3MountSpec? {
        guard let store, let toml = try? String(contentsOf: store.configURL(m.id), encoding: .utf8) else { return nil }
        return try? parseS3Config(configToml: toml)
    }

    func remove(_ m: SharedStore.Mount) {
        Task {
            do {
                try await NSFileProviderManager.remove(domain(m))
                try store?.remove(mountId: m.id)
                mounts = store?.mounts() ?? []
                disconnected.remove(m.id)
            } catch {
                lastError = "移除失败：\(error)"
            }
        }
    }

    // MARK: 断开 / 连接

    func refreshDomains() async {
        guard let domains = try? await NSFileProviderManager.domains() else { return }
        disconnected = Set(domains.filter(\.isDisconnected).map(\.identifier.rawValue))
    }

    func toggleConnection(_ m: SharedStore.Mount) {
        Task {
            guard let manager = NSFileProviderManager(for: domain(m)) else { return }
            do {
                if disconnected.contains(m.id) {
                    try await manager.reconnect()
                } else {
                    try await manager.disconnect(reason: "已由 NDisk 断开", options: [])
                }
                await refreshDomains()
            } catch {
                lastError = "切换连接失败：\(error)"
            }
        }
    }

    func reveal(_ m: SharedStore.Mount) {
        Task {
            do {
                guard let manager = NSFileProviderManager(for: domain(m)) else { return }
                let url = try await manager.getUserVisibleURL(for: .rootContainer)
                // 沙盒 App 对 CloudStorage 路径没有文件访问权，请 Finder 自行打开。
                if !NSWorkspace.shared.selectFile(nil, inFileViewerRootedAtPath: url.path) {
                    NSWorkspace.shared.activateFileViewerSelecting([url])
                }
            } catch {
                lastError = "无法打开：\(error)"
            }
        }
    }

    /// 用临时目录打开一次会话并列出根目录，验证配置与凭据。
    func testConnection(spec: S3MountSpec, secret: String?) async -> Result<Int, Error> {
        let tmp = FileManager.default.temporaryDirectory.appendingPathComponent("ndisk-test-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: tmp) }
        do {
            let toml = try renderS3Config(spec: spec)
            // 编辑时未重新输入密钥：用钥匙串里的现有密钥测试。
            let effectiveSecret = (secret?.isEmpty ?? true) ? store?.secret(for: spec.id) : secret
            let s = try await openSession(configToml: toml, settingsToml: store?.settingsToml(), mountCount: 1,
                                          dataDir: tmp.appendingPathComponent("data").path,
                                          cacheDir: tmp.appendingPathComponent("cache").path, secret: effectiveSecret)
            defer { s.close() }
            return .success(try await s.list(dir: NdiskIdentifier.rootId, force: true).count)
        } catch {
            return .failure(error)
        }
    }

    // MARK: 全局设置

    func settings() -> SettingsSpec {
        if let toml = store?.settingsToml(), let s = try? parseSettings(settingsToml: toml) { return s }
        return defaultSettings()
    }

    func saveSettings(_ spec: SettingsSpec) async {
        guard let store else { return }
        do {
            try store.saveSettings(toml: try renderSettings(spec: spec))
            lastError = nil
            signalAll() // 扩展在下个周期检测到设置变化后重开会话
        } catch {
            lastError = "保存设置失败：\(error)"
        }
    }

    var launchAtLogin: Bool {
        get { SMAppService.mainApp.status == .enabled }
        set {
            do {
                if newValue { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }
            } catch {
                lastError = "设置开机启动失败：\(error)"
            }
            objectWillChange.send()
        }
    }

    /// 菜单打开时调用：读取扩展写出的状态文件与 domain 连接状态。
    func refreshStatuses() {
        guard let store else { return }
        var out: [String: SharedStore.MountStatus] = [:]
        for m in mounts { if let st = store.readStatus(m.id) { out[m.id] = st } }
        statuses = out
        Task { await refreshDomains() }
    }

    /// 处理 `ndisk://` URL（开发自动化与脚本使用）：
    ///   ndisk://add?id=…&name=…&bucket=…&endpoint=…&access_key=…&secret=…&ttl=3600&path_style=1&region=…&prefix=…
    ///   ndisk://remove?id=…   ndisk://signal?id=…
    func handle(url: URL) async {
        guard let comps = URLComponents(url: url, resolvingAgainstBaseURL: false) else { return }
        var q: [String: String] = [:]
        for item in comps.queryItems ?? [] { q[item.name] = item.value ?? "" }
        switch comps.host {
        case "add":
            guard let id = q["id"], let bucket = q["bucket"] else { lastError = "ndisk://add 缺少 id 或 bucket"; return }
            let auth: S3AuthSpec = (q["access_key"]?.isEmpty ?? true) ? .default : .static(accessKeyId: q["access_key"]!)
            let spec = S3MountSpec(
                id: id, displayName: q["name"] ?? id, bucket: bucket, prefix: q["prefix"] ?? "",
                region: q["region"] ?? "us-east-1", endpoint: q["endpoint"], forcePathStyle: q["path_style"] == "1",
                auth: auth, policyOverride: q["ttl"].flatMap { UInt64($0) }.map { .ttl(seconds: $0) },
                readOnly: q["read_only"] == "1")
            await save(spec: spec, secret: q["secret"])
            log.info("url add \(id, privacy: .public): \(self.lastError ?? "ok", privacy: .public)")
        case "remove":
            if let id = q["id"], let m = mounts.first(where: { $0.id == id }) { remove(m) }
        case "signal":
            if let id = q["id"], let m = mounts.first(where: { $0.id == id }) {
                try? await NSFileProviderManager(for: domain(m))?.signalEnumerator(for: .workingSet)
                log.info("url signal \(id, privacy: .public)")
            }
        default:
            lastError = "未知 URL：\(url)"
        }
    }

    private func signalAll() {
        for m in mounts {
            NSFileProviderManager(for: domain(m))?.signalEnumerator(for: .workingSet) { [log] error in
                if let error { log.error("signal \(m.id, privacy: .public) failed: \(String(describing: error), privacy: .public)") }
            }
        }
    }
}
