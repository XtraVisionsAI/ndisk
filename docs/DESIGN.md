# ndisk 设计文档

> 状态：草案 v0.1（2026-10-16）。本文记录已确认的产品决策与技术架构，是后续实现的依据。

## 1. 目标与边界

**目标**：把 S3 / OSS / WebDAV / Dropbox 等网络存储呈现为操作系统可直接使用的本地文件位置，带本地缓存，文件管理完全交给操作系统。

**已确认的产品决策**

| 议题 | 决策 |
|---|---|
| macOS 呈现方式 | File Provider（Finder 侧栏位置，路径 `~/Library/CloudStorage/<Domain>`），不做 macFUSE；最低 macOS 13（见 4.1） |
| 目录布局 | 遵循系统惯例：持久数据（元数据库、未上传文件、配置）在 Application Support / `$XDG_DATA_HOME` / `%APPDATA%`，可重建的块缓存在 Caches / `$XDG_CACHE_HOME` / `%LOCALAPPDATA%`；不开放自定义 |
| 签名与分发 | Developer ID / App Store，File Provider entitlement 后续提供 |
| 用户形态 | 个人单机使用，不做多人协同锁 |
| 首期后端 | S3，兼容 MinIO / R2 / OSS 等 S3 兼容接口；认证支持 AK/SK、Profile、环境变量、STS AssumeRole、SSO、Web Identity |
| 读写 | 必须支持读写，大文件断点续传 |
| Linux | CLI + FUSE 必做；桌面托盘与角标作为增强 |
| Windows | Cloud Files API，M4 阶段 |
| 核心语言 | Rust |
| 最低系统 | macOS 13+（原定 12.3，因右键动作与 contentPolicy 提高）、Windows 10 1709+、Linux 需 FUSE3 |
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
| rename | 文件：`CopyObject` 后删除，超过 `copy_threshold`（默认 5 GiB）用 `UploadPartCopy` 按 512 MiB 分片复制；`CopySource` 需 URL 编码以支持中文、空格等键名；目录：递归 |
| mtime / 权限 | 存在用户元数据 `x-amz-meta-mtime` 等，兼容 s3fs 字段名；权限位不强求保存 |
| 冲突检测 | 上传时带 `If-Match: <本地已知 etag>`；返回 412 则保留本地为 `name (conflicted copy YYYY-MM-DD).ext` 并拉取远端版本 |

### 3.2.1 OpenDAL 后端（OSS / WebDAV / Dropbox）

`ndisk-backend-opendal` 用 Apache OpenDAL 0.59 一次接入多种服务，配置为 `type = "opendal"`、`scheme`、`options`（即 OpenDAL 的服务配置项，`root` 表示挂载子路径）与 `secret_ref`。钥匙串中的密钥可为纯字符串（填入该服务默认密钥项：WebDAV `password`、OSS `access_key_secret`、Dropbox 有 `client_id` 时为 `refresh_token`，否则为 `access_token`），或 JSON 对象按键合并进选项。

| 能力 | 处理 |
|---|---|
| 列目录 | OpenDAL lister（WebDAV 为 PROPFIND Depth 1），过滤掉目录自身；一次返回整页 |
| ETag | 去掉 `W/` 与引号后使用：Apache 在 PROPFIND 返回弱 ETag、HEAD 返回强 ETag，不统一会被误判为冲突；写入返回的元数据不全时补一次 stat |
| 条件写入 | 服务原生支持 `if_match` / `if_not_exists` 时直接用；否则（WebDAV、Dropbox、OSS 的 if_match）先 stat 比对版本键再写，有竞争窗口，但能覆盖"另一台设备改过"的常见场景 |
| 上传 | 支持分片写入的服务（OSS）经 `put_file` 按 8 MiB 流式写；WebDAV / Dropbox 只接受一次性写入，整文件读入内存后 PUT。分片续传需要显式 upload id，OpenDAL 不暴露，因此这些后端不支持跨重启续传，失败后整体重传 |
| rename | 文件用原生 MOVE / move（OSS 为 copy+delete）；OpenDAL 的 rename 不接受目录，目录由 vfs 逐个子项递归 |
| 代理 | 每个 Operator 注入独立的 reqwest 客户端（`network.proxy` 取值同 S3），另加 120 s 请求 / 60 s IO 超时层 |

