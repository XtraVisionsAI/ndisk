//! 面向 Swift / Kotlin / C# 的稳定 API 层。
//!
//! M2 阶段用 UniFFI 导出；当前先定义好与 File Provider 回调一一对应的接口形状，
//! 便于 Swift 侧并行开发。对应关系见 `docs/DESIGN.md` 4.1。

use std::sync::Arc;

use ndisk_core::{ChangeAnchor, Engine, ItemId, Result};

/// File Provider 扩展与菜单栏 App 共享的会话对象。一个挂载点一个实例。
pub struct Session {
    engine: Arc<Engine>,
    rt: tokio::runtime::Runtime,
}

impl Session {
    pub fn open(config_toml: &str, cache_root: &str, secret: Option<String>) -> Result<Self> {
        let cfg = ndisk_core::config::MountConfig::from_toml(config_toml)?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let engine = rt.block_on(async {
            let backend: Arc<dyn ndisk_core::Backend> = match &cfg.backend {
                ndisk_core::config::BackendConfig::S3(s3) => Arc::new(
                    ndisk_backend_s3::S3Backend::connect(s3, secret, cfg.network.proxy.as_deref())
                        .await?,
                ),
                _ => return Err(ndisk_core::Error::Unsupported("backend not implemented")),
            };
            Engine::open(cfg.clone(), backend, cache_root.into())
        })?;
        Ok(Self {
            engine: Arc::new(engine),
            rt,
        })
    }

    // enumerator(for:) / enumerateItems
    pub fn list(&self, dir: u64, force: bool) -> Result<String> {
        let nodes = self.rt.block_on(self.engine.vfs.list(ItemId(dir), force))?;
        Ok(serde_json::to_string(&nodes).unwrap_or_default())
    }

    // item(for:)
    pub fn item(&self, id: u64) -> Result<String> {
        Ok(serde_json::to_string(&self.engine.vfs.get(ItemId(id))?).unwrap_or_default())
    }

    // enumerateChanges(from:)
    pub fn changes_since(&self, anchor: u64) -> Result<(String, u64)> {
        let (changes, next) = self.engine.vfs.changes_since(ChangeAnchor(anchor))?;
        Ok((serde_json::to_string(&changes).unwrap_or_default(), next.0))
    }

    // fetchContents(for:) → 返回已完整落盘后可供复制的本地块目录；M2 阶段改为直接给出文件路径。
    pub fn download(&self, id: u64) -> Result<()> {
        self.rt.block_on(self.engine.download(ItemId(id)))
    }

    // modifyItem(contents) → 写入脏文件后调用
    pub fn open_for_write(&self, id: u64) -> Result<String> {
        Ok(self
            .rt
            .block_on(self.engine.writeback.open_for_write(ItemId(id)))?
            .to_string_lossy()
            .into_owned())
    }
    pub fn close_written(&self, id: u64) -> Result<()> {
        self.rt.block_on(self.engine.writeback.enqueue(ItemId(id)))
    }
    pub fn flush(&self, id: u64) -> Result<()> {
        self.rt
            .block_on(self.engine.writeback.flush(ItemId(id)))
            .map(|_| ())
    }

    // createItem / modifyItem(rename) / deleteItem
    pub fn mkdir(&self, parent: u64, name: &str) -> Result<u64> {
        Ok(self
            .rt
            .block_on(self.engine.vfs.mkdir(ItemId(parent), name))?
            .id
            .0)
    }
    pub fn create_file(&self, parent: u64, name: &str) -> Result<u64> {
        Ok(self
            .rt
            .block_on(self.engine.vfs.create_file(ItemId(parent), name))?
            .id
            .0)
    }
    pub fn rename(&self, id: u64, new_parent: u64, new_name: &str) -> Result<()> {
        self.rt
            .block_on(
                self.engine
                    .vfs
                    .rename(ItemId(id), ItemId(new_parent), new_name),
            )
            .map(|_| ())
    }
    pub fn delete(&self, id: u64) -> Result<()> {
        self.rt.block_on(self.engine.vfs.delete(ItemId(id)))
    }

    // 右键动作
    pub fn pin(&self, id: u64) -> Result<()> {
        self.rt.block_on(self.engine.pin(ItemId(id)))
    }
    pub fn unpin(&self, id: u64) -> Result<()> {
        self.engine.unpin(ItemId(id))
    }
    pub fn evict(&self, id: u64) -> Result<()> {
        self.rt.block_on(self.engine.evict(ItemId(id)))
    }

    /// 设置缓存策略；`policy_json` 为 `CachePolicy` 的 JSON，空串表示继承。
    pub fn set_policy(&self, id: u64, policy_json: &str) -> Result<()> {
        let policy = if policy_json.trim().is_empty() {
            None
        } else {
            Some(
                serde_json::from_str(policy_json)
                    .map_err(|e| ndisk_core::Error::Config(e.to_string()))?,
            )
        };
        self.rt.block_on(self.engine.set_policy(ItemId(id), policy))
    }

    /// 执行一次维护，返回 `MaintainReport` 的 JSON。
    pub fn maintain(&self) -> Result<String> {
        let r = self.rt.block_on(self.engine.maintain())?;
        Ok(serde_json::to_string(&r).unwrap_or_default())
    }
}
