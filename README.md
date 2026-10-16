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
它会启动 versitygw 作为 S3 兼容服务，覆盖读写、分片上传、重命名、删除、外部变更可见性，
以及上传中 SIGKILL 后重挂载续传。

## 授权

核心库、后端、CLI、FUSE 以 Apache-2.0 发布。图形应用与系统扩展属于商用版。
