import NdiskKit
import SwiftUI

/// 服务商预设：决定 Endpoint 格式与寻址方式，用户只需填 Bucket 与凭据。
enum S3Provider: String, CaseIterable, Identifiable {
    case aws = "Amazon S3"
    case aliyunOSS = "阿里云 OSS"
    case tencentCOS = "腾讯云 COS"
    case cloudflareR2 = "Cloudflare R2"
    case minio = "MinIO / 自建"
    case custom = "自定义 S3 兼容"

    var id: String { rawValue }

    /// 需要用户填写的"区域 / 账户"字段标题；nil 表示不需要。
    var regionLabel: String? {
        switch self {
        case .aws: return "Region（可留空自动探测）"
        case .aliyunOSS: return "Region，如 oss-cn-hangzhou"
        case .tencentCOS: return "Region，如 ap-shanghai"
        case .cloudflareR2: return "Account ID"
        case .minio, .custom: return nil
        }
    }

    var needsEndpoint: Bool { self == .minio || self == .custom }

    func endpoint(region: String, custom: String) -> String? {
        switch self {
        case .aws: return nil
        case .aliyunOSS: return "https://\(region).aliyuncs.com"
        case .tencentCOS: return "https://cos.\(region).myqcloud.com"
        case .cloudflareR2: return "https://\(region).r2.cloudflarestorage.com"
        case .minio, .custom: return custom
        }
    }

    /// 传给 SDK 的签名区域。
    func sdkRegion(_ region: String) -> String? {
        switch self {
        case .aws: return region.isEmpty ? nil : region
        case .aliyunOSS, .tencentCOS: return region
        case .cloudflareR2: return "auto"
        case .minio, .custom: return "us-east-1"
        }
    }

    /// MinIO 等常用路径式；其余先试虚拟主机式，后端会自动回退。
    var forcePathStyle: Bool { self == .minio }

    static func detect(_ spec: S3MountSpec) -> S3Provider {
        guard let ep = spec.endpoint, !ep.isEmpty else { return .aws }
        if ep.contains("aliyuncs.com") { return .aliyunOSS }
        if ep.contains("myqcloud.com") { return .tencentCOS }
        if ep.contains("r2.cloudflarestorage.com") { return .cloudflareR2 }
        return spec.forcePathStyle ? .minio : .custom
    }
}

enum AuthKind: String, CaseIterable, Identifiable {
    case accessKey = "Access Key"
    case assumeRole = "AssumeRole（STS）"
    case profile = "命名 Profile"
    case defaultChain = "默认凭据链"
    var id: String { rawValue }
}

enum PolicyChoice: String, CaseIterable, Identifiable {
    case inherit = "使用全局默认"
    case ttl = "自定义有效期"
    case always = "始终保留"
    case transient = "读完即可回收"
    var id: String { rawValue }
}

/// 添加或编辑一个 S3 挂载。编辑时 Bucket / 前缀 / Endpoint 不可改（等同于换一个存储）。
struct MountFormView: View {
    @ObservedObject var model: MountsModel
    /// 编辑的现有挂载；nil 为新建。
    let existing: S3MountSpec?
    @Environment(\.dismiss) private var dismiss

    @State private var provider: S3Provider = .aws
    @State private var displayName = ""
    @State private var bucket = ""
    @State private var prefix = ""
    @State private var region = ""
    @State private var customEndpoint = ""
    @State private var authKind: AuthKind = .accessKey
    @State private var accessKey = ""
    @State private var secretKey = ""
    @State private var sessionToken = ""
    @State private var roleArn = ""
    @State private var externalId = ""
    @State private var profileName = ""
    @State private var policyChoice: PolicyChoice = .inherit
    @State private var ttlHours = 168
    @State private var readOnly = false
    @State private var testResult: String?
    @State private var busy = false

    init(model: MountsModel, existing: S3MountSpec? = nil) {
        self.model = model
        self.existing = existing
    }

    private var isEditing: Bool { existing != nil }

    private var mountId: String {
        if let existing { return existing.id }
        let base = (displayName.isEmpty ? bucket : displayName).lowercased()
        let cleaned = base.map { $0.isLetter || $0.isNumber ? $0 : "-" }
        return String(cleaned).trimmingCharacters(in: CharacterSet(charactersIn: "-"))
    }

    private var auth: S3AuthSpec {
        switch authKind {
        case .accessKey: return .static(accessKeyId: accessKey)
        case .assumeRole: return .assumeRole(roleArn: roleArn, externalId: externalId.isEmpty ? nil : externalId,
                                             sourceAccessKeyId: accessKey.isEmpty ? nil : accessKey)
        case .profile: return .profile(name: profileName)
        case .defaultChain: return .default
        }
    }

    private var policyOverride: CachePolicyFfi? {
        switch policyChoice {
        case .inherit: return nil
        case .ttl: return .ttl(seconds: UInt64(ttlHours) * 3600)
        case .always: return .always
        case .transient: return .transient
        }
    }

    /// 存入钥匙串的密钥：纯 Secret，或带 Session Token 的 JSON。
    private var secretPayload: String? {
        guard !secretKey.isEmpty else { return nil }
        if sessionToken.isEmpty { return secretKey }
        let obj: [String: String] = ["secret_access_key": secretKey, "session_token": sessionToken]
        return String(data: try! JSONSerialization.data(withJSONObject: obj), encoding: .utf8)
    }

