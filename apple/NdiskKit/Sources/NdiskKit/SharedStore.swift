import Foundation
import Security

/// App 与扩展共享的存储布局（位于 App Group 容器内，遵循 macOS 惯例）：
///
///     <group>/Library/Application Support/ndisk/mounts.json       已登记的挂载列表
///     <group>/Library/Application Support/ndisk/mounts/<id>.toml  每个挂载的配置
///     <group>/Library/Application Support/ndisk/data/<id>/        元数据库、未上传文件（持久）
///     <group>/Library/Application Support/ndisk/status/<id>.json  扩展写出的状态
///     <group>/Library/Caches/ndisk/<id>/                          块缓存与导出暂存（系统可清理）
///
/// 密钥存放在共享钥匙串访问组中，配置文件里只有引用名 `keychain:<id>`。
public struct SharedStore: Sendable {
    public let root: URL
    public let keychainAccessGroup: String?

    /// - Parameters:
    ///   - appGroup: App Group 标识，如 `group.ai.xtravisions.ndisk`；Info.plist 中的 `NdiskAppGroup`。
    ///   - keychainAccessGroup: 共享钥匙串访问组（含 Team ID 前缀）；为 nil 时使用默认组。
    public init(appGroup: String, keychainAccessGroup: String? = nil) throws {
        guard let url = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroup) else {
            throw NdiskError.Config(message: "App Group \(appGroup) is not available; check entitlements")
        }
        self.init(root: url, keychainAccessGroup: keychainAccessGroup)
    }

    public init(root: URL, keychainAccessGroup: String? = nil) {
        self.root = root
        self.keychainAccessGroup = keychainAccessGroup
    }

    /// 从 Info.plist 读取 `NdiskAppGroup` 与 `NdiskKeychainGroup`。
    public static func fromBundle(_ bundle: Bundle = .main) throws -> SharedStore {
        guard let group = bundle.object(forInfoDictionaryKey: "NdiskAppGroup") as? String, !group.isEmpty else {
            throw NdiskError.Config(message: "NdiskAppGroup missing from Info.plist")
        }
        let kc = bundle.object(forInfoDictionaryKey: "NdiskKeychainGroup") as? String
        return try SharedStore(appGroup: group, keychainAccessGroup: (kc?.isEmpty ?? true) ? nil : kc)
    }

    // MARK: 挂载登记

    public struct Mount: Codable, Hashable, Identifiable, Sendable {
        public var id: String
        public var displayName: String
        public init(id: String, displayName: String) {
            self.id = id
            self.displayName = displayName
        }
    }

    var supportDir: URL { root.appendingPathComponent("Library/Application Support/ndisk", isDirectory: true) }
    var cachesDir: URL { root.appendingPathComponent("Library/Caches/ndisk", isDirectory: true) }
    var mountsFile: URL { supportDir.appendingPathComponent("mounts.json") }
    public func configURL(_ id: String) -> URL { supportDir.appendingPathComponent("mounts/\(id).toml") }
    public func dataURL(_ id: String) -> URL { supportDir.appendingPathComponent("data/\(id)", isDirectory: true) }
    public func cacheURL(_ id: String) -> URL { cachesDir.appendingPathComponent(id, isDirectory: true) }

    public func mounts() -> [Mount] {
        migrateLegacyLayout()
        guard let data = try? Data(contentsOf: mountsFile) else { return [] }
        return (try? JSONDecoder().decode([Mount].self, from: data)) ?? []
    }

    public func save(mount: Mount, configToml: String, secret: String?) throws {
        let fm = FileManager.default
        try fm.createDirectory(at: configURL(mount.id).deletingLastPathComponent(), withIntermediateDirectories: true)
        try configToml.write(to: configURL(mount.id), atomically: true, encoding: .utf8)
        if let secret, !secret.isEmpty { try setSecret(secret, for: mount.id) }
        try fm.createDirectory(at: supportDir, withIntermediateDirectories: true)
        var all = mounts().filter { $0.id != mount.id }
        all.append(mount)
        try JSONEncoder().encode(all).write(to: mountsFile, options: .atomic)
    }

    public func remove(mountId: String) throws {
        let all = mounts().filter { $0.id != mountId }
        try JSONEncoder().encode(all).write(to: mountsFile, options: .atomic)
        try? FileManager.default.removeItem(at: configURL(mountId))
        try? FileManager.default.removeItem(at: cacheURL(mountId))
        try? FileManager.default.removeItem(at: dataURL(mountId))
        try? FileManager.default.removeItem(at: statusURL(mountId))
        deleteSecret(for: mountId)
    }

    /// 旧布局（<group>/mounts*, <group>/cache/<id>）一次性迁移到新布局。开发期产物，可在首个正式版后删除。
    func migrateLegacyLayout() {
        let fm = FileManager.default
        let oldMounts = root.appendingPathComponent("mounts.json")
        if fm.fileExists(atPath: oldMounts.path), !fm.fileExists(atPath: mountsFile.path) {
            try? fm.createDirectory(at: supportDir, withIntermediateDirectories: true)
            try? fm.moveItem(at: oldMounts, to: mountsFile)
            try? fm.moveItem(at: root.appendingPathComponent("mounts"), to: supportDir.appendingPathComponent("mounts"))
            try? fm.moveItem(at: root.appendingPathComponent("status"), to: supportDir.appendingPathComponent("status"))
        }
        let oldCache = root.appendingPathComponent("cache")
        guard let ids = try? fm.contentsOfDirectory(atPath: oldCache.path) else { return }
        for id in ids where !fm.fileExists(atPath: dataURL(id).path) {
            let old = oldCache.appendingPathComponent(id)
            try? fm.createDirectory(at: dataURL(id), withIntermediateDirectories: true)
            try? fm.createDirectory(at: cacheURL(id), withIntermediateDirectories: true)
            for f in ["meta.sqlite", "meta.sqlite-wal", "meta.sqlite-shm", "dirty"] {
                try? fm.moveItem(at: old.appendingPathComponent(f), to: dataURL(id).appendingPathComponent(f))
            }
            for f in ["blocks", "staging"] {
                try? fm.moveItem(at: old.appendingPathComponent(f), to: cacheURL(id).appendingPathComponent(f))
            }
            try? fm.removeItem(at: old)
        }
        try? fm.removeItem(at: oldCache)
    }

    /// 打开挂载会话（扩展与 App 都可调用）。
    public func openSession(mountId: String) async throws -> Session {
        migrateLegacyLayout()
        let toml = try String(contentsOf: configURL(mountId), encoding: .utf8)
        try FileManager.default.createDirectory(at: dataURL(mountId), withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: cacheURL(mountId), withIntermediateDirectories: true)
        return try await NdiskKit.openSession(configToml: toml, dataDir: dataURL(mountId).path,
                                              cacheDir: cacheURL(mountId).path, secret: secret(for: mountId))
    }

    // MARK: 状态文件（扩展写、App 读）

    /// 扩展周期性写出的运行状态，App 菜单据此展示。
    public struct MountStatus: Codable, Sendable {
        public var mountId: String
        public var backend: String
        public var cachedBytes: UInt64
        public var pendingUploads: UInt64
        public var materializedItems: UInt64
        public var lastRefreshChanges: UInt64
        public var lastError: String?
        public var updatedAt: Date
        public init(mountId: String, backend: String, cachedBytes: UInt64, pendingUploads: UInt64,
                    materializedItems: UInt64, lastRefreshChanges: UInt64, lastError: String?, updatedAt: Date) {
            self.mountId = mountId; self.backend = backend; self.cachedBytes = cachedBytes
            self.pendingUploads = pendingUploads; self.materializedItems = materializedItems
            self.lastRefreshChanges = lastRefreshChanges; self.lastError = lastError; self.updatedAt = updatedAt
        }
    }

    public func statusURL(_ id: String) -> URL { supportDir.appendingPathComponent("status/\(id).json") }

    public func writeStatus(_ st: MountStatus) throws {
        try FileManager.default.createDirectory(at: statusURL(st.mountId).deletingLastPathComponent(), withIntermediateDirectories: true)
        let enc = JSONEncoder(); enc.dateEncodingStrategy = .iso8601
        try enc.encode(st).write(to: statusURL(st.mountId), options: .atomic)
    }

    public func readStatus(_ id: String) -> MountStatus? {
        guard let data = try? Data(contentsOf: statusURL(id)) else { return nil }
        let dec = JSONDecoder(); dec.dateDecodingStrategy = .iso8601
        return try? dec.decode(MountStatus.self, from: data)
    }

    // MARK: 钥匙串

    static let service = "ai.xtravisions.ndisk.secret"

    private func baseQuery(_ id: String) -> [String: Any] {
        var q: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: Self.service,
            kSecAttrAccount as String: id,
            kSecUseDataProtectionKeychain as String: true,
        ]
        if let g = keychainAccessGroup { q[kSecAttrAccessGroup as String] = g }
        return q
    }

    public func setSecret(_ secret: String, for id: String) throws {
        deleteSecret(for: id)
        var q = baseQuery(id)
        q[kSecValueData as String] = Data(secret.utf8)
        q[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
        let status = SecItemAdd(q as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw NdiskError.Config(message: "keychain write failed: \(status)")
        }
    }

    public func secret(for id: String) -> String? {
        var q = baseQuery(id)
        q[kSecReturnData as String] = true
        q[kSecMatchLimit as String] = kSecMatchLimitOne
        var out: CFTypeRef?
        guard SecItemCopyMatching(q as CFDictionary, &out) == errSecSuccess, let data = out as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }

    public func deleteSecret(for id: String) {
        SecItemDelete(baseQuery(id) as CFDictionary)
    }
}