S3 兼容服务（含阿里云 OSS 的 S3 接口）在界面中仍走原生 S3 后端；原生 OSS（STS、RAM 角色）目前只在 CLI 配置中提供。

### 3.3 缓存

**元数据缓存**（SQLite）

- 表 `nodes`：`item_id, parent_id, name, kind, size, mtime, etag, remote_key, listed_at, stat_at, state`
- 目录列表与 stat 分别有 TTL（默认目录 30 s、stat 60 s），可按挂载点配置。
- File Provider 的 `ItemIdentifier` 与 FUSE 的 `inode` 都映射到 `item_id`，重命名不改变 `item_id`。

**内容缓存**（分块文件）

- 布局：`<cache_root>/<mount_id>/blocks/<item_id>/<etag>/<block_no>`，块大小默认 8 MiB。
- 任意 `etag` 变化即整体失效，不做局部比对。部分 S3 兼容实现（如 versitygw posix 后端对外部写入的文件）HEAD 不返回 etag，此时版本键退化为 `mtime+size`；If-Match 冲突检测仍只使用真实 etag。
- 三种策略，按挂载点设默认值，可对目录 / 文件单独覆盖（生效顺序：固定标记 > 节点自身覆盖 > 最近祖先目录覆盖 > 挂载默认）：
  - `ttl(duration)`：最后访问超过时长后由后台驱逐器删除本地块；
  - `always`：固定，不驱逐，元数据变化时主动预取；
  - `none`：只在读取过程中短暂驻留，60 秒宽限期后回收。
- 容量上限 + LRU 作为所有策略之上的硬约束。
- 驱逐以文件为粒度（与 Finder 的"云端 / 已下载"状态一致），由 `Engine::maintain` 周期执行：先 TTL，再按 LRU 压到容量上限，最后为固定的文件与目录补齐下载（固定目录下新出现的文件也会被预取）。始终缓存的文件与有未上传修改的文件永不驱逐。
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
- 代理：`network.proxy` 取值 `env` / `system`（读取 `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY`，macOS 系统代理由平台层转换为环境变量）或 `http://[user:pass@]host:port`。通过 aws-smithy-http-client 的 `ProxyConfig` 构造自定义连接器注入 SDK，STS / SSO 凭据请求同样走代理。

### 3.6 认证

通过 `aws-config` 的 credential provider chain 支持：静态 AK/SK、`~/.aws/credentials` Profile、环境变量、`AssumeRole`（STS）、SSO（IAM Identity Center）、Web Identity。自定义 endpoint + `force_path_style` 开关用于 MinIO / OSS / R2 等兼容实现。密钥只存系统凭据库（Keychain / Credential Manager / libsecret），配置文件里只存引用名。

## 4. 平台适配

### 4.1 macOS File Provider

**最低系统 macOS 13**：自定义右键动作（`NSFileProviderCustomAction`）、`contentPolicy`、`requestDownloadForItem` 均要求 13.0，因此部署目标从原定的 12.3 提高到 13.0。

**工程结构**（`apple/`，由 XcodeGen 的 `project.yml` 生成 `NDisk.xcodeproj`）：

| 部分 | 内容 |
|---|---|
| `NdiskKit`（Swift Package） | `NdiskFFI.xcframework`（Rust 静态库 + UniFFI 头文件）、生成的 Swift 绑定、标识符互转与错误映射、App Group 存储与钥匙串 |
| `NDisk`（菜单栏 App） | 挂载列表、添加挂载（含测试连接）、注册 / 移除 domain、定期唤醒工作集 |
| `NDiskFileProvider`（扩展） | `NSFileProviderReplicatedExtension` + `NSFileProviderCustomAction`，一个 domain 一个实例，domain id 即挂载 id |
| `Config/NDisk.xcconfig` | Team ID、Bundle 前缀、App Group、钥匙串组；真实值写入 gitignore 的 `Signing.local.xcconfig` |