    private var spec: S3MountSpec {
        S3MountSpec(
            id: mountId, displayName: displayName.isEmpty ? bucket : displayName,
            bucket: bucket, prefix: prefix,
            region: provider.sdkRegion(region),
            endpoint: provider.endpoint(region: region, custom: customEndpoint),
            forcePathStyle: provider.forcePathStyle,
            auth: auth, policyOverride: policyOverride, readOnly: readOnly)
    }

    private var canSave: Bool {
        !bucket.isEmpty && !mountId.isEmpty
            && (!provider.needsEndpoint || !customEndpoint.isEmpty)
            && (authKind != .accessKey || (!accessKey.isEmpty && (isEditing || !secretKey.isEmpty)))
            && (authKind != .assumeRole || !roleArn.isEmpty)
            && (authKind != .profile || !profileName.isEmpty)
    }

    var body: some View {
        Form {
            Section("存储") {
                Picker("服务商", selection: $provider) {
                    ForEach(S3Provider.allCases) { Text($0.rawValue).tag($0) }
                }
                .disabled(isEditing)
                TextField("显示名称", text: $displayName)
                TextField("Bucket", text: $bucket).disabled(isEditing)
                TextField("前缀（可选，只挂载 Bucket 的一部分）", text: $prefix).disabled(isEditing)
                if let label = provider.regionLabel {
                    TextField(label, text: $region).disabled(isEditing)
                }
                if provider.needsEndpoint {
                    TextField("Endpoint，如 http://127.0.0.1:9000", text: $customEndpoint).disabled(isEditing)
                }
                if isEditing {
                    Text("Bucket、前缀与 Endpoint 不可修改；需要更换请移除后重新添加。")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
            Section("凭据") {
                Picker("方式", selection: $authKind) {
                    ForEach(AuthKind.allCases) { Text($0.rawValue).tag($0) }
                }
                switch authKind {
                case .accessKey, .assumeRole:
                    TextField(authKind == .assumeRole ? "源 Access Key ID（留空用默认凭据链）" : "Access Key ID", text: $accessKey)
                    SecureField(isEditing ? "Secret Access Key（留空保留原值）" : "Secret Access Key", text: $secretKey)
                    SecureField("Session Token（临时凭据时填写）", text: $sessionToken)
                    if authKind == .assumeRole {
                        TextField("Role ARN", text: $roleArn)
                        TextField("External ID（可选）", text: $externalId)
                    }
                case .profile:
                    TextField("Profile 名称", text: $profileName)
                    Text("需要 App 能读取 ~/.aws；沙盒版本暂不支持，建议改用 Access Key。")
                        .font(.caption).foregroundStyle(.secondary)
                case .defaultChain:
                    Text("使用环境变量或实例角色，主要供命令行使用。").font(.caption).foregroundStyle(.secondary)
                }
            }
            Section("缓存") {
                Picker("策略", selection: $policyChoice) {
                    ForEach(PolicyChoice.allCases) { Text($0.rawValue).tag($0) }
                }
                if policyChoice == .ttl {
                    Stepper("有效期：\(ttlHours) 小时", value: $ttlHours, in: 1...24 * 365)
                }
                Toggle("只读", isOn: $readOnly)
            }
            if let testResult {
                Text(testResult).font(.caption).foregroundStyle(.secondary)
            }
            HStack {
                Button("测试连接") {
                    busy = true
                    Task {
                        switch await model.testConnection(spec: spec, secret: secretPayload) {
                        case .success(let n): testResult = "连接成功，根目录有 \(n) 项"
                        case .failure(let e): testResult = "连接失败：\(e)"
                        }
                        busy = false
                    }
                }
                Spacer()
                Button("取消") { dismiss() }
                Button(isEditing ? "保存" : "添加") {
                    busy = true
                    Task {
                        await model.save(spec: spec, secret: secretPayload)
                        busy = false
                        if model.lastError == nil { dismiss() }
                    }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(!canSave)
            }
            .disabled(busy)
        }
        .formStyle(.grouped)
        .frame(width: 480)
        .padding()
        .onAppear(perform: prefill)
    }

    private func prefill() {
        guard let e = existing else { return }
        provider = S3Provider.detect(e)
        displayName = e.displayName
        bucket = e.bucket
        prefix = e.prefix
        switch provider {
        case .aws, .aliyunOSS, .tencentCOS: region = e.region ?? ""
        case .cloudflareR2: region = e.endpoint?.replacingOccurrences(of: "https://", with: "")
            .replacingOccurrences(of: ".r2.cloudflarestorage.com", with: "") ?? ""
        case .minio, .custom: customEndpoint = e.endpoint ?? ""
        }
        switch e.auth {
        case .default: authKind = .defaultChain
        case .static(let ak): authKind = .accessKey; accessKey = ak
        case .assumeRole(let arn, let ext, let src): authKind = .assumeRole; roleArn = arn; externalId = ext ?? ""; accessKey = src ?? ""
        case .profile(let name): authKind = .profile; profileName = name
        }
        switch e.policyOverride {
        case nil: policyChoice = .inherit
        case .ttl(let s): policyChoice = .ttl; ttlHours = max(1, Int(s / 3600))
        case .always: policyChoice = .always
        case .transient: policyChoice = .transient
        }
        readOnly = e.readOnly
    }
}
