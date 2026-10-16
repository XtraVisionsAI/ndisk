import FileProvider
import Foundation
import NdiskKit

// 用法：
//   ndisk-devctl add <id> <displayName> <bucket> <endpoint> <accessKey> <secret> [ttlSeconds]
//   ndisk-devctl remove <id>
//   ndisk-devctl list
//   ndisk-devctl url <id>
//   ndisk-devctl signal <id>
//
// add / remove 通过 `ndisk://` URL 交给菜单栏 App 执行（Group 容器只有沙盒 App 能写），其余直接查询系统。

let args = Array(CommandLine.arguments.dropFirst())
guard let cmd = args.first else { fputs("usage: ndisk-devctl add|remove|list|url|signal ...\n", stderr); exit(2) }


func domain(_ m: SharedStore.Mount) -> NSFileProviderDomain {
    NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(m.id), displayName: m.displayName)
}

func run() async throws {
    switch cmd {
    case "add":
        // Group 容器只有沙盒 App 能写：通过 URL scheme 交给 App 处理。
        guard args.count >= 7 else { throw NdiskError.Config(message: "add needs 6 args") }
        var c = URLComponents(string: "ndisk://add")!
        c.queryItems = [
            .init(name: "id", value: args[1]), .init(name: "name", value: args[2]), .init(name: "bucket", value: args[3]),
            .init(name: "endpoint", value: args[4]), .init(name: "access_key", value: args[5]), .init(name: "secret", value: args[6]),
            .init(name: "ttl", value: args.count > 7 ? args[7] : "3600"), .init(name: "path_style", value: "1"),
        ]
        let p = Process(); p.executableURL = URL(fileURLWithPath: "/usr/bin/open"); p.arguments = ["-g", c.url!.absoluteString]
        try p.run(); p.waitUntilExit()
        // 等待 domain 出现
        for _ in 0..<40 {
            if try await NSFileProviderManager.domains().contains(where: { $0.identifier.rawValue == args[1] }) { print("added domain \(args[1])"); return }
            try await Task.sleep(nanoseconds: 250_000_000)
        }
        throw NdiskError.Other(message: "domain \(args[1]) did not appear; check App log")
    case "remove":
        let p = Process(); p.executableURL = URL(fileURLWithPath: "/usr/bin/open"); p.arguments = ["-g", "ndisk://remove?id=\(args[1])"]
        try p.run(); p.waitUntilExit()
        for _ in 0..<40 {
            if try await !NSFileProviderManager.domains().contains(where: { $0.identifier.rawValue == args[1] }) { print("removed \(args[1])"); return }
            try await Task.sleep(nanoseconds: 250_000_000)
        }
        throw NdiskError.Other(message: "domain \(args[1]) still present")
    case "list":
        let domains = try await NSFileProviderManager.domains()
        for d in domains { print("domain: \(d.identifier.rawValue) (\(d.displayName))") }
    case "url":
        let m = SharedStore.Mount(id: args[1], displayName: args[1])
        guard let mgr = NSFileProviderManager(for: domain(m)) else { throw NdiskError.Other(message: "no manager") }
        print(try await mgr.getUserVisibleURL(for: .rootContainer).path)
    case "signal":
        let m = SharedStore.Mount(id: args[1], displayName: args[1])
        guard let mgr = NSFileProviderManager(for: domain(m)) else { throw NdiskError.Other(message: "no manager") }
        try await mgr.signalEnumerator(for: .workingSet)
        print("signaled")
    default:
        throw NdiskError.Config(message: "unknown command \(cmd)")
    }
}

do { try await run() } catch { fputs("error: \(error)\n", stderr); exit(1) }
