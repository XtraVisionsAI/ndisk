//! 面向 Swift（以及后续 Kotlin / C#）的稳定 API 层，基于 UniFFI 过程宏导出。
//!
//! 与 File Provider 回调的对应关系见 `docs/DESIGN.md` 4.1。约定：
//! - ItemId 以 `u64` 传递，`1` 为根目录；Swift 侧负责与 `NSFileProviderItemIdentifier` 互转；
//! - 涉及网络或磁盘的操作都是 `async`，在 [`Session`] 自有的 tokio runtime 上执行，
//!   Swift 侧直接 `await`，不会阻塞 File Provider 的回调线程；
//! - 读元数据（`item`、`changes_since`）只查本地 SQLite，同步返回。

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use ndisk_core::config::{BackendConfig, MountConfig, S3Auth, S3Config};
use ndisk_core::{CachePolicy, Change, ChangeAnchor, Engine, ItemId, Kind, LocalState, Node};

uniffi::setup_scaffolding!();

// ---------------------------------------------------------------- errors

/// 映射到 Swift 后由扩展转换为 `NSFileProviderError` / `CocoaError`。
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum NdiskError {
    #[error("not found: {message}")]
    NotFound { message: String },
    #[error("already exists: {message}")]
    AlreadyExists { message: String },
    #[error("not a directory: {message}")]
    NotADirectory { message: String },
    #[error("is a directory: {message}")]
    IsADirectory { message: String },
    #[error("directory not empty: {message}")]
    DirectoryNotEmpty { message: String },
    #[error("permission denied: {message}")]
    PermissionDenied { message: String },
    #[error("read-only mount")]
    ReadOnly,
    /// 远端已被修改（ETag 不匹配）。
    #[error("conflict: {message}")]
    Conflict { message: String },
    /// 离线且内容未缓存，或网络不可达。
    #[error("offline: {message}")]
    Offline { message: String },
    #[error("unsupported: {message}")]
    Unsupported { message: String },
    #[error("config: {message}")]
    Config { message: String },
    #[error("{message}")]
    Other { message: String },
}

impl From<ndisk_core::Error> for NdiskError {
    fn from(e: ndisk_core::Error) -> Self {
        use ndisk_core::Error as E;
        let message = e.to_string();
        match e {
            E::NotFound(_) => Self::NotFound { message },
            E::AlreadyExists(_) => Self::AlreadyExists { message },
            E::NotADirectory(_) => Self::NotADirectory { message },
            E::IsADirectory(_) => Self::IsADirectory { message },
            E::DirectoryNotEmpty(_) => Self::DirectoryNotEmpty { message },
            E::PermissionDenied(_) => Self::PermissionDenied { message },
            E::ReadOnly => Self::ReadOnly,
            E::Conflict { .. } => Self::Conflict { message },
            E::Offline(_) => Self::Offline { message },
            E::Unsupported(_) => Self::Unsupported { message },
            E::Config(_) => Self::Config { message },
            E::Backend(_) | E::Io(_) | E::Db(_) => Self::Other { message },
        }
    }
}

impl From<tokio::task::JoinError> for NdiskError {
    fn from(e: tokio::task::JoinError) -> Self {
        Self::Other {
            message: format!("task failed: {e}"),
        }
    }
}

type Result<T> = std::result::Result<T, NdiskError>;

// ---------------------------------------------------------------- records

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ItemKind {
    File,
    Directory,
}

/// 本地状态。macOS 上"是否已下载"由系统掌握，这里主要用于上传状态与 CLI / 托盘展示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ItemState {
    Cloud,
    Partial,
    Cached,
    Dirty,
    Uploading,
    Pinned,
}

/// 缓存策略。`Transient` 对应核心的 `CachePolicy::None`（避免与 Swift `Optional.none` 混淆）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CachePolicyFfi {
    Ttl { seconds: u64 },
    Always,
    Transient,
}

impl From<CachePolicy> for CachePolicyFfi {
    fn from(p: CachePolicy) -> Self {
        match p {
            CachePolicy::Ttl(d) => Self::Ttl {
                seconds: d.as_secs(),
            },
            CachePolicy::Always => Self::Always,
            CachePolicy::None => Self::Transient,
        }
    }
}

