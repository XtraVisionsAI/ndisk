//! 目录树与路径映射。把扁平的对象键空间抹平为文件系统语义，并维护元数据缓存。

use std::sync::Arc;
use std::time::SystemTime;

use crate::backend::DynBackend;
use crate::cache::{ContentCache, MetaStore};
use crate::config::CacheConfig;
use crate::types::*;
use crate::{Error, Result};

pub struct Vfs {
    backend: DynBackend,
    meta: Arc<MetaStore>,
    content: Arc<ContentCache>,
    cfg: CacheConfig,
    /// 挂载前缀，如 `photos/`，空表示 bucket 根。
    prefix: String,
}

impl Vfs {
    pub fn new(
        backend: DynBackend,
        meta: Arc<MetaStore>,
        content: Arc<ContentCache>,
        cfg: CacheConfig,
        prefix: String,
    ) -> Self {
        let prefix = normalize_prefix(&prefix);
        Self {
            backend,
            meta,
            content,
            cfg,
            prefix,
        }
    }

    pub fn backend(&self) -> &DynBackend {
        &self.backend
    }
    pub fn meta(&self) -> &Arc<MetaStore> {
        &self.meta
    }
    pub fn content(&self) -> &Arc<ContentCache> {
        &self.content
    }

    /// 节点的远端键：目录以 `/` 结尾。
    pub fn remote_key(&self, parent_key: &str, name: &str, kind: Kind) -> String {
        let base = if parent_key.is_empty() {
            self.prefix.clone()
        } else {
            parent_key.to_string()
        };
        match kind {
            Kind::Dir => format!("{base}{name}/"),
            Kind::File => format!("{base}{name}"),
        }
    }

    fn dir_key(&self, dir: &Node) -> String {
        if dir.id == ItemId::ROOT {
            self.prefix.clone()
        } else {
            dir.remote_key.clone()
        }
    }

    pub fn get(&self, id: ItemId) -> Result<Node> {
        let mut n = self
            .meta
            .get(id)?
            .ok_or_else(|| Error::NotFound(format!("item {}", id.0)))?;
        self.fill_local_state(&mut n)?;
        Ok(n)
    }

    fn fill_local_state(&self, n: &mut Node) -> Result<()> {
        if n.kind == Kind::File && n.local != LocalState::Pinned {
            n.local = self.content.local_state(n.id, &n.cache_key(), n.size)?;
        }
        Ok(())
    }

    /// 列出目录，必要时从后端刷新（受 dir_ttl 控制）。
    pub async fn list(&self, dir: ItemId, force: bool) -> Result<Vec<Node>> {
        let d = self.get(dir)?;
        if d.kind != Kind::Dir {
            return Err(Error::NotADirectory(d.name));
        }
        if force || !self.dir_fresh(dir)? {
            self.refresh_dir(&d).await?;
        }
        let mut nodes = self.meta.children(dir)?;
        for n in &mut nodes {
            self.fill_local_state(n)?;
        }
        Ok(nodes)
    }

    /// 分页列目录。第一页（`after` 为 `None`）按需刷新远端，后续页只读本地元数据，
    /// 保证同一轮枚举看到的是同一份快照。返回下一页游标（本页最后一个名字），`None` 表示结束。
    pub async fn list_page(
        &self,
        dir: ItemId,
        force: bool,
        after: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<Node>, Option<String>)> {
        let d = self.get(dir)?;
        if d.kind != Kind::Dir {
            return Err(Error::NotADirectory(d.name));
        }
        if after.is_none() && (force || !self.dir_fresh(dir)?) {
            self.refresh_dir(&d).await?;
        }
        let limit = limit.max(1);
        let mut nodes = self.meta.children_page(dir, after, limit)?;
        for n in &mut nodes {
            self.fill_local_state(n)?;
        }
        let next = (nodes.len() == limit)
            .then(|| nodes.last().map(|n| n.name.clone()))
            .flatten();
        Ok((nodes, next))
    }

    fn dir_fresh(&self, dir: ItemId) -> Result<bool> {
        Ok(match self.meta.listed_at(dir)? {
            Some(t) => SystemTime::now()
                .duration_since(t)
                .map(|a| a < self.cfg.dir_ttl)
                .unwrap_or(false),
            None => false,
        })
    }

    async fn refresh_dir(&self, d: &Node) -> Result<()> {
        let key = self.dir_key(d);
        let mut seen = std::collections::HashSet::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self.backend.list(&key, cursor.as_deref()).await?;
            let batch: Vec<(String, Entry)> = page
                .entries
                .into_iter()
                .filter(|e| e.key != key) // 目录自身的标记对象
                .map(|e| (e.name().to_string(), e))
                .filter(|(name, _)| !name.is_empty())
                .collect();
            seen.extend(batch.iter().map(|(n, _)| n.clone()));
            self.meta.upsert_many(d.id, &batch)?;
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        // 远端消失且本地没有未上传修改的节点，移除。
        for child in self.meta.children(d.id)? {
            if !seen.contains(&child.name) && self.meta.upload_get(child.id)?.is_none() {
                self.content.evict(child.id).await?;
                self.meta.remove(child.id)?;
            }
        }
        self.meta.mark_listed(d.id)?;
        Ok(())
    }

