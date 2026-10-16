# ndisk

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
export NDISK_S3_SECRET=...        # 骨架阶段静态密钥从环境变量读取
mkdir -p ~/mnt/s3
ndisk -c my.toml mount ~/mnt/s3   # 前台运行，fusermount3 -u ~/mnt/s3 卸载
```

运行时依赖 `fuse3`（提供 fusermount3）。端到端测试在 Docker 中进行：`scripts/e2e/e2e.sh`，
它会启动 versitygw 作为 S3 兼容服务，覆盖读写、分片上传、重命名、删除、外部变更可见性、
上传中 SIGKILL 后重挂载续传、非 ASCII 文件名、分片服务端复制、HTTP 代理、缓存策略与 TTL 回收。

## macOS App 与 File Provider 扩展

需要 Xcode 15+（已在 Xcode 27 验证）、Rust 的 `aarch64-apple-darwin`（Intel 版另需 `x86_64-apple-darwin`）、XcodeGen。

```sh
apple/scripts/build-rust.sh                     # Rust 核心 → NdiskFFI.xcframework + Swift 绑定
cd apple/NdiskKit && swift test                 # Swift 侧单元测试
NDISK_TEST_S3_ENDPOINT=http://127.0.0.1:7070 swift test   # 加上真实 S3 往返测试
apple/scripts/build-app.sh --unsigned           # 不签名编译 App 与扩展
open apple/NDisk.xcodeproj                      # 或在 Xcode 中构建运行（scheme 预操作会先构建 Rust）
```

签名运行前，在 `apple/Config/Signing.local.xcconfig` 中填写：

```
NDISK_TEAM_ID = <你的 Team ID>
NDISK_BUNDLE_PREFIX = ai.xtravisions.ndisk
```

## 缓存控制

```sh
ndisk -c my.toml cache policy /photos ttl:86400   # 目录下文件闲置 1 天后回收
ndisk -c my.toml cache policy /docs always        # 等同 pin：递归下载并始终保留
ndisk -c my.toml cache policy /tmp none           # 读完即可回收
ndisk -c my.toml cache policy /photos inherit     # 清除覆盖，继承上级
ndisk -c my.toml cache gc                         # 立即执行一次维护
```

## 授权

核心库、后端、CLI、FUSE 以 Apache-2.0 发布。图形应用与系统扩展属于商用版。
