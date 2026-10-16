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

## 授权

核心库、后端、CLI、FUSE 以 Apache-2.0 发布。图形应用与系统扩展属于商用版。
