# ndisk 设计文档

> 状态：草案 v0.1（2026-10-16）。本文记录已确认的产品决策与技术架构，是后续实现的依据。

## 1. 目标与边界

**目标**：把 S3 / OSS / WebDAV / Dropbox 等网络存储呈现为操作系统可直接使用的本地文件位置，带本地缓存，文件管理完全交给操作系统。

**已确认的产品决策**

| 议题 | 决策 |
|---|---|
| macOS 呈现方式 | File Provider（Finder 侧栏位置，路径 `~/Library/CloudStorage/<Domain>`），不做 macFUSE |
| 签名与分发 | Developer ID / App Store，File Provider entitlement 后续提供 |
| 用户形态 | 个人单机使用，不做多人协同锁 |
| 首期后端 | S3，兼容 MinIO / R2 / OSS 等 S3 兼容接口；认证支持 AK/SK、Profile、环境变量、STS AssumeRole、SSO、Web Identity |
| 读写 | 必须支持读写，大文件断点续传 |
| Linux | CLI + FUSE 必做；桌面托盘与角标作为增强 |
| Windows | Cloud Files API，M4 阶段 |
| 核心语言 | Rust |
| 最低系统 | macOS 12.3+、Windows 10 1709+、Linux 需 FUSE3 |
| 缓存加密 | 不需要 |
| 网络 | 支持 HTTP(S) 代理、上传/下载限速 |
| 授权 | 开源版 + 商用版（open core），第三方依赖只用商业友好许可（MIT / Apache-2.0 / BSD / MPL-2.0），不引入 GPL / AGPL |

**非目标**

- 不做文件浏览器、同步对比、分享链接管理等额外文件管理功能。
- 不做多人实时协作、分布式锁。
- 不做端到端加密或本地缓存加密。

## 2. 总体架构

```
┌───────────────────────────────────────────────────────────────┐
│ 平台适配层（薄，各平台原生）                                      │
│  macOS  : File Provider Extension (Swift) + 菜单栏 App (SwiftUI) │
│  Windows: Cloud Files API 同步根 (Rust windows-rs) + 托盘 App     │
│  Linux  : FUSE (fuser) + CLI；可选托盘 + Nautilus/Dolphin 角标插件 │
├───────────────────────────────────────────────────────────────┤
│ ndisk-ffi  : UniFFI 生成 Swift/Kotlin 绑定；平台层只调这一层       │
├───────────────────────────────────────────────────────────────┤
│ ndisk-core : 跨平台核心引擎（Rust）                               │
│   vfs       目录树、ItemId 分配、路径 <-> 对象键映射              │
│   cache     元数据缓存（SQLite）+ 内容缓存（分块文件，LRU/TTL/固定）│
│   writeback 写回队列、分片上传、断点续传、冲突检测                 │
│   sched     任务调度、并发上限、重试、限速、离线队列               │
│   config    挂载配置、缓存策略、代理                              │
├───────────────────────────────────────────────────────────────┤
│ Backend trait : 存储后端抽象                                     │
│   ndisk-backend-s3      aws-sdk-s3，首期                          │
│   ndisk-backend-opendal Apache OpenDAL，一次接入 OSS/WebDAV/Dropbox…│
└───────────────────────────────────────────────────────────────┘
```

**设计原则**

1. 所有与"正确性"有关的逻辑（缓存一致性、写回、断点续传、冲突）只在 `ndisk-core` 写一遍，三个平台共用。
2. 平台层只负责把系统回调翻译成核心引擎的调用，不持有状态。
3. 后端只做"对象级"操作，不理解目录树和缓存；语义抹平在 `vfs` 层完成。
4. 任何可能跨进程重启的状态（上传进度、脏文件列表、固定集合）都持久化到 SQLite，而不是只放内存。

## 3. 核心引擎

### 3.1 Backend trait

后端统一抽象为一个扁平的对象存储接口。目录是上层概念，后端只需要支持"按前缀分页列举"。

```rust
#[async_trait]
pub trait Backend: Send + Sync {
    fn capabilities(&self) -> Capabilities;         // 是否原生 rename、range 读、分片上传、ETag…
    async fn list(&self, prefix: &str, cursor: Option<&str>) -> Result<ListPage>;
    async fn stat(&self, key: &str) -> Result<Option<Entry>>;
    async fn read(&self, key: &str, range: Range<u64>) -> Result<Bytes>;
    async fn put(&self, key: &str, data: Bytes, opts: PutOptions) -> Result<Entry>;
    async fn multipart_start(&self, key: &str) -> Result<UploadId>;
    async fn multipart_put_part(&self, key: &str, upload: &UploadId, part: u32, data: Bytes) -> Result<PartEtag>;
    async fn multipart_complete(&self, key: &str, upload: &UploadId, parts: &[(u32, PartEtag)]) -> Result<Entry>;
    async fn multipart_abort(&self, key: &str, upload: &UploadId) -> Result<()>;
    async fn delete(&self, key: &str) -> Result<()>;
    async fn copy(&self, from: &str, to: &str) -> Result<Entry>;
}
```