impl From<CachePolicyFfi> for CachePolicy {
    fn from(p: CachePolicyFfi) -> Self {
        match p {
            CachePolicyFfi::Ttl { seconds } => CachePolicy::Ttl(Duration::from_secs(seconds)),
            CachePolicyFfi::Always => CachePolicy::Always,
            CachePolicyFfi::Transient => CachePolicy::None,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ItemInfo {
    pub id: u64,
    pub parent: u64,
    pub name: String,
    pub kind: ItemKind,
    pub size: u64,
    pub mtime: Option<SystemTime>,
    /// 内容版本（etag 或其回退值），用作 `NSFileProviderItemVersion.contentVersion`。
    pub content_version: String,
    /// 元数据版本，用作 `NSFileProviderItemVersion.metadataVersion`。
    pub metadata_version: String,
    pub state: ItemState,
    /// 生效的缓存策略（已解析继承），决定 `contentPolicy` 与角标。
    pub effective_policy: CachePolicyFfi,
    /// 是否有本地修改尚未上传，对应 `isUploaded == false`。
    pub pending_upload: bool,
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum ItemChange {
    Updated { item: ItemInfo },
    Deleted { id: u64 },
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ChangeBatch {
    pub changes: Vec<ItemChange>,
    /// 下一次调用 `changes_since` 时传入的锚点。
    pub anchor: u64,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct MaintainResult {
    pub ttl_evicted: u64,
    pub capacity_evicted: u64,
    pub bytes_freed: u64,
    pub prefetched: u64,
    pub cached_bytes: u64,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SessionStatus {
    pub mount_id: String,
    pub display_name: String,
    pub backend: String,
    pub cached_bytes: u64,
    pub pending_uploads: u64,
    pub materialized_items: u64,
}

/// 凭据方式（界面选项）。密钥本体存钥匙串，这里只有非敏感部分。
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum S3AuthSpec {
    /// aws-config 默认凭据链（环境变量、~/.aws、实例角色）。沙盒内基本不可用，供 CLI。
    Default,
    /// 静态 Access Key；Secret（可带 session token）存钥匙串。
    Static { access_key_id: String },
    /// 以静态密钥或默认链为源，AssumeRole 获取自动刷新的临时凭据。
    AssumeRole {
        role_arn: String,
        external_id: Option<String>,
        source_access_key_id: Option<String>,
    },
    /// 命名 profile（需要能读到 ~/.aws）。
    Profile { name: String },
}

/// 菜单栏 App 添加 / 编辑挂载时的 S3 参数。网络与缓存参数属于全局设置，不在这里。
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct S3MountSpec {
    pub id: String,
    pub display_name: String,
    pub bucket: String,
    pub prefix: String,
    pub region: Option<String>,
    pub endpoint: Option<String>,
    pub force_path_style: bool,
    pub auth: S3AuthSpec,
    /// 覆盖全局默认缓存策略；`None` 继承。
    pub policy_override: Option<CachePolicyFfi>,
    pub read_only: bool,
}

/// 全局设置的界面视图。
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct SettingsSpec {
    pub default_policy: CachePolicyFfi,
    pub max_cache_bytes: u64,
    /// `""` 直连，`"env"` 系统 / 环境变量代理，或 `http://[user:pass@]host:port`。
    pub proxy: String,
    pub upload_limit: u64,
    pub download_limit: u64,
    pub log_filter: String,
}

fn secret_ref(id: &str) -> String {
    format!("keychain:{id}")
}

/// 生成挂载配置 TOML。
#[uniffi::export]
pub fn render_s3_config(spec: S3MountSpec) -> Result<String> {
    let auth = match &spec.auth {
        S3AuthSpec::Default => S3Auth::Default,
        S3AuthSpec::Static { access_key_id } => S3Auth::Static {
            access_key_id: access_key_id.clone(),
            secret_ref: secret_ref(&spec.id),
        },
        S3AuthSpec::AssumeRole {
            role_arn,
            external_id,
            source_access_key_id,
        } => S3Auth::AssumeRole {
            role_arn: role_arn.clone(),
            session_name: Some("ndisk".into()),
            external_id: external_id.clone().filter(|s| !s.is_empty()),
            access_key_id: source_access_key_id.clone().filter(|s| !s.is_empty()),
            secret_ref: source_access_key_id
                .as_ref()
                .filter(|s| !s.is_empty())
                .map(|_| secret_ref(&spec.id)),
        },
        S3AuthSpec::Profile { name } => S3Auth::Profile { name: name.clone() },
    };
    let cfg = MountConfig {
        id: spec.id,
        display_name: spec.display_name,
        backend: BackendConfig::S3(S3Config {
            bucket: spec.bucket,
            prefix: spec.prefix,
            region: spec.region.filter(|s| !s.is_empty()),
            endpoint: spec.endpoint.filter(|s| !s.is_empty()),
            force_path_style: spec.force_path_style,
            auth,
            dir_markers: true,
            copy_threshold: None,
        }),
        read_only: spec.read_only,
        overrides: ndisk_core::config::MountOverrides {
            policy: spec.policy_override.map(Into::into),
        },
        cache: None,
        network: None,
    };
    cfg.validate()?;
    Ok(cfg.to_toml()?)
}

/// 解析挂载配置 TOML 为界面视图（编辑时回填）。非 S3 后端返回 Unsupported。
#[uniffi::export]
pub fn parse_s3_config(config_toml: String) -> Result<S3MountSpec> {
    let cfg = MountConfig::from_toml(&config_toml)?;
    let BackendConfig::S3(s3) = &cfg.backend else {
        return Err(NdiskError::Unsupported {
            message: "not an S3 mount".into(),
        });
    };
    let auth = match &s3.auth {
        S3Auth::Default | S3Auth::WebIdentity { .. } => S3AuthSpec::Default,
        S3Auth::Static { access_key_id, .. } => S3AuthSpec::Static {
            access_key_id: access_key_id.clone(),
        },
        S3Auth::AssumeRole {
            role_arn,
            external_id,
            access_key_id,
            ..
        } => S3AuthSpec::AssumeRole {
            role_arn: role_arn.clone(),
            external_id: external_id.clone(),
            source_access_key_id: access_key_id.clone(),
        },
        S3Auth::Profile { name } | S3Auth::Sso { profile: name } => {
            S3AuthSpec::Profile { name: name.clone() }
        }
    };
    Ok(S3MountSpec {
        id: cfg.id.clone(),
        display_name: cfg.display_name.clone(),
        bucket: s3.bucket.clone(),
        prefix: s3.prefix.clone(),
        region: s3.region.clone(),
        endpoint: s3.endpoint.clone(),
        force_path_style: s3.force_path_style,
        auth,
        policy_override: cfg.overrides.policy.map(Into::into),
        read_only: cfg.read_only,
    })
}

#[uniffi::export]
pub fn default_settings() -> SettingsSpec {
    settings_to_spec(&ndisk_core::config::GlobalSettings::default())
}

#[uniffi::export]
pub fn render_settings(spec: SettingsSpec) -> Result<String> {
    let mut g = ndisk_core::config::GlobalSettings::default();
    g.cache.policy = spec.default_policy.into();
    g.cache.max_bytes = spec.max_cache_bytes;
    g.network.proxy = Some(spec.proxy.trim().to_string()).filter(|s| !s.is_empty());
    g.network.upload_limit = spec.upload_limit;
    g.network.download_limit = spec.download_limit;
    if !spec.log_filter.trim().is_empty() {
        g.log_filter = spec.log_filter.trim().to_string();
    }
    Ok(g.to_toml()?)
}

#[uniffi::export]
pub fn parse_settings(settings_toml: String) -> Result<SettingsSpec> {
    Ok(settings_to_spec(
        &ndisk_core::config::GlobalSettings::from_toml(&settings_toml)?,
    ))
}

fn settings_to_spec(g: &ndisk_core::config::GlobalSettings) -> SettingsSpec {
    SettingsSpec {
        default_policy: g.cache.policy.into(),
        max_cache_bytes: g.cache.max_bytes,
        proxy: g.network.proxy.clone().unwrap_or_default(),
        upload_limit: g.network.upload_limit,
        download_limit: g.network.download_limit,
        log_filter: g.log_filter.clone(),
    }
}

/// 初始化日志（进程内只生效一次）。`filter` 形如 `info,ndisk_core=debug`。
/// Apple 平台输出到统一日志（subsystem `ai.xtravisions.ndisk`，category `core`），
/// 可用 `log show --predicate 'subsystem == "ai.xtravisions.ndisk"'` 查看；其他平台输出到 stderr。
#[uniffi::export]
pub fn init_logging(filter: String) {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let env = tracing_subscriber::EnvFilter::new(filter);
        #[cfg(target_vendor = "apple")]
        {
            let _ = tracing_subscriber::registry()
                .with(env)
                .with(tracing_oslog::OsLogger::new("ai.xtravisions.ndisk", "core"))
                .try_init();
        }
        #[cfg(not(target_vendor = "apple"))]
        {
            let _ = tracing_subscriber::registry()
                .with(env)
                .with(tracing_subscriber::fmt::layer().with_ansi(false))
                .try_init();
        }
    });
}

#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 打开一个挂载会话。`settings_toml` 为全局设置（可空），`mount_count` 用于分摊全局缓存容量；
/// `data_dir` 放元数据库与未上传文件（持久），`cache_dir` 放块缓存（可清理）。
/// `secret` 为钥匙串中的密钥：纯 Secret Access Key，或含 session_token 的 JSON。
#[uniffi::export]
pub async fn open_session(
    config_toml: String,
    settings_toml: Option<String>,
    mount_count: u32,
    data_dir: String,
    cache_dir: String,
    secret: Option<String>,
) -> Result<Arc<Session>> {
    let global = match settings_toml
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(t) => ndisk_core::config::GlobalSettings::from_toml(t)?,
        None => ndisk_core::config::GlobalSettings::default(),
    };
    let cfg = MountConfig::from_toml(&config_toml)?.apply(&global, mount_count);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("ndisk-rt")
        .enable_all()
        .build()
        .map_err(|e| NdiskError::Other {
            message: e.to_string(),
        })?;
    let data_dir = PathBuf::from(data_dir);
    let cache_dir = PathBuf::from(cache_dir);
    let engine = rt
        .spawn(async move {
            let backend: Arc<dyn ndisk_core::Backend> = match &cfg.backend {
                BackendConfig::S3(s3) => Arc::new(
                    ndisk_backend_s3::S3Backend::connect(
                        s3,
                        secret,
                        cfg.network().proxy.as_deref(),
                    )
                    .await?,
                ),
                BackendConfig::Opendal {
                    scheme, options, ..
                } => Arc::new(
                    ndisk_backend_opendal::OpendalBackend::connect(
                        scheme,
                        options,
                        secret,
                        cfg.network().proxy.as_deref(),
                    )
                    .await?,
                ),
            };
            let engine = Arc::new(Engine::open(cfg, backend, data_dir, cache_dir)?);
            engine.resume_uploads().await?;
            Ok::<_, ndisk_core::Error>(engine)
        })
        .await??;
    Ok(Arc::new(Session {
        engine,
        rt: Mutex::new(Some(rt)),
    }))
}

// ---------------------------------------------------------------- session

/// 一个挂载点一个实例；File Provider 扩展与菜单栏 App 各自持有。
#[derive(uniffi::Object)]
pub struct Session {
    engine: Arc<Engine>,
    rt: Mutex<Option<tokio::runtime::Runtime>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Swift 可能在任意线程释放对象：不能在异步上下文里同步 drop runtime。
        if let Some(rt) = self.rt.lock().unwrap().take() {
            rt.shutdown_background();
        }
    }
}

impl Session {
    async fn run<F, T>(&self, f: F) -> Result<T>
    where
        F: std::future::Future<Output = ndisk_core::Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let handle = {
            let rt = self.rt.lock().unwrap();
            rt.as_ref()
                .ok_or_else(|| NdiskError::Other {
                    message: "session closed".into(),
                })?
                .handle()
                .clone()
        };
        Ok(handle.spawn(f).await??)
    }

    fn info(&self, n: Node) -> ndisk_core::Result<ItemInfo> {
        let pending = self.engine.writeback.is_pending(n.id)?;
        let policy = self.engine.effective_policy(n.id)?;
        Ok(to_info(n, pending, policy))
    }
}

#[uniffi::export]
impl Session {
    pub fn status(&self) -> Result<SessionStatus> {
        let m = self.engine.vfs.meta();
        Ok(SessionStatus {
            mount_id: self.engine.cfg.id.clone(),
            display_name: self.engine.cfg.display_name.clone(),
            backend: self.engine.vfs.backend().name().to_string(),
            cached_bytes: m.total_cached_bytes()?,
            pending_uploads: m.uploads_pending()?.len() as u64,
            materialized_items: m.materialized_items()?.len() as u64,
        })
    }

    // ---- 元数据（同步，只查本地）----

    /// `item(for:)`。
    pub fn item(&self, id: u64) -> Result<ItemInfo> {
        let n = self.engine.stat(ItemId(id))?;
        Ok(self.info(n)?)
    }

    /// 当前变更锚点，`enumerator.currentSyncAnchor`。
    pub fn current_anchor(&self) -> Result<u64> {
        Ok(self.engine.vfs.meta().latest_anchor()?.0)
    }

    /// `enumerateChanges(from:)`。
    pub fn changes_since(&self, anchor: u64) -> Result<ChangeBatch> {
        let (changes, next) = self.engine.vfs.changes_since(ChangeAnchor(anchor))?;
        let mut out = Vec::with_capacity(changes.len());
        for c in changes {
            match c {
                Change::Upsert(n) => {
                    let n = self.engine.stat(n.id)?;
                    out.push(ItemChange::Updated {
                        item: self.info(n)?,
                    });
                }
                Change::Delete(id) => out.push(ItemChange::Deleted { id: id.0 }),
            }
        }
        Ok(ChangeBatch {
            changes: out,
            anchor: next.0,
        })
    }

    // ---- 枚举与查找（可能访问网络）----

    /// `enumerateItems`。`force` 时忽略目录 TTL 强制刷新。
    pub async fn list(&self, dir: u64, force: bool) -> Result<Vec<ItemInfo>> {
        let e = self.engine.clone();
        let nodes = self
            .run(async move { e.vfs.list(ItemId(dir), force).await })
            .await?;
        nodes
            .into_iter()
            .map(|n| self.info(n).map_err(Into::into))
            .collect()
    }

    pub async fn lookup(&self, parent: u64, name: String) -> Result<ItemInfo> {
        let e = self.engine.clone();
        let n = self
            .run(async move { e.vfs.lookup(ItemId(parent), &name).await })
            .await?;
        Ok(self.info(n)?)
    }

    // ---- 内容 ----

    /// `fetchContents(for:)`：导出完整内容到 `dest_path`，返回导出时的条目。
    pub async fn fetch_contents(&self, id: u64, dest_path: String) -> Result<ItemInfo> {
        let e = self.engine.clone();
        let n = self
            .run(async move {
                e.export_to(ItemId(id), std::path::Path::new(&dest_path), false)
                    .await
            })
            .await?;
        Ok(self.info(n)?)
    }

    /// 导出用的临时目录（与缓存同卷）。
    pub fn staging_dir(&self) -> String {
        self.engine
            .vfs
            .content()
            .staging_dir()
            .to_string_lossy()
            .into_owned()
    }

    // ---- 修改 ----

    /// `createItem`：`contents_path` 为空表示目录或空文件。
    pub async fn create_item(
        &self,
        parent: u64,
        name: String,
        kind: ItemKind,
        contents_path: Option<String>,
    ) -> Result<ItemInfo> {
        let e = self.engine.clone();
        let n = self
            .run(async move {
                e.ensure_writable()?;
                match kind {
                    ItemKind::Directory => e.vfs.mkdir(ItemId(parent), &name).await,
                    ItemKind::File => {
                        let node = e.vfs.create_file(ItemId(parent), &name).await?;
                        let src = match &contents_path {
                            Some(p) => PathBuf::from(p),
                            None => {
                                let p = e
                                    .vfs
                                    .content()
                                    .staging_dir()
                                    .join(format!("empty-{}", node.id.0));
                                tokio::fs::create_dir_all(p.parent().unwrap()).await?;
                                tokio::fs::write(&p, b"").await?;
                                p
                            }
                        };
                        e.import_and_upload(node.id, &src).await
                    }
                }
            })
            .await?;
        Ok(self.info(n)?)
    }

    /// `modifyItem` 的内容变更。
    pub async fn modify_contents(&self, id: u64, contents_path: String) -> Result<ItemInfo> {
        let e = self.engine.clone();
        let n = self
            .run(async move {
                e.import_and_upload(ItemId(id), std::path::Path::new(&contents_path))
                    .await
            })
            .await?;
        Ok(self.info(n)?)
    }

    /// `modifyItem` 的重命名 / 移动。
    pub async fn rename(&self, id: u64, new_parent: u64, new_name: String) -> Result<ItemInfo> {
        let e = self.engine.clone();
        let n = self
            .run(async move {
                e.ensure_writable()?;
                e.vfs
                    .rename(ItemId(id), ItemId(new_parent), &new_name)
                    .await
            })
            .await?;
        Ok(self.info(n)?)
    }

    /// `deleteItem`。目录需为空（File Provider 会先递归删除子项）。
    pub async fn delete(&self, id: u64) -> Result<()> {
        let e = self.engine.clone();
        self.run(async move {
            e.ensure_writable()?;
            e.vfs.delete(ItemId(id)).await
        })
        .await
    }

    // ---- 缓存控制（右键动作）----

    /// 固定为始终缓存。只记录策略：File Provider 据 `contentPolicy` 由系统下载并保留。
    pub fn pin(&self, id: u64) -> Result<()> {
        Ok(self
            .engine
            .set_policy_only(ItemId(id), Some(CachePolicy::Always))?)
    }

    pub fn unpin(&self, id: u64) -> Result<()> {
        Ok(self.engine.unpin(ItemId(id))?)
    }

    /// 设置缓存策略；`None` 表示继承。不预取（同 [`Session::pin`]）。
    pub fn set_policy(&self, id: u64, policy: Option<CachePolicyFfi>) -> Result<()> {
        Ok(self
            .engine
            .set_policy_only(ItemId(id), policy.map(Into::into))?)
    }

    /// 清理引擎侧的块缓存（系统持有的副本由 Swift 调 `evictItem` 处理）。
    pub async fn evict_local(&self, id: u64) -> Result<()> {
        let e = self.engine.clone();
        self.run(async move { e.evict(ItemId(id)).await }).await
    }

    /// 引擎侧维护：块缓存的 TTL / 容量驱逐。`prefetch_pinned` 为真时还会把固定内容预取进块缓存
    /// （File Provider 场景应传 false，内容由系统持有）。
    pub async fn maintain(&self, prefetch_pinned: bool) -> Result<MaintainResult> {
        let e = self.engine.clone();
        let r = self
            .run(async move {
                if prefetch_pinned {
                    e.maintain().await
                } else {
                    e.maintain_blocks().await
                }
            })
            .await?;
        Ok(MaintainResult {
            ttl_evicted: r.ttl_evicted,
            capacity_evicted: r.capacity_evicted,
            bytes_freed: r.bytes_freed,
            prefetched: r.prefetched,
            cached_bytes: r.cached_bytes,
        })
    }

    /// 刷新最近 `within_seconds` 内被用户看过的目录，发现远端变更。返回新变更条数；
    /// 大于 0 时 Swift 侧应 `signalEnumerator(.workingSet)`。
    pub async fn refresh_hot_dirs(&self, within_seconds: u64) -> Result<u64> {
        let e = self.engine.clone();
        self.run(async move {
            e.refresh_hot_dirs(Duration::from_secs(within_seconds))
                .await
        })
        .await
    }

    // ---- 系统持有副本（File Provider 已下载集合）----

    /// `materializedItemsDidChange` 后由 Swift 传入系统当前的完整已下载集合。
    pub fn sync_materialized(&self, ids: Vec<u64>) -> Result<()> {
        let ids: Vec<ItemId> = ids.into_iter().map(ItemId).collect();
        Ok(self.engine.vfs.meta().sync_materialized(&ids)?)
    }

    /// TTL 已到期、应调用 `NSFileProviderManager.evictItem` 的条目。
    pub fn eviction_candidates(&self) -> Result<Vec<u64>> {
        Ok(self
            .engine
            .eviction_candidates(SystemTime::now())?
            .into_iter()
            .map(|i| i.0)
            .collect())
    }

    /// 系统驱逐成功后调用。
    pub fn mark_evicted(&self, id: u64) -> Result<()> {
        Ok(self.engine.vfs.meta().mark_evicted(ItemId(id))?)
    }

    /// 关闭会话：停止后台任务。之后的调用返回错误。
    pub fn close(&self) {
        if let Some(rt) = self.rt.lock().unwrap().take() {
            rt.shutdown_background();
        }
    }
}

fn to_info(n: Node, pending_upload: bool, policy: CachePolicy) -> ItemInfo {
    let content_version = n.cache_key();
    let mtime_secs = n
        .mtime
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    ItemInfo {
        id: n.id.0,
        parent: if n.id == ItemId::ROOT {
            ItemId::ROOT.0
        } else {
            n.parent.0
        },
        metadata_version: format!("{}|{}|{}|{}", n.parent.0, n.name, mtime_secs, n.size),
        name: n.name,
        kind: match n.kind {
            Kind::File => ItemKind::File,
            Kind::Dir => ItemKind::Directory,
        },
        size: n.size,
        mtime: n.mtime,
        content_version,
        state: match n.local {
            LocalState::Cloud => ItemState::Cloud,
            LocalState::Partial => ItemState::Partial,
            LocalState::Cached => ItemState::Cached,
            LocalState::Dirty => ItemState::Dirty,
            LocalState::Uploading => ItemState::Uploading,
            LocalState::Pinned => ItemState::Pinned,
        },
        effective_policy: policy.into(),
        pending_upload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> S3MountSpec {
        S3MountSpec {
            id: "m1".into(),
            display_name: "My Bucket".into(),
            bucket: "b".into(),
            prefix: "p/".into(),
            region: Some("us-east-1".into()),
            endpoint: Some("http://127.0.0.1:7070".into()),
            force_path_style: true,
            auth: S3AuthSpec::Static {
                access_key_id: "AK".into(),
            },
            policy_override: Some(CachePolicyFfi::Ttl { seconds: 3600 }),
            read_only: false,
        }
    }

    #[test]
    fn render_parse_roundtrip() {
        let toml = render_s3_config(spec()).unwrap();
        assert!(toml.contains("secret_ref = \"keychain:m1\""));
        assert!(
            !toml.contains("[cache]"),
            "network/cache belong to global settings"
        );
        assert_eq!(parse_s3_config(toml).unwrap(), spec());
        let mut a = spec();
        a.auth = S3AuthSpec::AssumeRole {
            role_arn: "arn:aws:iam::1:role/r".into(),
            external_id: None,
            source_access_key_id: Some("AK".into()),
        };
        a.policy_override = None;
        assert_eq!(
            parse_s3_config(render_s3_config(a.clone()).unwrap()).unwrap(),
            a
        );
    }

    #[test]
    fn settings_roundtrip() {
        let mut s = default_settings();
        s.proxy = "http://127.0.0.1:3128".into();
        s.max_cache_bytes = 5 << 30;
        assert_eq!(
            parse_settings(render_settings(s.clone()).unwrap()).unwrap(),
            s
        );
    }
}
