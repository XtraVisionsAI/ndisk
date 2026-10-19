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

/// 存储类型。S3 兼容服务（含阿里云 OSS、腾讯云 COS）走原生 S3 后端，其余走 OpenDAL。
enum StorageKind: String, CaseIterable, Identifiable {
    case s3 = "S3 兼容对象存储"
    case webdav = "WebDAV"
    case dropbox = "Dropbox"
    var id: String { rawValue }
}

/// 添加或编辑一个挂载。编辑时存储位置不可改（等同于换一个存储）。
struct MountFormView: View {
    @ObservedObject var model: MountsModel
    /// 编辑的现有挂载；nil 为新建。
    let existing: MountSpecAny?
    @Environment(\.dismiss) private var dismiss

    @State private var kind: StorageKind = .s3
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
    // WebDAV
    @State private var davURL = ""
    @State private var davRoot = ""
    @State private var davUser = ""
    @State private var davPassword = ""
    // Dropbox
    @State private var dropboxRoot = ""
    @State private var appKey = ""
    @State private var appSecret = ""
    @State private var dropboxToken = ""
    @State private var policyChoice: PolicyChoice = .inherit
    @State private var ttlHours = 168
    @State private var readOnly = false
    @State private var testResult: String?
    @State private var busy = false

    init(model: MountsModel, existing: MountSpecAny? = nil) {
        self.model = model
        self.existing = existing
    }

    private var isEditing: Bool { existing != nil }

    private var mountId: String {
        if let existing { return existing.id }
        let base = (displayName.isEmpty ? defaultName : displayName).lowercased()
        let cleaned = base.map { $0.isLetter || $0.isNumber ? $0 : "-" }
        return String(cleaned).trimmingCharacters(in: CharacterSet(charactersIn: "-"))
    }

