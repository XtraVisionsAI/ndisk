import AppKit
import FileProvider
import Foundation
import NdiskKit
import os

/// 管理挂载登记与 File Provider domain。
///
/// App 不为已登记的挂载打开会话：同一挂载的会话只由扩展持有，避免两个写回队列重复上传。
/// 测试连接时使用独立的临时缓存目录。
@MainActor
final class MountsModel: ObservableObject {
    @Published private(set) var mounts: [SharedStore.Mount] = []
    @Published private(set) var statuses: [String: SharedStore.MountStatus] = [:]
    @Published var lastError: String?

    let coreVersion = NdiskKit.coreVersion()
    private let log = Logger(subsystem: "ai.xtravisions.ndisk", category: "App")
    private var store: SharedStore?
    private var ticker: Timer?

    /// 唤醒扩展工作集的间隔：驱动远端变更上报与 TTL 驱逐。
    static let signalInterval: TimeInterval = 10 * 60

    init() {
        initLogging(filter: "info")
        do {
            store = try SharedStore.fromBundle()
            mounts = store?.mounts() ?? []
        } catch {
            lastError = "\(error)"
        }
        ticker = Timer.scheduledTimer(withTimeInterval: Self.signalInterval, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.signalAll() }
        }
    }

    private func domain(_ m: SharedStore.Mount) -> NSFileProviderDomain {
        NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(m.id), displayName: m.displayName)
    }

    func add(spec: S3MountSpec, secret: String?) async {
        guard let store else { return }
        do {
            let toml = try renderS3Config(spec: spec)
            let mount = SharedStore.Mount(id: spec.id, displayName: spec.displayName)
            try store.save(mount: mount, configToml: toml, secret: secret)
            try await NSFileProviderManager.add(domain(mount))
            mounts = store.mounts()
            lastError = nil
        } catch {
            lastError = "添加失败：\(error)"
        }
    }

    func remove(_ m: SharedStore.Mount) {
        Task {
            do {
                try await NSFileProviderManager.remove(domain(m))
                try store?.remove(mountId: m.id)
                mounts = store?.mounts() ?? []
            } catch {
                lastError = "移除失败：\(error)"
            }
        }
    }

    func reveal(_ m: SharedStore.Mount) {
        Task {
            do {
                guard let manager = NSFileProviderManager(for: domain(m)) else { return }
                let url = try await manager.getUserVisibleURL(for: .rootContainer)
                // 沙盒 App 对 CloudStorage 路径没有文件访问权，不能 open(url)；
                // 请 Finder 自行打开该位置则不需要权限。
                if !NSWorkspace.shared.selectFile(nil, inFileViewerRootedAtPath: url.path) {
                    NSWorkspace.shared.activateFileViewerSelecting([url])
                }
            } catch {
                lastError = "无法打开：\(error)"
            }
        }
    }

    /// 用临时缓存打开一次会话并列出根目录，验证配置与凭据。
    func testConnection(spec: S3MountSpec, secret: String?) async -> Result<Int, Error> {
        let tmp = FileManager.default.temporaryDirectory.appendingPathComponent("ndisk-test-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: tmp) }
        do {
            let toml = try renderS3Config(spec: spec)
            let s = try await openSession(configToml: toml, dataDir: tmp.appendingPathComponent("data").path,
                                          cacheDir: tmp.appendingPathComponent("cache").path, secret: secret)
            defer { s.close() }
            return .success(try await s.list(dir: NdiskIdentifier.rootId, force: true).count)
        } catch {
            return .failure(error)
        }
    }

    /// 处理 `ndisk://` URL（开发自动化与脚本使用）：
    ///   ndisk://add?id=…&name=…&bucket=…&endpoint=…&access_key=…&secret=…&ttl=3600&path_style=1&region=…&prefix=…
    ///   ndisk://remove?id=…
    func handle(url: URL) async {
        guard let comps = URLComponents(url: url, resolvingAgainstBaseURL: false) else { return }
        var q: [String: String] = [:]
        for item in comps.queryItems ?? [] { q[item.name] = item.value ?? "" }
        switch comps.host {
        case "add":
            guard let id = q["id"], let bucket = q["bucket"] else { lastError = "ndisk://add 缺少 id 或 bucket"; return }
            let spec = S3MountSpec(
                id: id, displayName: q["name"] ?? id, bucket: bucket, prefix: q["prefix"] ?? "",
                region: q["region"] ?? "us-east-1", endpoint: q["endpoint"], forcePathStyle: q["path_style"] == "1",
                accessKeyId: q["access_key"], profile: q["profile"],
                defaultPolicy: .ttl(seconds: UInt64(q["ttl"] ?? "") ?? 7 * 24 * 3600),
                maxCacheBytes: nil, proxy: q["proxy"], uploadLimit: 0, downloadLimit: 0, readOnly: q["read_only"] == "1")
            await add(spec: spec, secret: q["secret"])
            log.info("url add \(id, privacy: .public): \(self.lastError ?? "ok", privacy: .public)")
        case "remove":
            if let id = q["id"], let m = mounts.first(where: { $0.id == id }) { remove(m) }
        case "signal":
            // 立即唤醒工作集（触发变更上报与 TTL 驱逐），调试用。
            if let id = q["id"], let m = mounts.first(where: { $0.id == id }) {
                try? await NSFileProviderManager(for: domain(m))?.signalEnumerator(for: .workingSet)
                log.info("url signal \(id, privacy: .public)")
            }
        default:
            lastError = "未知 URL：\(url)"
        }
    }

    /// 菜单打开时调用：读取扩展写出的状态文件。
    func refreshStatuses() {
        guard let store else { return }
        var out: [String: SharedStore.MountStatus] = [:]
        for m in mounts { if let st = store.readStatus(m.id) { out[m.id] = st } }
        statuses = out
    }

    private func signalAll() {
        for m in mounts {
            NSFileProviderManager(for: domain(m))?.signalEnumerator(for: .workingSet) { [log] error in
                if let error { log.error("signal \(m.id, privacy: .public) failed: \(String(describing: error), privacy: .public)") }
            }
        }
    }
}
