//! 元数据存储。一个挂载点一个 SQLite 文件，所有需要跨进程重启保留的状态都在这里：
//! 节点树、目录列举时间、变更日志、固定集合、上传进度。

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

use crate::types::*;
use crate::Result;

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS nodes (
    id          INTEGER PRIMARY KEY,
    parent_id   INTEGER NOT NULL,
    name        TEXT    NOT NULL,
    kind        INTEGER NOT NULL,            -- 0 file, 1 dir
    size        INTEGER NOT NULL DEFAULT 0,
    mtime       INTEGER,                     -- unix seconds
    etag        TEXT,
    remote_key  TEXT    NOT NULL,
    listed_at   INTEGER,                     -- 目录：上次完整列举时间
    stat_at     INTEGER,                     -- 上次 stat 时间
    policy      TEXT,                        -- 单独覆盖的缓存策略（json）
    pinned      INTEGER NOT NULL DEFAULT 0,
    UNIQUE(parent_id, name)
);
CREATE INDEX IF NOT EXISTS nodes_remote_key ON nodes(remote_key);

CREATE TABLE IF NOT EXISTS changes (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id  INTEGER NOT NULL,
    deleted  INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS uploads (
    item_id     INTEGER PRIMARY KEY,
    remote_key  TEXT NOT NULL,
    upload_id   TEXT,                        -- NULL 表示小对象整体 put
    part_size   INTEGER NOT NULL,
    parts_done  TEXT NOT NULL DEFAULT '[]',  -- json: [[part_no, etag], ...]
    base_etag   TEXT,                        -- 打开写入时远端 etag，用于 If-Match
    state       TEXT NOT NULL,               -- queued | uploading | failed
    updated_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS blocks (
    item_id   INTEGER NOT NULL,
    etag      TEXT    NOT NULL,
    block_no  INTEGER NOT NULL,
    size      INTEGER NOT NULL,
    last_used INTEGER NOT NULL,
    PRIMARY KEY(item_id, etag, block_no)
);
CREATE INDEX IF NOT EXISTS blocks_last_used ON blocks(last_used);

-- 由操作系统持有副本的已下载文件（macOS File Provider / Windows Cloud Files）。
-- 这些平台上内容缓存由系统管理，引擎只记录下载时间并计算 TTL 到期的候选项。
CREATE TABLE IF NOT EXISTS materialized (
    item_id INTEGER PRIMARY KEY,
    at      INTEGER NOT NULL
);
"#;

pub struct MetaStore {
    conn: Mutex<Connection>,
}

impl MetaStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.ensure_root()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.ensure_root()?;
        Ok(store)
    }

    fn ensure_root(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO nodes(id, parent_id, name, kind, remote_key) VALUES (?1, 0, '', 1, '')",
            params![ItemId::ROOT.0],
        )?;
        Ok(())
    }

    pub fn get(&self, id: ItemId) -> Result<Option<Node>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, parent_id, name, kind, size, mtime, etag, remote_key, policy, pinned FROM nodes WHERE id = ?1",
            params![id.0],
            row_to_node,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn lookup(&self, parent: ItemId, name: &str) -> Result<Option<Node>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, parent_id, name, kind, size, mtime, etag, remote_key, policy, pinned FROM nodes WHERE parent_id = ?1 AND name = ?2",
            params![parent.0, name],
            row_to_node,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn children(&self, parent: ItemId) -> Result<Vec<Node>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, parent_id, name, kind, size, mtime, etag, remote_key, policy, pinned FROM nodes WHERE parent_id = ?1 ORDER BY name",
        )?;
        let rows = stmt.query_map(params![parent.0], row_to_node)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// 插入或按 (parent, name) 更新，返回 ItemId，并写入变更日志。
    pub fn upsert(
        &self,
        parent: ItemId,
        name: &str,
        e: &Entry,
        remote_key: &str,
    ) -> Result<ItemId> {
        let conn = self.conn.lock().unwrap();
        let mtime = e.mtime.map(to_secs);
        // 列目录会对每个条目调用 upsert；只有内容真正变化时才写变更日志，
        // 否则 File Provider 每次刷新都会重新枚举所有条目。
        let existing: Option<ExistingRow> = conn
            .query_row(
                "SELECT id, kind, size, mtime, etag, remote_key FROM nodes WHERE parent_id = ?1 AND name = ?2",
                params![parent.0, name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()?;
        let unchanged = matches!(&existing, Some((_, k, sz, mt, tag, key))
            if *k == kind_to_i(e.kind) && *sz == e.size as i64 && *mt == mtime && *tag == e.etag && key == remote_key);
        conn.execute(
            "INSERT INTO nodes(parent_id, name, kind, size, mtime, etag, remote_key, stat_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(parent_id, name) DO UPDATE SET
               kind = excluded.kind, size = excluded.size, mtime = excluded.mtime,
               etag = excluded.etag, remote_key = excluded.remote_key, stat_at = excluded.stat_at",
            params![
                parent.0,
                name,
                kind_to_i(e.kind),
                e.size as i64,
                mtime,
                e.etag,
                remote_key,
                now()
            ],
        )?;
        let id: i64 = match existing {
            Some((id, ..)) => id,
            None => conn.query_row(
                "SELECT id FROM nodes WHERE parent_id = ?1 AND name = ?2",
                params![parent.0, name],
                |r| r.get(0),
            )?,
        };
        if !unchanged {
            conn.execute(
                "INSERT INTO changes(item_id, deleted) VALUES (?1, 0)",
                params![id],
            )?;
        }
        Ok(ItemId(id as u64))
    }

    pub fn remove(&self, id: ItemId) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM nodes WHERE id = ?1", params![id.0])?;
        conn.execute(
            "INSERT INTO changes(item_id, deleted) VALUES (?1, 1)",
            params![id.0],
        )?;
        Ok(())
    }

    pub fn rename(
        &self,
        id: ItemId,
        new_parent: ItemId,
        new_name: &str,
        new_key: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE nodes SET parent_id = ?2, name = ?3, remote_key = ?4 WHERE id = ?1",
            params![id.0, new_parent.0, new_name, new_key],
        )?;
        conn.execute(
            "INSERT INTO changes(item_id, deleted) VALUES (?1, 0)",
            params![id.0],
        )?;
        Ok(())
    }

    /// 待上传记录跟随节点改名：目标键换成新键，分片进度作废（分片绑定在旧键上），
    /// If-Match 基准换成新键上对象的 etag。
    pub fn rekey_upload(&self, id: ItemId, key: &str, base_etag: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE uploads SET remote_key = ?2, upload_id = NULL, parts_done = '[]', base_etag = ?3 WHERE item_id = ?1",
            params![id.0, key, base_etag],
        )?;
        Ok(())
    }

    pub fn mark_listed(&self, dir: ItemId) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE nodes SET listed_at = ?2 WHERE id = ?1",
            params![dir.0, now()],
        )?;
        Ok(())
    }

    /// 在 `since` 之后被列举过的目录（用户最近看过的"热"目录），供远端变更轮询。
    pub fn recently_listed_dirs(&self, since: SystemTime) -> Result<Vec<ItemId>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id FROM nodes WHERE kind = 1 AND listed_at IS NOT NULL AND listed_at >= ?1 ORDER BY listed_at DESC",
        )?;
        let rows = stmt.query_map(params![to_secs(since)], |r| r.get::<_, i64>(0))?;
        Ok(rows
            .map(|r| r.map(|v| ItemId(v as u64)))
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn listed_at(&self, dir: ItemId) -> Result<Option<SystemTime>> {
        let conn = self.conn.lock().unwrap();
        let v: Option<i64> = conn.query_row(
            "SELECT listed_at FROM nodes WHERE id = ?1",
            params![dir.0],
            |r| r.get(0),
        )?;
        Ok(v.map(from_secs))
    }

    pub fn set_pinned(&self, id: ItemId, pinned: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE nodes SET pinned = ?2 WHERE id = ?1",
            params![id.0, pinned as i32],
        )?;
        conn.execute(
            "INSERT INTO changes(item_id, deleted) VALUES (?1, 0)",
            params![id.0],
        )?;
        Ok(())
    }

    pub fn latest_anchor(&self) -> Result<ChangeAnchor> {
        let conn = self.conn.lock().unwrap();
        let seq: i64 = conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM changes", [], |r| {
            r.get(0)
        })?;
        Ok(ChangeAnchor(seq as u64))
    }

    /// 返回 `anchor` 之后的变更（已去重，删除优先）。
    pub fn changes_since(&self, anchor: ChangeAnchor) -> Result<(Vec<Change>, ChangeAnchor)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT item_id, MAX(deleted), MAX(seq) FROM changes WHERE seq > ?1 GROUP BY item_id ORDER BY MAX(seq)",
        )?;
        let rows = stmt.query_map(params![anchor.0], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)? != 0,
                r.get::<_, i64>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        let mut last = anchor;
        for row in rows {
            let (item_id, deleted, seq) = row?;
            last = ChangeAnchor(seq as u64);
            if deleted {
                out.push(Change::Delete(ItemId(item_id as u64)));
            } else if let Some(n) = conn
                .query_row(
                    "SELECT id, parent_id, name, kind, size, mtime, etag, remote_key, policy, pinned FROM nodes WHERE id = ?1",
                    params![item_id],
                    row_to_node,
                )
                .optional()?
            {
                out.push(Change::Upsert(n));
            }
        }
        Ok((out, last))
    }

    // ---- blocks ----

    pub fn record_block(&self, id: ItemId, etag: &str, block_no: u64, size: u64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO blocks(item_id, etag, block_no, size, last_used) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id.0, etag, block_no as i64, size as i64, now()],
        )?;
        Ok(())
    }

    pub fn touch_block(&self, id: ItemId, etag: &str, block_no: u64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE blocks SET last_used = ?4 WHERE item_id = ?1 AND etag = ?2 AND block_no = ?3",
            params![id.0, etag, block_no as i64, now()],
        )?;
        Ok(())
    }

    pub fn cached_blocks(&self, id: ItemId, etag: &str) -> Result<Vec<u64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT block_no FROM blocks WHERE item_id = ?1 AND etag = ?2 ORDER BY block_no",
        )?;
        let rows = stmt.query_map(params![id.0, etag], |r| r.get::<_, i64>(0))?;
        Ok(rows
            .map(|r| r.map(|v| v as u64))
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn forget_blocks(&self, id: ItemId) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM blocks WHERE item_id = ?1", params![id.0])?;
        Ok(())
    }

    pub fn total_cached_bytes(&self) -> Result<u64> {
        let conn = self.conn.lock().unwrap();
        let v: i64 = conn.query_row("SELECT COALESCE(SUM(size), 0) FROM blocks", [], |r| {
            r.get(0)
        })?;
        Ok(v as u64)
    }

    /// 所有有本地块的节点：(id, 最近访问时间, 占用字节)，按最近访问升序（LRU 在前）。
    pub fn cached_items(&self) -> Result<Vec<CachedItem>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT item_id, MAX(last_used), SUM(size) FROM blocks GROUP BY item_id ORDER BY MAX(last_used) ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(CachedItem {
                id: ItemId(r.get::<_, i64>(0)? as u64),
                last_used: from_secs(r.get(1)?),
                bytes: r.get::<_, i64>(2)? as u64,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn pinned_items(&self) -> Result<Vec<ItemId>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id FROM nodes WHERE pinned = 1 ORDER BY id")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        Ok(rows
            .map(|r| r.map(|v| ItemId(v as u64)))
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// 设置节点自身的缓存策略覆盖；`None` 表示继承。
    pub fn set_policy(&self, id: ItemId, policy: Option<&CachePolicy>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let v = policy.map(|p| serde_json::to_string(p).unwrap_or_default());
        conn.execute(
            "UPDATE nodes SET policy = ?2 WHERE id = ?1",
            params![id.0, v],
        )?;
        conn.execute(
            "INSERT INTO changes(item_id, deleted) VALUES (?1, 0)",
            params![id.0],
        )?;
        Ok(())
    }

    // ---- materialized（系统持有副本）----

    pub fn mark_materialized(&self, id: ItemId) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO materialized(item_id, at) VALUES (?1, ?2)",
            params![id.0, now()],
        )?;
        Ok(())
    }

    pub fn mark_evicted(&self, id: ItemId) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM materialized WHERE item_id = ?1", params![id.0])?;
        Ok(())
    }

    /// 用系统报告的完整集合替换记录：新出现的记为现在下载，消失的删除。
    pub fn sync_materialized(&self, ids: &[ItemId]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS mat_sync(item_id INTEGER PRIMARY KEY); DELETE FROM mat_sync;")?;
        for id in ids {
            tx.execute(
                "INSERT OR IGNORE INTO mat_sync(item_id) VALUES (?1)",
                params![id.0],
            )?;
        }
        tx.execute(
            "DELETE FROM materialized WHERE item_id NOT IN (SELECT item_id FROM mat_sync)",
            [],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO materialized(item_id, at) SELECT item_id, ?1 FROM mat_sync",
            params![now()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn materialized_items(&self) -> Result<Vec<(ItemId, SystemTime)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT item_id, at FROM materialized ORDER BY at")?;
        let rows = stmt.query_map([], |r| {
            Ok((ItemId(r.get::<_, i64>(0)? as u64), from_secs(r.get(1)?)))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ---- uploads ----

    pub fn upload_get(&self, id: ItemId) -> Result<Option<UploadRecord>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT item_id, remote_key, upload_id, part_size, parts_done, base_etag, state FROM uploads WHERE item_id = ?1",
            params![id.0],
            row_to_upload,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn upload_put(&self, rec: &UploadRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO uploads(item_id, remote_key, upload_id, part_size, parts_done, base_etag, state, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                rec.item_id.0,
                rec.remote_key,
                rec.upload_id.as_ref().map(|u| &u.0),
                rec.part_size as i64,
                serde_json::to_string(&rec.parts_done).unwrap_or_else(|_| "[]".into()),
                rec.base_etag,
                rec.state.as_str(),
                now()
            ],
        )?;
        Ok(())
    }

    pub fn upload_remove(&self, id: ItemId) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM uploads WHERE item_id = ?1", params![id.0])?;
        Ok(())
    }

    pub fn uploads_pending(&self) -> Result<Vec<UploadRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT item_id, remote_key, upload_id, part_size, parts_done, base_etag, state FROM uploads ORDER BY updated_at",
        )?;
        let rows = stmt.query_map([], row_to_upload)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

/// upsert 用来比对的现有行：(id, kind, size, mtime, etag, remote_key)。
type ExistingRow = (i64, i64, i64, Option<i64>, Option<String>, String);

/// 驱逐器视角的一个已缓存节点。
#[derive(Debug, Clone)]
pub struct CachedItem {
    pub id: ItemId,
    pub last_used: SystemTime,
    pub bytes: u64,
}

/// 一条写回记录，进程重启后据此恢复分片上传。
#[derive(Debug, Clone)]
pub struct UploadRecord {
    pub item_id: ItemId,
    pub remote_key: String,
    pub upload_id: Option<UploadId>,
    pub part_size: u64,
    pub parts_done: Vec<(u32, PartEtag)>,
    pub base_etag: Option<String>,
    pub state: UploadState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadState {
    Queued,
    Uploading,
    Failed,
}

impl UploadState {
    fn as_str(self) -> &'static str {
        match self {
            UploadState::Queued => "queued",
            UploadState::Uploading => "uploading",
            UploadState::Failed => "failed",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "uploading" => UploadState::Uploading,
            "failed" => UploadState::Failed,
            _ => UploadState::Queued,
        }
    }
}

fn row_to_node(r: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
    let policy: Option<String> = r.get(8)?;
    let pinned: i64 = r.get(9)?;
    Ok(Node {
        id: ItemId(r.get::<_, i64>(0)? as u64),
        parent: ItemId(r.get::<_, i64>(1)? as u64),
        name: r.get(2)?,
        kind: if r.get::<_, i64>(3)? == 1 {
            Kind::Dir
        } else {
            Kind::File
        },
        size: r.get::<_, i64>(4)? as u64,
        mtime: r.get::<_, Option<i64>>(5)?.map(from_secs),
        etag: r.get(6)?,
        remote_key: r.get(7)?,
        // LocalState 由 ContentCache 结合 blocks 表计算，这里给默认值。
        local: if pinned != 0 {
            LocalState::Pinned
        } else {
            LocalState::Cloud
        },
        policy: policy.and_then(|p| serde_json::from_str(&p).ok()),
    })
}

fn row_to_upload(r: &rusqlite::Row<'_>) -> rusqlite::Result<UploadRecord> {
    let parts: String = r.get(4)?;
    let state: String = r.get(6)?;
    Ok(UploadRecord {
        item_id: ItemId(r.get::<_, i64>(0)? as u64),
        remote_key: r.get(1)?,
        upload_id: r.get::<_, Option<String>>(2)?.map(UploadId),
        part_size: r.get::<_, i64>(3)? as u64,
        parts_done: serde_json::from_str(&parts).unwrap_or_default(),
        base_etag: r.get(5)?,
        state: UploadState::parse(&state),
    })
}

fn kind_to_i(k: Kind) -> i64 {
    match k {
        Kind::File => 0,
        Kind::Dir => 1,
    }
}

fn now() -> i64 {
    to_secs(SystemTime::now())
}
fn to_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
fn from_secs(s: i64) -> SystemTime {
    UNIX_EPOCH + std::time::Duration::from_secs(s.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_lookup_changes() {
        let m = MetaStore::open_in_memory().unwrap();
        let e = Entry {
            key: "a.txt".into(),
            kind: Kind::File,
            size: 3,
            mtime: None,
            etag: Some("x".into()),
            version: None,
        };
        let id = m.upsert(ItemId::ROOT, "a.txt", &e, "a.txt").unwrap();
        assert_eq!(m.lookup(ItemId::ROOT, "a.txt").unwrap().unwrap().id, id);
        let (changes, anchor) = m.changes_since(ChangeAnchor(0)).unwrap();
        assert_eq!(changes.len(), 1);
        m.remove(id).unwrap();
        let (changes, _) = m.changes_since(anchor).unwrap();
        assert!(matches!(changes[0], Change::Delete(d) if d == id));
    }
}
