import FileProvider
import NdiskKit
import os

/// 一个挂载点（File Provider domain）对应一个扩展实例，domain.identifier 即挂载 id。
final class FileProviderExtension: NSObject, NSFileProviderReplicatedExtension, NSFileProviderCustomAction {
    let domain: NSFileProviderDomain
    private let log = Logger(subsystem: "ai.xtravisions.ndisk", category: "FileProvider")
    private let holder = SessionHolder()
    private let pinnedDecoration: NSFileProviderItemDecorationIdentifier

    /// 远端变更轮询间隔与"热目录"窗口。
    static let pollInterval: TimeInterval = 60
    static let hotWindow: UInt64 = 15 * 60
    private var background: Task<Void, Never>?
    private var lastError: String?
    private var fingerprint: String?

    required init(domain: NSFileProviderDomain) {
        self.domain = domain
        let bundleId = Bundle.main.bundleIdentifier ?? "ai.xtravisions.ndisk.FileProvider"
        self.pinnedDecoration = NSFileProviderItemDecorationIdentifier("\(bundleId).pinned")
        super.init()
        initLogging(filter: "info,aws_config=warn,aws_smithy_runtime=warn")
        background = Task { [weak self] in await self?.backgroundLoop() }
    }

    func invalidate() {
        background?.cancel()
        Task { await holder.close() }
    }

    /// 扩展存活期间：轮询用户最近看过的目录发现远端变更，执行 TTL 驱逐，写出状态供 App 展示。
    private func backgroundLoop() async {
        while !Task.isCancelled {
            await pollOnce()
            try? await Task.sleep(nanoseconds: UInt64(Self.pollInterval * 1_000_000_000))
        }
    }

    private func pollOnce() async {
        var changes: UInt64 = 0
        // 配置或全局设置被 App 修改：关掉会话，下次调用时按新配置重开。
        if let store = try? SharedStore.fromBundle() {
            let fp = store.configFingerprint(mountId: domain.identifier.rawValue)
            if let old = fingerprint, old != fp {
                log.notice("config changed; reopening session")
                await holder.close()
            }
            fingerprint = fp
        }
        do {
            let s = try await session()
            changes = try await s.refreshHotDirs(withinSeconds: Self.hotWindow)
            if changes > 0 {
                try await manager?.signalEnumerator(for: .workingSet)
                log.notice("remote changes discovered: \(changes)")
            }
            lastError = nil
        } catch {
            lastError = "\(error)"
            log.error("poll failed: \(String(describing: error), privacy: .public)")
        }
        await evictExpired()
        writeStatus(lastRefreshChanges: changes)
    }

    private func writeStatus(lastRefreshChanges: UInt64) {
        guard let store = try? SharedStore.fromBundle() else { return }
        Task {
            guard let s = try? await session(), let st = try? s.status() else { return }
            let status = SharedStore.MountStatus(
                mountId: domain.identifier.rawValue, backend: st.backend, cachedBytes: st.cachedBytes,
                pendingUploads: st.pendingUploads, materializedItems: st.materializedItems,
                lastRefreshChanges: lastRefreshChanges, lastError: lastError, updatedAt: Date())
            try? store.writeStatus(status)
        }
    }

    // MARK: 会话

    func session() async throws -> Session {
        let mountId = domain.identifier.rawValue
        return try await holder.get {
            let store = try SharedStore.fromBundle()
            return try await store.openSession(mountId: mountId)
        }
    }

    func makeItem(_ info: ItemInfo) -> FileProviderItem {
        let rootName = domain.displayName.isEmpty ? "NDisk" : domain.displayName
        return FileProviderItem(info, pinnedDecoration: pinnedDecoration, rootName: rootName)
    }

    private var manager: NSFileProviderManager? { NSFileProviderManager(for: domain) }

