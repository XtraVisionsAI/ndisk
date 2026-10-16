import FileProvider
import Foundation

/// Rust 侧 ItemId（u64，1 为根）与 `NSFileProviderItemIdentifier` 的互转。
public enum NdiskIdentifier {
    public static let rootId: UInt64 = 1

    public static func itemId(_ identifier: NSFileProviderItemIdentifier) -> UInt64? {
        switch identifier {
        case .rootContainer: return rootId
        case .workingSet, .trashContainer: return nil
        default: return UInt64(identifier.rawValue)
        }
    }

    public static func identifier(_ id: UInt64) -> NSFileProviderItemIdentifier {
        id == rootId ? .rootContainer : NSFileProviderItemIdentifier(String(id))
    }
}

extension NdiskError {
    /// 转为 File Provider 能理解的错误，系统据此决定重试、提示或冲突处理。
    public var fileProviderError: Error {
        switch self {
        case .NotFound:
            return NSFileProviderError(.noSuchItem)
        case .AlreadyExists:
            return NSFileProviderError(.filenameCollision)
        case .Conflict:
            return NSFileProviderError(.cannotSynchronize)
        case .Offline:
            return NSFileProviderError(.serverUnreachable)
        case .ReadOnly, .PermissionDenied:
            return CocoaError(.fileWriteNoPermission)
        case .DirectoryNotEmpty:
            return NSFileProviderError(.directoryNotEmpty)
        case .NotADirectory, .IsADirectory, .Unsupported:
            return CocoaError(.featureUnsupported)
        case .Config, .Other:
            return NSError(domain: NSCocoaErrorDomain, code: CocoaError.fileReadUnknown.rawValue,
                           userInfo: [NSLocalizedDescriptionKey: "\(self)"])
        }
    }
}

/// 任意错误转为 File Provider 错误。
public func fileProviderError(_ error: Error) -> Error {
    (error as? NdiskError)?.fileProviderError ?? error
}
