import FileProvider
import XCTest
@testable import NdiskKit

final class NdiskKitTests: XCTestCase {
    func testIdentifierMapping() {
        XCTAssertEqual(NdiskIdentifier.itemId(.rootContainer), 1)
        XCTAssertEqual(NdiskIdentifier.identifier(1), .rootContainer)
        XCTAssertEqual(NdiskIdentifier.itemId(NdiskIdentifier.identifier(42)), 42)
        XCTAssertNil(NdiskIdentifier.itemId(.workingSet))
    }

    func testRenderConfigAndVersion() throws {
        XCTAssertFalse(coreVersion().isEmpty)
        let toml = try renderS3Config(spec: Self.spec(endpoint: "http://127.0.0.1:7070"))
        XCTAssertTrue(toml.contains("bucket = \"test\""))
        XCTAssertTrue(toml.contains("secret_ref = \"keychain:swift-test\""))
        XCTAssertThrowsError(try renderS3Config(spec: Self.spec(id: "bad/id", endpoint: nil))) { err in
            guard case NdiskError.Config = err else { return XCTFail("expected Config, got \(err)") }
        }
    }

    /// 需要 S3 兼容服务：`NDISK_TEST_S3_ENDPOINT=http://127.0.0.1:7070 swift test`
    /// （凭据 testkey / testsecret，桶 test）。未设置时跳过。
    func testSessionRoundTripAgainstS3() async throws {
        guard let endpoint = ProcessInfo.processInfo.environment["NDISK_TEST_S3_ENDPOINT"] else {
            throw XCTSkip("NDISK_TEST_S3_ENDPOINT not set")
        }
        let tmp = FileManager.default.temporaryDirectory.appendingPathComponent("ndiskkit-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: tmp, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: tmp) }

        let toml = try renderS3Config(spec: Self.spec(endpoint: endpoint))
        let s = try await openSession(configToml: toml, settingsToml: nil, mountCount: 1,
                                      dataDir: tmp.appendingPathComponent("data").path,
                                      cacheDir: tmp.appendingPathComponent("cache").path, secret: "testsecret")
        defer { s.close() }

        // 创建目录与文件
        let dirName = "swift-\(UUID().uuidString.prefix(8))"
        let dir = try await s.createItem(parent: 1, name: dirName, kind: .directory, contentsPath: nil)
        XCTAssertEqual(dir.kind, .directory)
        let src = tmp.appendingPathComponent("src.txt")
        try "hello from swift 你好".write(to: src, atomically: true, encoding: .utf8)
        let file = try await s.createItem(parent: dir.id, name: "a b.txt", kind: .file, contentsPath: src.path)
        XCTAssertEqual(file.size, UInt64(try Data(contentsOf: src).count))
        XCTAssertFalse(file.pendingUpload)

        // 列举、变更锚点
        let listed = try await s.list(dir: dir.id, force: true)
        XCTAssertEqual(listed.map(\.name), ["a b.txt"])
        let batch = try s.changesSince(anchor: 0)
        XCTAssertTrue(batch.changes.contains { if case .updated(let i) = $0 { return i.id == file.id } else { return false } })
        XCTAssertEqual(try s.currentAnchor(), batch.anchor)

        // 下载内容
        let dest = URL(fileURLWithPath: s.stagingDir()).appendingPathComponent("out.txt")
        _ = try await s.fetchContents(id: file.id, destPath: dest.path)
        XCTAssertEqual(try String(contentsOf: dest, encoding: .utf8), "hello from swift 你好")
        XCTAssertEqual(try s.status().materializedItems, 1)

        // 修改内容、重命名
        try "v2".write(to: src, atomically: true, encoding: .utf8)
        let modified = try await s.modifyContents(id: file.id, contentsPath: src.path)
        XCTAssertEqual(modified.size, 2)
        XCTAssertNotEqual(modified.contentVersion, file.contentVersion)
        let renamed = try await s.rename(id: file.id, newParent: 1, newName: "\(dirName)-moved.txt")
        XCTAssertEqual(renamed.id, file.id)
        XCTAssertEqual(renamed.parent, 1)

        // 策略
        try s.setPolicy(id: dir.id, policy: .ttl(seconds: 60))
        XCTAssertEqual(try s.item(id: dir.id).effectivePolicy, .ttl(seconds: 60))
        try s.pin(id: renamed.id)
        XCTAssertEqual(try s.item(id: renamed.id).effectivePolicy, .always)
        XCTAssertEqual(try s.evictionCandidates(), [])
        let m = try await s.maintain(prefetchPinned: false)
        XCTAssertEqual(m.prefetched, 0)

        // 删除与错误映射
        try await s.delete(id: renamed.id)
        try await s.delete(id: dir.id)
        do {
            _ = try await s.lookup(parent: 1, name: dirName)
            XCTFail("expected NotFound")
        } catch let e as NdiskError {
            guard case .NotFound = e else { return XCTFail("expected NotFound, got \(e)") }
            XCTAssertEqual((e.fileProviderError as NSError).code, NSFileProviderError.noSuchItem.rawValue)
        }
    }

    static func spec(id: String = "swift-test", endpoint: String?) -> S3MountSpec {
        S3MountSpec(
            id: id, displayName: "Swift Test", bucket: "test", prefix: "",
            region: "us-east-1", endpoint: endpoint, forcePathStyle: true,
            auth: .static(accessKeyId: "testkey"), policyOverride: .ttl(seconds: 3600), readOnly: false)
    }
}
