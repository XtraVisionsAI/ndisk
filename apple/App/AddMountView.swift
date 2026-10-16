import NdiskKit
import SwiftUI

/// 添加 S3 / S3 兼容挂载。密钥写入共享钥匙串，配置文件中只保存引用。
struct AddMountView: View {
    @ObservedObject var model: MountsModel
    @Environment(\.dismiss) private var dismiss

    @State private var displayName = ""
    @State private var bucket = ""
    @State private var prefix = ""
    @State private var endpoint = ""
    @State private var region = "us-east-1"
    @State private var pathStyle = false
    @State private var accessKey = ""
    @State private var secretKey = ""
    @State private var ttlHours = 168
    @State private var readOnly = false
    @State private var proxy = ""
    @State private var testResult: String?
    @State private var busy = false

    private var mountId: String {
        let base = (displayName.isEmpty ? bucket : displayName).lowercased()
        let cleaned = base.map { $0.isLetter || $0.isNumber ? $0 : "-" }
        return String(cleaned).trimmingCharacters(in: CharacterSet(charactersIn: "-"))
    }

    private var spec: S3MountSpec {
        S3MountSpec(
            id: mountId, displayName: displayName.isEmpty ? bucket : displayName,
            bucket: bucket, prefix: prefix, region: region, endpoint: endpoint,
            forcePathStyle: pathStyle, accessKeyId: accessKey, profile: nil,
            defaultPolicy: .ttl(seconds: UInt64(ttlHours) * 3600), maxCacheBytes: nil,
            proxy: proxy, uploadLimit: 0, downloadLimit: 0, readOnly: readOnly)
    }

    var body: some View {
        Form {
            Section("存储") {
                TextField("显示名称", text: $displayName)
                TextField("Bucket", text: $bucket)
                TextField("前缀（可选）", text: $prefix)
                TextField("Endpoint（S3 兼容服务填写）", text: $endpoint)
                TextField("Region", text: $region)
                Toggle("Path-style 访问（MinIO 等需要）", isOn: $pathStyle)
            }
            Section("凭据") {
                TextField("Access Key ID（留空使用默认凭据链）", text: $accessKey)
                SecureField("Secret Access Key", text: $secretKey)
            }
            Section("缓存与网络") {
                Stepper("缓存有效期：\(ttlHours) 小时", value: $ttlHours, in: 1...24 * 365)
                Toggle("只读", isOn: $readOnly)
                TextField("代理（可选，如 http://host:3128 或 env）", text: $proxy)
            }
            if let testResult {
                Text(testResult).font(.caption).foregroundStyle(.secondary)
            }
            HStack {
                Button("测试连接") {
                    busy = true
                    Task {
                        switch await model.testConnection(spec: spec, secret: secretKey) {
                        case .success(let n): testResult = "连接成功，根目录有 \(n) 项"
                        case .failure(let e): testResult = "连接失败：\(e)"
                        }
                        busy = false
                    }
                }
                Spacer()
                Button("取消") { dismiss() }
                Button("添加") {
                    busy = true
                    Task {
                        await model.add(spec: spec, secret: secretKey)
                        busy = false
                        if model.lastError == nil { dismiss() }
                    }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(bucket.isEmpty || mountId.isEmpty)
            }
            .disabled(busy)
        }
        .formStyle(.grouped)
        .frame(width: 460)
        .padding()
    }
}