    /// 未填显示名称时的默认名：Bucket、WebDAV 主机名或 "Dropbox"。
    private var defaultName: String {
        switch kind {
        case .s3: return bucket
        case .webdav: return URL(string: davURL)?.host ?? ""
        case .dropbox: return "Dropbox"
        }
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

    /// 存入钥匙串的密钥。S3：纯 Secret 或带 Session Token 的 JSON；WebDAV：密码；
    /// Dropbox：token（有 App Key 时为可续期的 refresh token，连同 App Secret 存为 JSON）。
    private var secretPayload: String? {
        switch kind {
        case .s3:
            guard !secretKey.isEmpty else { return nil }
            if sessionToken.isEmpty { return secretKey }
            return json(["secret_access_key": secretKey, "session_token": sessionToken])
        case .webdav:
            return davPassword.isEmpty ? nil : davPassword
        case .dropbox:
            guard !dropboxToken.isEmpty else { return nil }
            if appKey.isEmpty { return json(["access_token": dropboxToken]) }
            var obj = ["refresh_token": dropboxToken]
            if !appSecret.isEmpty { obj["client_secret"] = appSecret }
            return json(obj)
        }
    }

    private func json(_ obj: [String: String]) -> String {
        String(data: try! JSONSerialization.data(withJSONObject: obj, options: .sortedKeys), encoding: .utf8)!
    }

    private var name: String { displayName.isEmpty ? defaultName : displayName }

    private var spec: MountSpecAny {
        switch kind {
        case .s3:
            return .s3(S3MountSpec(
                id: mountId, displayName: name,
                bucket: bucket, prefix: prefix,
                region: provider.sdkRegion(region),
                endpoint: provider.endpoint(region: region, custom: customEndpoint),
                forcePathStyle: provider.forcePathStyle,
                auth: auth, policyOverride: policyOverride, readOnly: readOnly))
        case .webdav:
            return .opendal(OpendalMountSpec(
                id: mountId, displayName: name, scheme: "webdav",
                options: ["endpoint": davURL, "username": davUser, "root": davRoot],
                policyOverride: policyOverride, readOnly: readOnly))
        case .dropbox:
            return .opendal(OpendalMountSpec(
                id: mountId, displayName: name, scheme: "dropbox",
                options: ["root": dropboxRoot, "client_id": appKey],
                policyOverride: policyOverride, readOnly: readOnly))
        }
    }

    private var canSave: Bool {
        guard !mountId.isEmpty else { return false }
        switch kind {
        case .s3:
            return !bucket.isEmpty
                && (!provider.needsEndpoint || !customEndpoint.isEmpty)
                && (authKind != .accessKey || (!accessKey.isEmpty && (isEditing || !secretKey.isEmpty)))
                && (authKind != .assumeRole || !roleArn.isEmpty)
                && (authKind != .profile || !profileName.isEmpty)
        case .webdav:
            return URL(string: davURL)?.host != nil
        case .dropbox:
            return isEditing || !dropboxToken.isEmpty
        }
    }

    var body: some View {
        Form {
            Section {
                Picker("类型", selection: $kind) {
                    ForEach(StorageKind.allCases) { Text($0.rawValue).tag($0) }
                }
                .disabled(isEditing)
                TextField("显示名称", text: $displayName)
            }
            switch kind {
            case .s3: s3Sections
            case .webdav: webdavSection
            case .dropbox: dropboxSection
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

    @ViewBuilder private var s3Sections: some View {
        Section("存储") {
            Picker("服务商", selection: $provider) {
                ForEach(S3Provider.allCases) { Text($0.rawValue).tag($0) }
            }
            .disabled(isEditing)
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
    }

    private var webdavSection: some View {
        Section("WebDAV") {
            TextField("地址，如 https://nas.local/dav", text: $davURL).disabled(isEditing)
            TextField("子路径（可选）", text: $davRoot).disabled(isEditing)
            TextField("用户名", text: $davUser)
            SecureField(isEditing ? "密码（留空保留原值）" : "密码", text: $davPassword)
            Text("坚果云：https://dav.jianguoyun.com/dav/（使用应用密码）\nNextcloud：https://主机/remote.php/dav/files/用户名/")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private var dropboxSection: some View {
        Section("Dropbox") {
            TextField("子路径（可选）", text: $dropboxRoot).disabled(isEditing)
            TextField("App Key（可选，填写后 token 视为 refresh token）", text: $appKey)
            SecureField("App Secret（可选）", text: $appSecret)
            SecureField(isEditing ? "Token（留空保留原值）" : (appKey.isEmpty ? "Access Token" : "Refresh Token"),
                        text: $dropboxToken)
            Text("在 Dropbox App Console 创建应用后生成 token。长期使用建议填写 App Key 与 refresh token，access token 数小时后过期。")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private func prefill() {
        switch existing {
        case nil: return
        case .s3(let e):
            kind = .s3
            prefillS3(e)
            prefillCommon(name: e.displayName, policy: e.policyOverride, readOnly: e.readOnly)
        case .opendal(let e):
            let o = e.options
            switch e.scheme {
            case "dropbox":
                kind = .dropbox
                dropboxRoot = o["root"] ?? ""
                appKey = o["client_id"] ?? ""
            default:
                kind = .webdav
                davURL = o["endpoint"] ?? ""
                davRoot = o["root"] ?? ""
                davUser = o["username"] ?? ""
            }
            prefillCommon(name: e.displayName, policy: e.policyOverride, readOnly: e.readOnly)
        }
    }

    private func prefillCommon(name: String, policy: CachePolicyFfi?, readOnly ro: Bool) {
        displayName = name
        switch policy {
        case nil: policyChoice = .inherit
        case .ttl(let s): policyChoice = .ttl; ttlHours = max(1, Int(s / 3600))
        case .always: policyChoice = .always
        case .transient: policyChoice = .transient
        }
        readOnly = ro
    }

    private func prefillS3(_ e: S3MountSpec) {
        provider = S3Provider.detect(e)
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
    }
}
