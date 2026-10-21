# ndisk

[![CI](https://github.com/XtraVisionsAI/ndisk/actions/workflows/ci.yml/badge.svg)](https://github.com/XtraVisionsAI/ndisk/actions/workflows/ci.yml)

把 S3 / OSS / WebDAV / Dropbox 等网络存储挂载为本地文件位置，带本地缓存与断点续传。

- macOS：File Provider（Finder 侧栏位置，原生角标与按需下载）
- Linux：FUSE + CLI
- Windows：Cloud Files API（计划中）

设计文档见 [docs/DESIGN.md](docs/DESIGN.md)。

## 构建

需要 Rust 1.94 以上（aws-sdk 要求）。workspace 使用 resolver 3，`rust-version` 为 1.93 时会自动选择兼容的依赖版本。

```sh
cargo build --workspace
cargo run -p ndisk-cli -- --help
```

## Linux 挂载

```sh
ndisk config > my.toml            # 生成配置模板后修改 bucket / endpoint / auth
export NDISK_SECRET=...           # 骨架阶段密钥从环境变量读取（S3 / OpenDAL 后端通用）
mkdir -p ~/mnt/s3
ndisk -c my.toml mount ~/mnt/s3   # 前台运行，fusermount3 -u ~/mnt/s3 卸载
```

WebDAV / Dropbox / 原生 OSS 走 OpenDAL 后端，配置示例（密钥同样经 `NDISK_SECRET` 提供）：

```toml
id = "nas"
display_name = "NAS"

[backend]
type = "opendal"
scheme = "webdav"            # 或 dropbox / oss
secret_ref = "env"

[backend.options]            # OpenDAL 的服务配置项
endpoint = "https://nas.local/dav"
username = "me"
root = "/"
```

运行时依赖 `fuse3`（提供 fusermount3）。端到端测试在 Docker 中进行：`scripts/e2e/e2e.sh`，
它会启动 versitygw 作为 S3 兼容服务，覆盖读写、分片上传、重命名、删除、外部变更可见性、
上传中 SIGKILL 后重挂载续传、非 ASCII 文件名、分片服务端复制、HTTP 代理、缓存策略与 TTL 回收；
随后在容器内起 Apache mod_dav，用 WebDAV 后端复跑同一套读写检查（`scripts/e2e/e2e.sh webdav` 只跑这一段）。

## macOS App 与 File Provider 扩展

需要 Xcode 15+（已在 Xcode 27 验证）、Rust 的 `aarch64-apple-darwin`（Intel 版另需 `x86_64-apple-darwin`）、XcodeGen。

```sh
apple/scripts/build-rust.sh                     # Rust 核心 → NdiskFFI.xcframework + Swift 绑定
cd apple/NdiskKit && swift test                 # Swift 侧单元测试
NDISK_TEST_S3_ENDPOINT=http://127.0.0.1:7070 swift test   # 加上真实 S3 往返测试
apple/scripts/build-app.sh --unsigned           # 不签名编译 App 与扩展
open apple/NDisk.xcodeproj                      # 或在 Xcode 中构建运行（scheme 预操作会先构建 Rust）
```

运行时 App 必须位于 `/Applications` 或 `~/Applications`，且首次需在"系统设置 → 通用 → 登录项与扩展 → 文件提供程序"中启用 NDisk。
签名运行前，在 `apple/Config/Signing.local.xcconfig` 中填写：

```
NDISK_TEAM_ID = <你的 Team ID>
NDISK_BUNDLE_PREFIX = ai.xtravisions.ndisk
```

全局设置（代理、限速、缓存上限、默认策略）在菜单栏"设置…"中；每个挂载可编辑显示名、凭据、策略覆盖与只读，可断开 / 连接而不删除配置。

### 发布（签名、公证、DMG）

```sh
apple/scripts/release.sh --dev            # 演练：开发证书导出 + DMG，不公证
apple/scripts/release.sh                  # 正式：Developer ID 签名，App 与 DMG 分别公证并装订
```

正式发布的一次性准备：在 Xcode → Settings → Accounts 登录团队账号，并在 Manage Certificates 中创建
Developer ID Application 证书（需 Account Holder 或 Admin 角色；共享钥匙串所需的描述文件由 Xcode 在导出时自动获取）；
用 app 专用密码保存公证凭据：

```sh
xcrun notarytool store-credentials ndisk-notary --apple-id <Apple ID> --team-id <Team ID> --password <app 专用密码>
```

自动更新用 Sparkle（EdDSA 签名）。一次性生成更新签名密钥（私钥存于登录钥匙串，**务必备份**，丢失后已安装的用户将无法再收到更新），
把打印出的公钥填入 `apple/Config/NDisk.xcconfig` 的 `NDISK_SPARKLE_PUBLIC_KEY`：

```sh
SPARKLE=target/xcode/SourcePackages/artifacts/sparkle/Sparkle/bin   # 构建一次 App 后出现
$SPARKLE/generate_keys                     # 生成并打印公钥
$SPARKLE/generate_keys -x ~/ndisk-sparkle-private.key   # 导出私钥备份（妥善离线保存）
```

每次发布前递增 `MARKETING_VERSION` 与 `CURRENT_PROJECT_VERSION`。产物为 `target/release-app/NDisk-<版本>.dmg`（arm64 + x86_64 通用）、
`.sha256` 与 `appcast.xml`，按脚本最后打印的 `gh release create` 命令上传到 GitHub Release `v<版本>`；
App 的更新源固定为 `releases/latest/download/appcast.xml`。

## 缓存控制

```sh
ndisk -c my.toml cache policy /photos ttl:86400   # 目录下文件闲置 1 天后回收
ndisk -c my.toml cache policy /docs always        # 等同 pin：递归下载并始终保留
ndisk -c my.toml cache policy /tmp none           # 读完即可回收
ndisk -c my.toml cache policy /photos inherit     # 清除覆盖，继承上级
ndisk -c my.toml cache gc                         # 立即执行一次维护
```

## 授权

全部代码（核心库、后端、CLI、FUSE、macOS App 与 File Provider 扩展）以 [Apache License 2.0](LICENSE) 发布，见 [NOTICE](NOTICE)。
