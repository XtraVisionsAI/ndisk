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
                NSWorkspace.shared.open(url)
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
            let s = try await openSession(configToml: toml, cacheRoot: tmp.path, secret: secret)
            defer { s.close() }
            return .success(try await s.list(dir: NdiskIdentifier.rootId, force: true).count)
        } catch {
            return .failure(error)
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