    pub async fn lookup(&self, parent: ItemId, name: &str) -> Result<Node> {
        if let Some(mut n) = self.meta.lookup(parent, name)? {
            self.fill_local_state(&mut n)?;
            return Ok(n);
        }
        // 未缓存：先 stat 文件，再 stat 目录标记。
        let p = self.get(parent)?;
        let pkey = self.dir_key(&p);
        for kind in [Kind::File, Kind::Dir] {
            let key = self.remote_key(&pkey, name, kind);
            if let Some(e) = self.backend.stat(&key).await? {
                let id = self.meta.upsert(parent, name, &e, &e.key)?;
                return self.get(id);
            }
        }
        Err(Error::NotFound(format!("{name} in {}", p.name)))
    }

    pub async fn mkdir(&self, parent: ItemId, name: &str) -> Result<Node> {
        if self.meta.lookup(parent, name)?.is_some() {
            return Err(Error::AlreadyExists(name.into()));
        }
        let p = self.get(parent)?;
        let key = self.remote_key(&self.dir_key(&p), name, Kind::Dir);
        let e = self.backend.mkdir(&key).await?;
        let id = self.meta.upsert(parent, name, &e, &key)?;
        self.get(id)
    }

    /// 创建空文件节点（内容由写回层负责）。
    pub async fn create_file(&self, parent: ItemId, name: &str) -> Result<Node> {
        if self.meta.lookup(parent, name)?.is_some() {
            return Err(Error::AlreadyExists(name.into()));
        }
        let p = self.get(parent)?;
        let key = self.remote_key(&self.dir_key(&p), name, Kind::File);
        let e = Entry {
            key: key.clone(),
            kind: Kind::File,
            size: 0,
            mtime: Some(SystemTime::now()),
            etag: None,
            version: None,
        };
        let id = self.meta.upsert(parent, name, &e, &key)?;
        self.get(id)
    }

    pub async fn delete(&self, id: ItemId) -> Result<()> {
        let n = self.get(id)?;
        if n.kind == Kind::Dir && !self.list(id, true).await?.is_empty() {
            return Err(Error::DirectoryNotEmpty(n.name));
        }
        if let Some(rec) = self.meta.upload_get(id)? {
            if let Some(up) = &rec.upload_id {
                let _ = self.backend.multipart_abort(&rec.remote_key, up).await;
            }
            self.meta.upload_remove(id)?;
        }
        if n.etag.is_some() || n.kind == Kind::Dir {
            self.backend.delete(&n.remote_key).await?;
        }
        self.content.evict(id).await?;
        self.content.remove_dirty(id).await?;
        self.meta.remove(id)?;
        Ok(())
    }

    /// 重命名/移动，ItemId 保持不变。文件交给后端 rename（原生 MOVE 或 copy+delete），
    /// 目录逐个递归：OpenDAL 等后端的 rename 只接受文件。
    pub async fn rename(&self, id: ItemId, new_parent: ItemId, new_name: &str) -> Result<Node> {
        let n = self.get(id)?;
        if let Some(existing) = self.meta.lookup(new_parent, new_name)? {
            if existing.id != id {
                return Err(Error::AlreadyExists(new_name.into()));
            }
        }
        let np = self.get(new_parent)?;
        let new_key = self.remote_key(&self.dir_key(&np), new_name, n.kind);
        if new_key == n.remote_key {
            return self.get(id);
        }
        match n.kind {
            Kind::File => {
                let moved = if n.etag.is_some() {
                    Some(self.backend.rename(&n.remote_key, &new_key).await?)
                } else {
                    None
                };
                self.meta.rename(id, new_parent, new_name, &new_key)?;
                self.follow_upload(id, &new_key, moved.as_ref().and_then(|e| e.etag.as_deref()))
                    .await?;
            }
            Kind::Dir => {
                let children = self.list(id, true).await?;
                self.backend.mkdir(&new_key).await?;
                self.meta.rename(id, new_parent, new_name, &new_key)?;
                for c in children {
                    Box::pin(self.rename(c.id, id, &c.name)).await?;
                }
                let _ = self.backend.delete(&n.remote_key).await;
            }
        }
        self.get(id)
    }

    /// 待上传记录跟随节点改名。已开始的分片上传绑定在旧键上，先中止。
    async fn follow_upload(&self, id: ItemId, key: &str, base_etag: Option<&str>) -> Result<()> {
        let Some(rec) = self.meta.upload_get(id)? else {
            return Ok(());
        };
        if let Some(up) = &rec.upload_id {
            let _ = self.backend.multipart_abort(&rec.remote_key, up).await;
        }
        self.meta.rekey_upload(id, key, base_etag)
    }

    pub fn changes_since(&self, anchor: ChangeAnchor) -> Result<(Vec<Change>, ChangeAnchor)> {
        self.meta.changes_since(anchor)
    }
}

fn normalize_prefix(p: &str) -> String {
    let p = p.trim_matches('/');
    if p.is_empty() {
        String::new()
    } else {
        format!("{p}/")
    }
}
