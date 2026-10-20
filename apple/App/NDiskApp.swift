import AppKit
import NdiskKit
import SwiftUI

@main
struct NDiskApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate

    var body: some Scene {
        MenuBarExtra("NDisk", systemImage: "externaldrive.connected.to.line.below") {
            MenuContent(model: delegate.model)
        }
        .menuBarExtraStyle(.window)

        Window("添加挂载", id: "add-mount") {
            MountFormView(model: delegate.model)
        }
        .windowResizability(.contentSize)

        // 编辑窗口按挂载 id 打开。
        WindowGroup("编辑挂载", id: "edit-mount", for: String.self) { $mountId in
            if let id = mountId, let m = delegate.model.mounts.first(where: { $0.id == id }),
               let spec = delegate.model.existingSpec(m) {
                MountFormView(model: delegate.model, existing: spec)
            } else {
                Text("找不到该挂载").padding()
            }
        }
        .windowResizability(.contentSize)

        Window("NDisk 设置", id: "settings") {
            SettingsView(model: delegate.model)
        }
        .windowResizability(.contentSize)
    }
}

/// 持有数据模型并接收 `ndisk://` URL。
/// 注意：MenuBarExtra 的内容视图只在菜单打开时存在，`onOpenURL` 放在那里会丢事件，所以在这里处理。
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    let model = MountsModel()

    func application(_ application: NSApplication, open urls: [URL]) {
        for url in urls {
            Task { await model.handle(url: url) }
        }
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
                let off = model.disconnected.contains(m.id)
                HStack {
                    Image(systemName: off ? "externaldrive.badge.xmark" : "externaldrive")
                        .foregroundStyle(off ? .secondary : .primary)
                    VStack(alignment: .leading) {
                        Text(m.displayName)
                        Text(off ? "已断开" : statusLine(m)).font(.caption).foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button { model.reveal(m) } label: { Image(systemName: "folder") }
                        .help("在 Finder 中打开").disabled(off)
                    Button {
                        openWindow(id: "edit-mount", value: m.id)
                        NSApp.activate(ignoringOtherApps: true)
                    } label: { Image(systemName: "pencil") }
                        .help("编辑")
                    Button { model.toggleConnection(m) } label: { Image(systemName: off ? "play.circle" : "eject") }
                        .help(off ? "连接" : "断开（保留配置与缓存）")
                    Button { if model.confirmRemove(m) { model.remove(m) } } label: { Image(systemName: "trash") }
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
                Button("设置…") {
                    openWindow(id: "settings")
                    NSApp.activate(ignoringOtherApps: true)
                }
                Spacer()
                Button("退出") { NSApp.terminate(nil) }
                    .help("只退出菜单栏程序；挂载由系统继续提供，不受影响")
            }
        }
        .padding(12)
        .frame(width: 360)
        .onAppear { model.refreshStatuses() }
    }

    private func statusLine(_ m: SharedStore.Mount) -> String {
        guard let st = model.statuses[m.id] else { return "尚未同步" }
        var parts = [ByteCountFormatter.string(fromByteCount: Int64(st.cachedBytes), countStyle: .file) + " 缓存"]
        if st.pendingUploads > 0 { parts.append("\(st.pendingUploads) 个待上传") }
        parts.append("\(st.materializedItems) 个已下载")
        if let err = st.lastError { parts.append("错误：\(err)") }
        return parts.joined(separator: " · ")
    }
}
