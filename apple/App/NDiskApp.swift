import AppKit
import SwiftUI

@main
struct NDiskApp: App {
    @StateObject private var model = MountsModel()

    var body: some Scene {
        MenuBarExtra("NDisk", systemImage: "externaldrive.connected.to.line.below") {
            MenuContent(model: model)
        }
        .menuBarExtraStyle(.window)

        Window("添加挂载", id: "add-mount") {
            AddMountView(model: model)
        }
        .windowResizability(.contentSize)
    }
}

struct MenuContent: View {
    @ObservedObject var model: MountsModel
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("NDisk").font(.headline)
                Spacer()
                Text("core \(model.coreVersion)").font(.caption).foregroundStyle(.secondary)
            }
            Divider()
            if model.mounts.isEmpty {
                Text("尚未添加挂载").foregroundStyle(.secondary)
            }
            ForEach(model.mounts) { m in
                HStack {
                    Image(systemName: "externaldrive")
                    VStack(alignment: .leading) {
                        Text(m.displayName)
                        Text(m.id).font(.caption).foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button { model.reveal(m) } label: { Image(systemName: "folder") }
                        .help("在 Finder 中打开")
                    Button { model.remove(m) } label: { Image(systemName: "trash") }
                        .help("移除挂载")
                }
                .buttonStyle(.borderless)
            }
            if let err = model.lastError {
                Text(err).font(.caption).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
            }
            Divider()
            HStack {
                Button("添加挂载…") {
                    openWindow(id: "add-mount")
                    NSApp.activate(ignoringOtherApps: true)
                }
                Spacer()
                Button("退出") { NSApp.terminate(nil) }
            }
        }
        .padding(12)
        .frame(width: 320)
    }
}