    /// 把 async 工作包装为 File Provider 需要的 Progress + completion 形式。
    private func run<T>(_ completion: @escaping (T?, Error?) -> Void, _ body: @escaping () async throws -> T) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        let task = Task {
            do {
                let v = try await body()
                progress.completedUnitCount = 1
                completion(v, nil)
            } catch {
                completion(nil, fileProviderError(error))
            }
        }
        progress.cancellationHandler = { task.cancel() }
        return progress
    }

    // MARK: 条目

    func item(for identifier: NSFileProviderItemIdentifier, request: NSFileProviderRequest,
              completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void) -> Progress {
        run(completionHandler) { [self] in
            guard let id = NdiskIdentifier.itemId(identifier) else { throw NdiskError.NotFound(message: identifier.rawValue) }
            return makeItem(try await session().item(id: id))
        }
    }

    func fetchContents(for itemIdentifier: NSFileProviderItemIdentifier, version requestedVersion: NSFileProviderItemVersion?,
                       request: NSFileProviderRequest,
                       completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        let task = Task { [self] in
            do {
                guard let id = NdiskIdentifier.itemId(itemIdentifier) else { throw NdiskError.NotFound(message: itemIdentifier.rawValue) }
                let s = try await session()
                // 系统会移动或复制该文件到自己的存储，之后由系统持有副本。
                let dest = URL(fileURLWithPath: s.stagingDir()).appendingPathComponent(UUID().uuidString)
                let info = try await s.fetchContents(id: id, destPath: dest.path)
                progress.completedUnitCount = 1
                completionHandler(dest, makeItem(info), nil)
            } catch {
                completionHandler(nil, nil, fileProviderError(error))
            }
        }
        progress.cancellationHandler = { task.cancel() }
        return progress
    }

    func createItem(basedOn itemTemplate: NSFileProviderItem, fields: NSFileProviderItemFields, contents url: URL?,
                    options: NSFileProviderCreateItemOptions = [], request: NSFileProviderRequest,
                    completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        run({ item, error in completionHandler(item, [], false, error) }) { [self] () async throws -> NSFileProviderItem in
            let s = try await session()
            guard let parent = NdiskIdentifier.itemId(itemTemplate.parentItemIdentifier) else {
                throw NdiskError.NotFound(message: "parent")
            }
            let isDir = itemTemplate.contentType == .folder
            if itemTemplate.contentType == .symbolicLink || (itemTemplate.contentType?.conforms(to: .package) ?? false) {
                throw NdiskError.Unsupported(message: "symlinks and packages are not supported")
            }
            // 重新导入（例如 domain 重建）时，同名条目已存在则直接返回。
            if options.contains(.mayAlreadyExist),
               let existing = try? await s.lookup(parent: parent, name: itemTemplate.filename) {
                return makeItem(existing)
            }
            let info = try await s.createItem(parent: parent, name: itemTemplate.filename,
                                              kind: isDir ? .directory : .file, contentsPath: url?.path)
            return makeItem(info)
        }
    }

    func modifyItem(_ item: NSFileProviderItem, baseVersion version: NSFileProviderItemVersion,
                    changedFields: NSFileProviderItemFields, contents newContents: URL?,
                    options: NSFileProviderModifyItemOptions = [], request: NSFileProviderRequest,
                    completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        run({ item, error in completionHandler(item, [], false, error) }) { [self] () async throws -> NSFileProviderItem in
            let s = try await session()
            guard let id = NdiskIdentifier.itemId(item.itemIdentifier) else { throw NdiskError.NotFound(message: "item") }
            var info = try s.item(id: id)
            if changedFields.contains(.contents), let url = newContents {
                info = try await s.modifyContents(id: id, contentsPath: url.path)
            }
            if changedFields.contains(.filename) || changedFields.contains(.parentItemIdentifier) {
                guard let parent = NdiskIdentifier.itemId(item.parentItemIdentifier) else {
                    throw NdiskError.Unsupported(message: "moving to trash is not supported")
                }
                info = try await s.rename(id: id, newParent: parent, newName: item.filename)
            }
            // 其余字段（时间、扩展属性、标签）对象存储不保存，视为已接受。
            return makeItem(info)
        }
    }

    func deleteItem(identifier: NSFileProviderItemIdentifier, baseVersion version: NSFileProviderItemVersion,
                    options: NSFileProviderDeleteItemOptions = [], request: NSFileProviderRequest,
                    completionHandler: @escaping (Error?) -> Void) -> Progress {
        run({ (_: Void?, error) in completionHandler(error) }) { [self] in
            let s = try await session()
            guard let id = NdiskIdentifier.itemId(identifier) else { throw NdiskError.NotFound(message: identifier.rawValue) }
            if options.contains(.recursive) { try await deleteRecursively(s, id) } else { try await s.delete(id: id) }
        }
    }

    private func deleteRecursively(_ s: Session, _ id: UInt64) async throws {
        if try s.item(id: id).kind == .directory {
            for child in try await s.list(dir: id, force: true) {
                try await deleteRecursively(s, child.id)
            }
        }
        try await s.delete(id: id)
    }

    func enumerator(for containerItemIdentifier: NSFileProviderItemIdentifier,
                    request: NSFileProviderRequest) throws -> NSFileProviderEnumerator {
        FileProviderEnumerator(container: containerItemIdentifier, extension: self)
    }

    // MARK: 已下载集合与 TTL 驱逐

    func materializedItemsDidChange(completionHandler: @escaping () -> Void) {
        Task { [self] in
            defer { completionHandler() }
            guard let manager else { return }
            do {
                let ids = try await materializedIds(manager)
                try await session().syncMaterialized(ids: ids)
                await evictExpired()
            } catch {
                log.error("materialized sync failed: \(String(describing: error), privacy: .public)")
            }
        }
    }

    private func materializedIds(_ manager: NSFileProviderManager) async throws -> [UInt64] {
        let collector = MaterializedCollector()
        let enumerator = manager.enumeratorForMaterializedItems()
        return try await collector.collect(from: enumerator)
    }

    func scheduleEviction() {
        Task { await evictExpired() }
    }

    /// 让系统驱逐 TTL 已到期的已下载文件。始终保留与有未上传修改的文件不会出现在候选中。
    private func evictExpired() async {
        guard let manager, let s = try? await session(), let ids = try? s.evictionCandidates() else { return }
        for id in ids {
            do {
                try await manager.evictItem(identifier: NdiskIdentifier.identifier(id))
                try s.markEvicted(id: id)
                log.notice("evicted item \(id) (ttl expired)")
            } catch {
                log.error("evict \(id) failed: \(String(describing: error), privacy: .public)")
            }
        }
    }

    // MARK: 右键动作

    func performAction(identifier actionIdentifier: NSFileProviderExtensionActionIdentifier,
                       onItemsWithIdentifiers itemIdentifiers: [NSFileProviderItemIdentifier],
                       completionHandler: @escaping (Error?) -> Void) -> Progress {
        let action = actionIdentifier.rawValue.components(separatedBy: ".action.").last ?? ""
        return run({ (_: Void?, error) in completionHandler(error) }) { [self] in
            let s = try await session()
            guard let manager else { throw NdiskError.Other(message: "no manager for domain") }
            for identifier in itemIdentifiers {
                guard let id = NdiskIdentifier.itemId(identifier) else { continue }
                switch action {
                case "download":
                    try await manager.requestDownloadForItem(withIdentifier: identifier, requestedRange: NSRange(location: NSNotFound, length: 0))
                case "pin":
                    try s.pin(id: id)
                case "unpin":
                    try s.unpin(id: id)
                case "evict":
                    try await manager.evictItem(identifier: identifier)
                    try s.markEvicted(id: id)
                default:
                    throw NdiskError.Unsupported(message: "unknown action \(actionIdentifier.rawValue)")
                }
            }
            // 策略变化通过工作集上报，系统据此更新 contentPolicy 与角标。
            if action == "pin" || action == "unpin" {
                try await manager.signalEnumerator(for: .workingSet)
            }
        }
    }
}

