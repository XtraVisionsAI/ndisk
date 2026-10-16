import FileProvider
import NdiskKit
import UniformTypeIdentifiers

/// Rust `ItemInfo` 到 `NSFileProviderItem` 的适配。
final class FileProviderItem: NSObject, NSFileProviderItem, NSFileProviderItemDecorating {
    let info: ItemInfo
    private let pinnedDecoration: NSFileProviderItemDecorationIdentifier

    init(_ info: ItemInfo, pinnedDecoration: NSFileProviderItemDecorationIdentifier) {
        self.info = info
        self.pinnedDecoration = pinnedDecoration
    }

    var isPinned: Bool { info.effectivePolicy == .always }

    var itemIdentifier: NSFileProviderItemIdentifier { NdiskIdentifier.identifier(info.id) }

    var parentItemIdentifier: NSFileProviderItemIdentifier {
        info.id == NdiskIdentifier.rootId ? .rootContainer : NdiskIdentifier.identifier(info.parent)
    }

    var filename: String { info.id == NdiskIdentifier.rootId ? "" : info.name }

    var contentType: UTType {
        if info.kind == .directory { return .folder }
        let ext = (info.name as NSString).pathExtension
        return UTType(filenameExtension: ext) ?? .data
    }

    var documentSize: NSNumber? { info.kind == .file ? NSNumber(value: info.size) : nil }

    var contentModificationDate: Date? { info.mtime }

    var creationDate: Date? { info.mtime }

    var itemVersion: NSFileProviderItemVersion {
        NSFileProviderItemVersion(contentVersion: Data(info.contentVersion.utf8),
                                  metadataVersion: Data(info.metadataVersion.utf8))
    }

    var capabilities: NSFileProviderItemCapabilities {
        var c: NSFileProviderItemCapabilities = [.allowsReading, .allowsRenaming, .allowsReparenting, .allowsDeleting]
        switch info.kind {
        case .directory: c.formUnion([.allowsContentEnumerating, .allowsAddingSubItems])
        case .file: c.insert(.allowsWriting)
        }
        return c
    }

    /// 上传状态由系统在 Finder 中显示进度与角标。
    var isUploaded: Bool { !info.pendingUpload }
    var isUploading: Bool { info.state == .uploading }

    /// "始终保留在本机"映射为系统的立即下载并保持；其余继承父级（根目录为按需下载）。
    var contentPolicy: NSFileProviderContentPolicy {
        if isPinned { return .downloadEagerlyAndKeepDownloaded }
        return info.id == NdiskIdentifier.rootId ? .downloadLazily : .inherited
    }

    /// 供右键菜单谓词使用（Info.plist 中的 `userInfo.pinned`）。
    var userInfo: [AnyHashable: Any]? { ["pinned": isPinned] }

    var decorations: [NSFileProviderItemDecorationIdentifier]? { isPinned ? [pinnedDecoration] : [] }
}
