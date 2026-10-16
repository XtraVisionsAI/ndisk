import FileProvider
import NdiskKit

/// 目录与工作集的枚举器。
///
/// 采用 replicated 模型：变更统一通过工作集（`.workingSet`）上报，
/// 目录枚举器只负责列出子项，`enumerateChanges` 直接推进到当前锚点。
final class FileProviderEnumerator: NSObject, NSFileProviderEnumerator {
    private let container: NSFileProviderItemIdentifier
    private let ext: FileProviderExtension

    init(container: NSFileProviderItemIdentifier, extension ext: FileProviderExtension) {
        self.container = container
        self.ext = ext
    }

    func invalidate() {}

    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        Task {
            do {
                if container == .workingSet || container == .trashContainer {
                    // 工作集的初始内容由各目录枚举得到；回收站不支持。
                    observer.finishEnumerating(upTo: nil)
                    return
                }
                guard let dir = NdiskIdentifier.itemId(container) else {
                    observer.finishEnumeratingWithError(NSFileProviderError(.noSuchItem))
                    return
                }
                let s = try await ext.session()
                // 初始页强制刷新，保证 Finder 打开目录时看到远端最新内容。
                let force = page == NSFileProviderPage.initialPageSortedByName as NSFileProviderPage
                    || page == NSFileProviderPage.initialPageSortedByDate as NSFileProviderPage
                let items = try await s.list(dir: dir, force: force)
                observer.didEnumerate(items.map(ext.makeItem))
                observer.finishEnumerating(upTo: nil)
            } catch {
                observer.finishEnumeratingWithError(fileProviderError(error))
            }
        }
    }

    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        Task {
            do {
                let s = try await ext.session()
                guard container == .workingSet else {
                    observer.finishEnumeratingChanges(upTo: Self.anchor(try s.currentAnchor()), moreComing: false)
                    return
                }
                let batch = try s.changesSince(anchor: Self.value(anchor))
                var updated: [NSFileProviderItem] = []
                var deleted: [NSFileProviderItemIdentifier] = []
                for c in batch.changes {
                    switch c {
                    case .updated(let item):
                        if item.id != NdiskIdentifier.rootId { updated.append(ext.makeItem(item)) }
                    case .deleted(let id):
                        deleted.append(NdiskIdentifier.identifier(id))
                    }
                }
                if !updated.isEmpty { observer.didUpdate(updated) }
                if !deleted.isEmpty { observer.didDeleteItems(withIdentifiers: deleted) }
                observer.finishEnumeratingChanges(upTo: Self.anchor(batch.anchor), moreComing: false)
                // 工作集被唤醒时顺带做一次 TTL 驱逐（App 定期 signal 工作集即可驱动）。
                ext.scheduleEviction()
            } catch {
                observer.finishEnumeratingWithError(fileProviderError(error))
            }
        }
    }

    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        Task {
            let s = try? await ext.session()
            completionHandler((try? s?.currentAnchor()).flatMap { $0 }.map(Self.anchor))
        }
    }

    static func anchor(_ v: UInt64) -> NSFileProviderSyncAnchor {
        withUnsafeBytes(of: v.bigEndian) { NSFileProviderSyncAnchor(Data($0)) }
    }

    static func value(_ a: NSFileProviderSyncAnchor) -> UInt64 {
        guard a.rawValue.count == 8 else { return 0 }
        return a.rawValue.withUnsafeBytes { UInt64(bigEndian: $0.loadUnaligned(as: UInt64.self)) }
    }
}