`Entry` 携带 `key / kind / size / mtime / etag / version`。`etag` 是缓存失效与冲突检测的唯一依据。

### 3.2 S3 语义映射

| 文件系统概念 | S3 实现 |
|---|---|
| 目录 | 前缀。显式目录用零字节对象 `dir/` 标记，兼容 s3fs / rclone 约定；只有隐式前缀的目录也要能被列出 |
| 列目录 | `ListObjectsV2` + `Delimiter=/`，分页惰性枚举，不一次性拉整个 Bucket |
| stat | `HeadObject`；失败时回退到前缀列举判断是否为隐式目录 |
| 读 | `GetObject` + `Range`，按块读取填充内容缓存 |
| 写 | 永远先写本地缓存文件；close 后进入写回队列。小于 `multipart_threshold`（默认 16 MiB）用 `PutObject`，否则分片上传 |
| rename | 文件：`CopyObject`（>5 GiB 用 `UploadPartCopy`）后删除；目录：递归 |
| mtime / 权限 | 存在用户元数据 `x-amz-meta-mtime` 等，兼容 s3fs 字段名；权限位不强求保存 |
| 冲突检测 | 上传时带 `If-Match: <本地已知 etag>`；返回 412 则保留本地为 `name (conflicted copy YYYY-MM-DD).ext` 并拉取远端版本 |

### 3.3 缓存

**元数据缓存**（SQLite）

- 表 `nodes`：`item_id, parent_id, name, kind, size, mtime, etag, remote_key, listed_at, stat_at, state`
- 目录列表与 stat 分别有 TTL（默认目录 30 s、stat 60 s），可按挂载点配置。
- File Provider 的 `ItemIdentifier` 与 FUSE 的 `inode` 都映射到 `item_id`，重命名不改变 `item_id`。

**内容缓存**（分块文件）

- 布局：`<cache_root>/<mount_id>/blocks/<item_id>/<etag>/<block_no>`，块大小默认 8 MiB。
- 任意 `etag` 变化即整体失效，不做局部比对。
- 三种策略，按挂载点设默认值，可对目录 / 文件单独覆盖：
  - `ttl(duration)`：最后访问超过时长后由后台驱逐器删除本地块；
  - `always`：固定，不驱逐，元数据变化时主动预取；
  - `none`：只在读取过程中短暂驻留，读完即可回收。
- 容量上限 + LRU 作为所有策略之上的硬约束。
- "文件已在本地 / 仍在云端"的判定：该 `item_id` 当前 `etag` 下所有块齐全即为已缓存，这个状态同时驱动 Finder 角标和 CLI 输出。

### 3.4 写回与断点续传

- 打开写入时在 `<cache_root>/<mount_id>/dirty/<item_id>` 创建完整的本地副本（首次写入前若远端有内容则先拉全量）。
- close 后登记到表 `uploads`：`item_id, upload_id, part_size, parts_done(json), base_etag, state`。
- 分片上传每完成一个 part 就更新 `parts_done`；进程重启后扫描 `uploads` 表，用同一 `upload_id` 继续，未完成的 part 重传。
- 上传期间文件被再次修改：中止当前上传，重新排队（debounce 2 s）。
- 下载同样按块落盘，重启后缺哪个块补哪个块，天然断点续传。
- 离线：写入全部成功落盘并排队，联网后按顺序上传；读取只能命中已缓存内容，未缓存返回 `EIO`/`notConnected`。

### 3.5 调度、限速、代理

- 全局 Tokio runtime，一个按挂载点隔离的任务队列；默认并发：下载 8、上传 4、元数据 16。
- 限速：令牌桶，分别作用于上传、下载字节流，可在运行时调整。
- 重试：指数退避，区分可重试错误（5xx、限流、超时）与不可重试（403、404、412）。
- 代理：系统代理 / 自定义 HTTP(S) 代理，通过自定义 hyper connector 注入 SDK；SDK 默认不读环境变量代理，需要我们显式处理。

### 3.6 认证

通过 `aws-config` 的 credential provider chain 支持：静态 AK/SK、`~/.aws/credentials` Profile、环境变量、`AssumeRole`（STS）、SSO（IAM Identity Center）、Web Identity。自定义 endpoint + `force_path_style` 开关用于 MinIO / OSS / R2 等兼容实现。密钥只存系统凭据库（Keychain / Credential Manager / libsecret），配置文件里只存引用名。