/// 串行化会话的打开与关闭：扩展的回调可能并发到达。
actor SessionHolder {
    private var session: Session?
    private var opening: Task<Session, Error>?

    func get(_ open: @escaping @Sendable () async throws -> Session) async throws -> Session {
        if let session { return session }
        if let opening { return try await opening.value }
        let t = Task { try await open() }
        opening = t
        defer { opening = nil }
        let s = try await t.value
        session = s
        return s
    }

    func close() {
        session?.close()
        session = nil
    }
}

/// 收集系统已下载条目的 identifier。
final class MaterializedCollector: NSObject, NSFileProviderEnumerationObserver {
    private var ids: [UInt64] = []
    private var continuation: CheckedContinuation<[UInt64], Error>?
    private var enumerator: NSFileProviderEnumerator?

    func collect(from enumerator: NSFileProviderEnumerator) async throws -> [UInt64] {
        self.enumerator = enumerator
        return try await withCheckedThrowingContinuation { c in
            continuation = c
            enumerator.enumerateItems(for: self, startingAt: NSFileProviderPage.initialPageSortedByName as NSFileProviderPage)
        }
    }

    func didEnumerate(_ updatedItems: [NSFileProviderItemProtocol]) {
        ids.append(contentsOf: updatedItems.compactMap { NdiskIdentifier.itemId($0.itemIdentifier) })
    }

    func finishEnumerating(upTo nextPage: NSFileProviderPage?) {
        if let nextPage, let enumerator {
            enumerator.enumerateItems(for: self, startingAt: nextPage)
            return
        }
        continuation?.resume(returning: ids)
        continuation = nil
        enumerator?.invalidate()
    }

    func finishEnumeratingWithError(_ error: Error) {
        continuation?.resume(throwing: error)
        continuation = nil
        enumerator?.invalidate()
    }
}