**内容由系统持有**：replicated 模型下系统把 `fetchContents` 交出的文件保存在自己的存储中，"已下载 / 云端"状态与角标由系统维护。因此：

- `fetchContents` 把内容导出到 staging 文件交给系统，随即清掉引擎块缓存（固定内容除外），避免双份占用；
- "始终保留在本机"映射为 `contentPolicy = .downloadEagerlyAndKeepDownloaded`，子项为 `.inherited`，由系统负责下载与保持；引擎只记录策略（`set_policy_only`），不预取；
- TTL 从下载时刻起算：引擎在 `materialized` 表记录下载时间，`materializedItemsDidChange` 时与系统的已下载集合对齐，`eviction_candidates` 给出到期项，由扩展调用 `NSFileProviderManager.evictItem`；
- 菜单栏 App 每 10 分钟 `signalEnumerator(.workingSet)`，唤醒扩展上报变更并执行一次驱逐。

**会话归属**：同一挂载的引擎会话只由扩展持有，App 不为已登记挂载打开会话，避免两个写回队列重复上传；App 的"测试连接"使用临时缓存目录。

**回调对应**：

| File Provider 回调 | Rust（`ndisk-ffi::Session`） |
|---|---|
| `item(for:)` | `item(id)`，只查本地 |
| `enumerator(for:)` → `enumerateItems` | `list(dir, force)`，首页强制刷新 |
| 工作集 `enumerateChanges(from:)` | `changes_since(anchor)`，锚点为 8 字节大端序 u64 |
| `currentSyncAnchor` | `current_anchor()` |
| `fetchContents(for:)` | `fetch_contents(id, dest)` |
| `createItem` | `create_item(parent, name, kind, contents)` |
| `modifyItem`（内容 / 重命名 / 移动） | `modify_contents` / `rename` |
| `deleteItem`（含 `.recursive`） | `delete`，递归由扩展展开 |
| `materializedItemsDidChange` | `sync_materialized` + `eviction_candidates` + `mark_evicted` |
| 右键动作 | 立即下载（系统 `requestDownloadForItem`）、始终保留 / 取消（`pin` / `unpin`）、清理本地缓存（系统 `evictItem`） |
| 角标 | 云端 / 已下载 / 上传中由系统显示；固定项用自定义装饰 `PinnedBadge` |

**真机运行的三个前提**（2026-10-16 验证）：

1. App 必须从 `/Applications` 或 `~/Applications` 运行。从构建目录运行时 fileproviderd 拒绝加载扩展（domain 一直是 disabled）。
2. 新装的第三方 File Provider 默认禁用，用户须在"系统设置 → 通用 → 登录项与扩展 → 文件提供程序"中启用，否则 Finder 提示"没有权限打开"。
3. 根条目（`.rootContainer`）的 `filename` 不能为空，否则扩展被系统断言中止（`__FILEPROVIDER_BAD_ITEM_MISSING_FILENAME__`）；用 domain 显示名。

已在真实 AWS Bucket 上验证：列目录、已有对象为 dataless（云端角标）、mkdir、写入与读回、5 MiB 复制、重命名（含中文）、移动到子目录；Finder 中人工确认了右键四项动作、图钉角标、菜单栏"文件夹"按钮；本机 S3 服务上验证了 60 秒 TTL 到期后系统驱逐已下载文件。

注意：系统上报的已下载集合包含根容器与目录，驱逐候选必须只取文件，否则会把根容器驱逐掉导致整个 domain 不可用（已修复并有测试）。

调试入口：App 注册了 `ndisk://` URL scheme，`add` / `remove` / `signal`（唤醒工作集）供脚本与 `apple/Tools/ndisk-devctl` 使用。

**远端变更发现**：扩展存活期间每 60 秒刷新用户最近 15 分钟内看过的目录（"热目录"），有变更则 `signalEnumerator(.workingSet)`；元数据 upsert 只在条目实际变化时写变更日志，避免每次刷新都让系统重新枚举。已验证直接写入 S3 的对象在一个周期内自动出现在 Finder。