## 4. 平台适配

### 4.1 macOS File Provider

`NSFileProviderReplicatedExtension` 的回调与核心引擎的对应：

| File Provider 回调 | 核心引擎 |
|---|---|
| `enumerator(for:)` / `enumerateItems` | `vfs.list(item_id, cursor)`，分页返回 |
| `enumerateChanges(from anchor)` | `vfs.changes_since(anchor)`，anchor 为本地变更序号 |
| `item(for:)` | `vfs.stat(item_id)` |
| `fetchContents(for:)` | `cache.ensure_full(item_id)`，块级下载并报告进度 |
| `createItem` / `modifyItem`（内容） | 写入 dirty 副本 → `writeback.enqueue` |
| `modifyItem`（重命名 / 移动） | `vfs.rename` |
| `deleteItem` | `vfs.delete` |
| `materializedItemsDidChange` / `evict` | 系统驱逐与我们的 TTL 驱逐共用 `cache.evict(item_id)` |
| 角标 | 系统根据 materialized 状态自动显示云朵 / 本地；上传中由 `isUploading` 字段驱动 |

自定义右键动作（`NSFileProviderAction`）：立即下载、固定为始终缓存、取消固定、清理本地缓存、复制对象 URL。系统自带"移除下载"。

菜单栏 App 负责：账户管理、挂载点配置、缓存策略、传输队列与速度、限速与代理。扩展进程和 App 进程通过 UniFFI 直接调用同一份核心库，状态落在共享 App Group 容器的 SQLite 中。

### 4.2 Linux

- `ndisk-fuse`：基于 `fuser`（MIT）实现 FUSE3，inode 即 `item_id`，默认 `allow_other` 关闭。
- `ndisk` CLI：`mount / umount / ls / status / cache pin|unpin|evict / config / auth`。
- 桌面增强（可选）：托盘用 `ksni` 或 `tray-icon`；角标通过 Nautilus Python 扩展 / Dolphin 插件读取 CLI 暴露的 D-Bus 状态接口。

### 4.3 Windows

Cloud Files API（`CfRegisterSyncRoot`、占位符、`CfHydratePlaceholder`），概念与 File Provider 一一对应，使用 `windows-rs` 直接在 Rust 中实现，不需要额外的 C++ 层。

## 5. 仓库与授权

```
ndisk/
├── Cargo.toml                 # workspace
├── docs/DESIGN.md
├── crates/
│   ├── ndisk-core/            # 核心引擎（Apache-2.0）
│   ├── ndisk-backend-s3/      # S3 后端（Apache-2.0）
│   ├── ndisk-cli/             # CLI（Apache-2.0）
│   ├── ndisk-fuse/            # Linux FUSE（Apache-2.0）
│   └── ndisk-ffi/             # UniFFI 绑定（Apache-2.0）
└── apple/                     # Xcode 工程：App + File Provider 扩展（商用版私有仓库，开源版可提供精简版）
```

- 开源版：核心库、后端、CLI、FUSE 以 Apache-2.0 发布。
- 商用版：macOS / Windows 图形应用、系统扩展、安装与自动更新，闭源。
- 依赖许可白名单：MIT、Apache-2.0、BSD-2/3、MPL-2.0、ISC、Zlib。CI 用 `cargo-deny` 强制检查。

当前关键依赖及许可：`tokio`(MIT)、`aws-sdk-s3`(Apache-2.0)、`opendal`(Apache-2.0)、`fuser`(MIT)、`rusqlite`(MIT)、`uniffi`(MPL-2.0)、`clap`(MIT/Apache-2.0)。

## 6. 里程碑

| 阶段 | 内容 | 验收 |
|---|---|---|
| M1 | 核心引擎 + S3 后端 + CLI + Linux FUSE | Linux 下挂载 MinIO 读写，断电重启后上传续传 |
| M2 | macOS File Provider 扩展 + 菜单栏 App | Finder 中可见角标、按需下载、TTL 驱逐、右键动作 |
| M3 | OpenDAL 后端接入 OSS / WebDAV / Dropbox | 同一套 CLI 挂载三种后端通过回归测试 |
| M4 | Windows Cloud Files API | 资源管理器中占位符、按需下载、角标 |

## 7. 待决事项

- 目录标记对象：默认写 `dir/` 零字节对象还是只依赖隐式前缀？倾向写标记以兼容空目录。
- 冲突副本命名与是否通知用户。
- 大 Bucket（百万级对象）的首次枚举体验：是否限制根目录直接挂载，推荐挂载前缀。
- Linux 角标插件的具体形态，视桌面环境需求再定。
