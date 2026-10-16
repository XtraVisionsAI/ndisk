import NdiskKit
import SwiftUI

/// 全局设置：对所有挂载生效。
struct SettingsView: View {
    @ObservedObject var model: MountsModel
    @Environment(\.dismiss) private var dismiss

    @State private var policyChoice: PolicyChoice = .ttl
    @State private var ttlHours = 168
    @State private var maxCacheGB = 20
    @State private var proxyMode = 0 // 0 直连 1 系统/环境变量 2 自定义
    @State private var proxyURL = ""
    @State private var uploadLimitMB = 0
    @State private var downloadLimitMB = 0
    @State private var launchAtLogin = false
    @State private var logFilter = ""

    var body: some View {
        Form {
            Section("缓存") {
                Picker("默认策略", selection: $policyChoice) {
                    ForEach(PolicyChoice.allCases.filter { $0 != .inherit }) { Text($0.rawValue).tag($0) }
                }
                if policyChoice == .ttl {
                    Stepper("下载后保留：\(ttlHours) 小时", value: $ttlHours, in: 1...24 * 365)
                }
                Stepper("本地缓存上限：\(maxCacheGB) GB（所有挂载合计）", value: $maxCacheGB, in: 1...2000)
            }
            Section("网络") {
                Picker("代理", selection: $proxyMode) {
                    Text("不使用").tag(0)
                    Text("系统 / 环境变量").tag(1)
                    Text("自定义").tag(2)
                }
                if proxyMode == 2 {
                    TextField("http://[user:pass@]host:port", text: $proxyURL)
                }
                Stepper("上传限速：\(uploadLimitMB == 0 ? "不限" : "\(uploadLimitMB) MB/s")", value: $uploadLimitMB, in: 0...1000)
                Stepper("下载限速：\(downloadLimitMB == 0 ? "不限" : "\(downloadLimitMB) MB/s")", value: $downloadLimitMB, in: 0...1000)
            }
            Section("通用") {
                Toggle("登录时启动", isOn: $launchAtLogin)
                TextField("日志级别（如 info,ndisk_core=debug）", text: $logFilter)
            }
            HStack {
                Spacer()
                Button("取消") { dismiss() }
                Button("保存") {
                    Task {
                        model.launchAtLogin = launchAtLogin
                        await model.saveSettings(spec)
                        if model.lastError == nil { dismiss() }
                    }
                }
                .keyboardShortcut(.defaultAction)
            }
        }
        .formStyle(.grouped)
        .frame(width: 460)
        .padding()
        .onAppear(perform: load)
    }

    private var spec: SettingsSpec {
        let policy: CachePolicyFfi = switch policyChoice {
        case .always: .always
        case .transient: .transient
        default: .ttl(seconds: UInt64(ttlHours) * 3600)
        }
        let proxy = switch proxyMode { case 1: "env"; case 2: proxyURL; default: "" }
        return SettingsSpec(defaultPolicy: policy, maxCacheBytes: UInt64(maxCacheGB) << 30, proxy: proxy,
                            uploadLimit: UInt64(uploadLimitMB) << 20, downloadLimit: UInt64(downloadLimitMB) << 20,
                            logFilter: logFilter)
    }

    private func load() {
        let s = model.settings()
        switch s.defaultPolicy {
        case .ttl(let secs): policyChoice = .ttl; ttlHours = max(1, Int(secs / 3600))
        case .always: policyChoice = .always
        case .transient: policyChoice = .transient
        }
        maxCacheGB = max(1, Int(s.maxCacheBytes >> 30))
        if s.proxy.isEmpty { proxyMode = 0 } else if s.proxy == "env" || s.proxy == "system" { proxyMode = 1 } else { proxyMode = 2; proxyURL = s.proxy }
        uploadLimitMB = Int(s.uploadLimit >> 20)
        downloadLimitMB = Int(s.downloadLimit >> 20)
        logFilter = s.logFilter
        launchAtLogin = model.launchAtLogin
    }
}