**冲突**：上传时 If-Match 失败（远端在编辑期间被改）→ 本地内容另存为 `name (conflicted copy YYYY-MM-DD HH-MM-SS).ext` 并上传，原条目回到远端版本。

**App 状态展示**：扩展每个轮询周期把缓存占用、待上传数、已下载数、最近错误写入 App Group 容器的 `status/<id>.json`，菜单栏 App 打开时读取。不走 XPC，避免沙盒 App 需要访问 CloudStorage 路径。

**配置模型**（2026-10-16 调整）：

- 全局设置 `settings.toml`（App Group 的 `Library/Application Support/ndisk/`）：默认缓存策略、缓存总上限（按挂载数分摊）、代理、上传 / 下载限速、并发、日志级别。
- 挂载配置只含后端、凭据引用、只读，以及对默认策略的可选覆盖。Bucket / 前缀 / Endpoint 创建后不可改（等同换存储），其余可编辑；App 保存后扩展在下个轮询周期检测到文件变化即重开会话，显示名变化通过同 id 再次 `add(domain)` 更新。
- 断开 / 连接：`NSFileProviderManager.disconnect/reconnect`，位置保留，不走网络；"退出"只退出菜单栏程序，挂载由系统继续提供。
- 服务商预设（Amazon S3、阿里云 OSS、腾讯云 COS、Cloudflare R2、MinIO、自定义）决定 Endpoint 格式；自定义 endpoint 先试虚拟主机式，连接失败自动退回路径式；目录标记与复制阈值不出现在界面。
- 凭据：Access Key（可带 Session Token，与 Secret 一起以 JSON 存钥匙串）、AssumeRole（SDK 自动刷新，可指定源密钥）、命名 Profile（沙盒内暂不可用）、默认凭据链（CLI）。

**待办**：

- 轮询只覆盖热目录；冷目录中的远端变更要等用户再次打开该目录；
- App 与扩展各自静态链接一份核心（Release 单二进制约 29 MB / 架构），后续改为共享动态框架以减半体积。

### 4.2 Linux

- `ndisk-fuse`：基于 `fuser`（MIT，关闭 libfuse 特性走纯 Rust 挂载，运行时只需 fusermount3）实现 FUSE3，inode 即 `item_id`，默认 `allow_other` 关闭。
- fuser 回调是同步的：`mount` 在 runtime 之外的线程运行，回调内用 `Handle::block_on` 调引擎。
- 写路径：以写模式 `open` 时取得脏文件句柄，`write` 直接 pwrite 脏文件；`release` 登记写回并后台上传；`fsync` 同步等待上传完成。上传进行中再次打开写入会等待该文件的写回锁。
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
| M1 | 核心引擎 + S3 后端 + CLI + Linux FUSE | Linux 下挂载 S3 兼容服务读写，进程被杀后重挂载上传续传。**已完成**（2026-10-16，见 `scripts/e2e`）|
| M2 | macOS File Provider 扩展 + 菜单栏 App | Finder 中可见角标、按需下载、TTL 驱逐、右键动作。**基本完成**（2026-10-16）：读写、角标、右键动作、TTL 驱逐、远端变更轮询在真机确认，冲突副本有单元测试，App 显示状态；剩余：共享动态框架减体积 |
| M3 | OpenDAL 后端接入 OSS / WebDAV / Dropbox | 同一套 CLI 挂载三种后端通过回归测试。**基本完成**（2026-10-19）：WebDAV 在 Docker（Apache mod_dav）中复跑全部 25 项 FUSE 检查通过，菜单栏 App 可添加 WebDAV / Dropbox 挂载；OSS、Dropbox 尚待真实账号验证；Dropbox 的 OAuth 授权流程（免手动生成 token）待做 |
| M4 | Windows Cloud Files API | 资源管理器中占位符、按需下载、角标 |

## 7. 待决事项

- 目录标记对象：默认写 `dir/` 零字节对象还是只依赖隐式前缀？倾向写标记以兼容空目录。
- 冲突副本命名与是否通知用户。
- 大 Bucket（百万级对象）的首次枚举体验：是否限制根目录直接挂载，推荐挂载前缀。
- Linux 角标插件的具体形态，视桌面环境需求再定。
