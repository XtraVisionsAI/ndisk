import Foundation
import Security

/// App 与扩展共享的存储布局（位于 App Group 容器内）：
///
///     <group>/mounts.json             已登记的挂载列表
///     <group>/mounts/<id>.toml        每个挂载的配置（由 Rust `renderS3Config` 生成）
///     <group>/cache/<id>/             每个挂载的引擎缓存（SQLite + 块缓存 + staging）
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

    var mountsFile: URL { root.appendingPathComponent("mounts.json") }
    public func configURL(_ id: String) -> URL { root.appendingPathComponent("mounts/\(id).toml") }
    public func cacheURL(_ id: String) -> URL { root.appendingPathComponent("cache/\(id)", isDirectory: true) }

    public func mounts() -> [Mount] {
        guard let data = try? Data(contentsOf: mountsFile) else { return [] }
        return (try? JSONDecoder().decode([Mount].self, from: data)) ?? []
    }

    public func save(mount: Mount, configToml: String, secret: String?) throws {
        let fm = FileManager.default
        try fm.createDirectory(at: configURL(mount.id).deletingLastPathComponent(), withIntermediateDirectories: true)
        try configToml.write(to: configURL(mount.id), atomically: true, encoding: .utf8)
        if let secret, !secret.isEmpty { try setSecret(secret, for: mount.id) }
        var all = mounts().filter { $0.id != mount.id }
        all.append(mount)
        try JSONEncoder().encode(all).write(to: mountsFile, options: .atomic)
    }

    public func remove(mountId: String) throws {
        let all = mounts().filter { $0.id != mountId }
        try JSONEncoder().encode(all).write(to: mountsFile, options: .atomic)
        try? FileManager.default.removeItem(at: configURL(mountId))
        try? FileManager.default.removeItem(at: cacheURL(mountId))
        deleteSecret(for: mountId)
    }

    /// 打开挂载会话（扩展与 App 都可调用）。
    public func openSession(mountId: String) async throws -> Session {
        let toml = try String(contentsOf: configURL(mountId), encoding: .utf8)
        try FileManager.default.createDirectory(at: cacheURL(mountId), withIntermediateDirectories: true)
        return try await NdiskKit.openSession(configToml: toml, cacheRoot: cacheURL(mountId).path, secret: secret(for: mountId))
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
