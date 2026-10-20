//! 基于 Apache OpenDAL 的通用后端：一套实现接入 OSS、WebDAV、Dropbox 等服务。
//!
//! OpenDAL 已把各服务抹平成"路径 + 元数据"的接口，这里只做三件事：
//! 把它映射到 [`Backend`] 的对象语义，按服务能力模拟缺失的条件写入，
//! 以及注入带代理的 HTTP 客户端。

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::path::Path;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use ndisk_core::{
    Backend, Capabilities, Entry, Error, Kind, ListPage, PartEtag, PutOptions, Result, UploadId,
};
use opendal::layers::TimeoutLayer;
use opendal::{
    services, Capability, ErrorKind, HttpTransporter, Metadata, MetadataBuilder, Operator,
};
use opendal_http_transport_reqwest::ReqwestTransport;
use tokio::io::AsyncReadExt;

/// 支持的服务。新增服务时在此登记，并打开 `opendal` 对应的 feature。
pub const SCHEMES: &[&str] = &["oss", "webdav", "dropbox"];

/// 流式上传的分块大小（仅对支持分片写入的服务生效，如 OSS）。
const CHUNK: usize = 8 * 1024 * 1024;

pub struct OpendalBackend {
    op: Operator,
    scheme: &'static str,
    cap: Capability,
    /// WebDAV 大文件绕过 OpenDAL 直接流式 PUT。
    dav: Option<DavDirect>,
}

impl OpendalBackend {
    /// `secret` 来自系统凭据库：纯字符串填入该服务的默认密钥项，
    /// JSON 对象（`{"password": "..."}`）则按键合并进 `options`。
    pub async fn connect(
        scheme: &str,
        options: &BTreeMap<String, String>,
        secret: Option<String>,
        proxy: Option<&str>,
    ) -> Result<Self> {
        let mut opts: HashMap<String, String> = options
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if let Some(s) = secret
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            merge_secret(scheme, &mut opts, &s)?;
        }
        let client = http_client(proxy)?;
        let dav = (scheme == "webdav")
            .then(|| DavDirect::new(&opts, client.clone()))
            .transpose()?;
        let (scheme, op) = match scheme {
            "oss" => ("oss", Operator::from_iter::<services::Oss>(opts)),
            "webdav" => ("webdav", Operator::from_iter::<services::Webdav>(opts)),
            "dropbox" => ("dropbox", Operator::from_iter::<services::Dropbox>(opts)),
            "memory" => ("memory", Operator::from_iter::<services::Memory>(opts)),
            other => {
                return Err(Error::Config(format!(
                    "unsupported backend scheme '{other}' (supported: {})",
                    SCHEMES.join(", ")
                )))
            }
        };
        let op = op.map_err(map_err)?;
        let op = if scheme == "memory" {
            op
        } else {
            let transport = HttpTransporter::new(ReqwestTransport::new(client));
            let ctx = op.context().with_http_transport(transport);
            op.with_context(ctx).layer(
                TimeoutLayer::new()
                    .with_timeout(Duration::from_secs(120))
                    .with_io_timeout(Duration::from_secs(60)),
            )
        };
        let cap = op.info().capability();
        let b = Self {
            op,
            scheme,
            cap,
            dav,
        };
        // 及早暴露地址、凭据错误，而不是等到第一次列目录。
        b.op.list_with("/").limit(1).await.map_err(map_err)?;
        tracing::info!(scheme, "opendal backend connected");
        Ok(b)
    }

    async fn stat_entry(&self, key: &str) -> Result<Option<Entry>> {
        match self.op.stat(key).await {
            Ok(m) => Ok(Some(entry(key, &m))),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            // 按文件名查一个目录时，Apache 等 WebDAV 服务回 301 到 `key/`，OpenDAL 报为永久错误。
            // 再按目录查一次；仍失败则报原错误。
            Err(e) if !key.is_empty() && !key.ends_with('/') => {
                match self.op.stat(&format!("{key}/")).await {
                    Ok(m) if m.is_dir() => Ok(Some(entry(&format!("{key}/"), &m))),
                    _ => Err(map_err(e)),
                }
            }
            Err(e) => Err(map_err(e)),
        }
    }

    /// 写入返回的元数据常常不全（WebDAV 不回 etag），补一次 stat，
    /// 保证节点的 etag 与之后列目录看到的一致，避免被误判为远端变更。
    async fn written(&self, key: &str, m: Metadata) -> Result<Entry> {
        if m.etag().is_some() && m.last_modified().is_some() {
            return Ok(entry(key, &m));
        }
        Ok(self
            .stat_entry(key)
            .await?
            .unwrap_or_else(|| entry(key, &m)))
    }

    /// 服务不支持条件写入时用"先 stat 再比较"模拟。有竞争窗口，
    /// 但足以在常见的"另一台设备改过"场景里产生冲突副本而不是静默覆盖。
    async fn check_precondition(&self, key: &str, opts: &PutOptions) -> Result<()> {
        let Some(expected) = &opts.if_match else {
            return Ok(());
        };
        if self.native_condition(expected) {
            return Ok(());
        }
        let actual = self.stat_entry(key).await?;
        let ok = match &actual {
            None => expected.is_empty(),
            // 无 etag 的服务只能用 mtime+size 版本键比较，与写入基准的来源一致。
            Some(e) => !expected.is_empty() && e.cache_key() == *expected,
        };
        if ok {
            Ok(())
        } else {
            Err(Error::Conflict {
                key: key.into(),
                expected: Some(expected.clone()),
                actual: actual.map(|e| e.cache_key()),
            })
        }
    }

    fn native_condition(&self, expected: &str) -> bool {
        if expected.is_empty() {
            self.cap.write_with_if_not_exists
        } else {
            self.cap.write_with_if_match
        }
    }
}

#[async_trait]
impl Backend for OpendalBackend {
    fn name(&self) -> &'static str {
        self.scheme
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // 仅文件级：OpenDAL 的 rename 不接受目录。
            native_rename: self.cap.rename,
            range_read: self.cap.read,
            // 分片续传需要显式的 upload id，OpenDAL 不暴露；整体流式写入走 `put_file`。
            multipart: false,
            etag: self.scheme != "memory",
            conditional_put: self.cap.write_with_if_match,
            versioning: false,
        }
    }

    async fn list(&self, prefix: &str, _cursor: Option<&str>) -> Result<ListPage> {
        let path = if prefix.is_empty() { "/" } else { prefix };
        let mut lister = self.op.lister(path).await.map_err(map_err)?;
        let mut entries = Vec::new();
        while let Some(e) = lister.try_next().await.map_err(map_err)? {
            let p = e.path();
            // OpenDAL 会把被列举的目录本身也作为一项返回。
            if p == prefix || p == "/" || p.is_empty() {
                continue;
            }
            entries.push(entry(p, e.metadata()));
        }
        Ok(ListPage {
            entries,
            next: None,
        })
    }

    async fn stat(&self, key: &str) -> Result<Option<Entry>> {
        self.stat_entry(key).await
    }

    async fn read(&self, key: &str, range: Range<u64>) -> Result<Bytes> {
        let buf = self.op.read_with(key).range(range).await.map_err(map_err)?;
        Ok(buf.to_bytes())
    }

    async fn put(&self, key: &str, data: Bytes, opts: PutOptions) -> Result<Entry> {
        self.check_precondition(key, &opts).await?;
        let mut w = self.op.write_with(key, data);
        if let Some(ct) = &opts.content_type {
            w = w.content_type(ct);
        }
        if let Some(m) = opts
            .if_match
            .as_deref()
            .filter(|m| self.native_condition(m))
        {
            w = if m.is_empty() {
                w.if_not_exists(true)
            } else {
                w.if_match(m)
            };
        }
        let m = w.await.map_err(|e| conflict_or(e, key, &opts))?;
        self.written(key, m).await
    }

    async fn put_file(&self, key: &str, path: &Path, opts: PutOptions) -> Result<Entry> {
        if let Some(dav) = &self.dav {
            self.check_precondition(key, &opts).await?;
            match dav.put_file(key, path, &opts).await {
                // 父集合不存在：交给 OpenDAL 逐级 MKCOL 后重试一次。
                Err(DavPutError::ParentMissing) => {
                    if let Some(i) = key.rfind('/') {
                        self.op.create_dir(&key[..=i]).await.map_err(map_err)?;
                    }
                    dav.put_file(key, path, &opts).await.map_err(Error::from)?;
                }
                r => r.map_err(Error::from)?,
            }
            return self
                .stat_entry(key)
                .await?
                .ok_or_else(|| Error::NotFound(key.into()));
        }
        // 不支持分片写入的服务（Dropbox）只接受一次性写入，OpenDAL 会拒绝第二次 write。
        if !self.cap.write_can_multi {
            let data = Bytes::from(tokio::fs::read(path).await?);
            return self.put(key, data, opts).await;
        }
        self.check_precondition(key, &opts).await?;
        let mut w = self.op.writer_with(key).chunk(CHUNK);
        if let Some(ct) = &opts.content_type {
            w = w.content_type(ct);
        }
        if let Some(m) = opts
            .if_match
            .as_deref()
            .filter(|m| self.native_condition(m))
        {
            w = if m.is_empty() {
                w.if_not_exists(true)
            } else {
                w.if_match(m)
            };
        }
        let mut writer = w.await.map_err(map_err)?;
        let mut f = tokio::fs::File::open(path).await?;
        loop {
            let mut buf = vec![0u8; CHUNK];
            let n = read_full(&mut f, &mut buf).await?;
            if n == 0 {
                break;
            }
            buf.truncate(n);
            if let Err(e) = writer.write(buf).await {
                let _ = writer.abort().await;
                return Err(map_err(e));
            }
        }
        let m = writer
            .close()
            .await
            .map_err(|e| conflict_or(e, key, &opts))?;
        self.written(key, m).await
    }

    async fn multipart_start(&self, _key: &str, _opts: PutOptions) -> Result<UploadId> {
        Err(Error::Unsupported("multipart upload"))
    }

    async fn multipart_put_part(
        &self,
        _key: &str,
        _upload: &UploadId,
        _part_number: u32,
        _data: Bytes,
    ) -> Result<PartEtag> {
        Err(Error::Unsupported("multipart upload"))
    }

    async fn multipart_complete(
        &self,
        _key: &str,
        _upload: &UploadId,
        _parts: &[(u32, PartEtag)],
    ) -> Result<Entry> {
        Err(Error::Unsupported("multipart upload"))
    }

    async fn multipart_abort(&self, _key: &str, _upload: &UploadId) -> Result<()> {
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.op.delete(key).await.map_err(map_err)
    }

    async fn copy(&self, from: &str, to: &str) -> Result<Entry> {
        if self.cap.copy {
            let m = self.op.copy(from, to).await.map_err(map_err)?;
            return self.written(to, m).await;
        }
        let data = self.op.read(from).await.map_err(map_err)?.to_bytes();
        let m = self.op.write(to, data).await.map_err(map_err)?;
        self.written(to, m).await
    }

    async fn rename(&self, from: &str, to: &str) -> Result<Entry> {
        if !self.cap.rename {
            let e = self.copy(from, to).await?;
            self.delete(from).await?;
            return Ok(e);
        }
        self.op.rename(from, to).await.map_err(map_err)?;
        Ok(self
            .stat_entry(to)
            .await?
            .unwrap_or_else(|| entry(to, &empty_meta(to))))
    }

    async fn mkdir(&self, key: &str) -> Result<Entry> {
        let key = if key.ends_with('/') {
            key.to_string()
        } else {
            format!("{key}/")
        };
        self.op.create_dir(&key).await.map_err(map_err)?;
        Ok(Entry {
            key,
            kind: Kind::Dir,
            size: 0,
            mtime: Some(SystemTime::now()),
            etag: None,
            version: None,
        })
    }
}

fn empty_meta(key: &str) -> Metadata {
    if key.ends_with('/') {
        MetadataBuilder::dir().build()
    } else {
        MetadataBuilder::file(0).build()
    }
}

fn entry(key: &str, m: &Metadata) -> Entry {
    let kind = if m.is_dir() || key.ends_with('/') {
        Kind::Dir
    } else {
        Kind::File
    };
    let key = match kind {
        Kind::Dir if !key.ends_with('/') => format!("{key}/"),
        _ => key.to_string(),
    };
    Entry {
        key,
        kind,
        size: if kind == Kind::Dir {
            0
        } else {
            m.content_length()
        },
        mtime: m.last_modified().map(SystemTime::from),
        // 目录的 etag 对缓存无意义，且部分服务会随子项变化，忽略以免误报变更。
        etag: match kind {
            Kind::Dir => None,
            Kind::File => m.etag().map(normalize_etag),
        },
        version: m.version().map(str::to_string),
    }
}

/// 同一对象在不同响应里可能带弱标记或引号（Apache 的 PROPFIND 回 `W/"…"`，HEAD 回 `"…"`），
/// 统一去掉，保证列目录、stat 与写入后拿到的版本键一致。
fn normalize_etag(s: &str) -> String {
    let s = s.trim();
    s.strip_prefix("W/")
        .unwrap_or(s)
        .trim_matches('"')
        .to_string()
}

/// 把凭据合并进选项。纯字符串写入服务的默认密钥项。
fn merge_secret(scheme: &str, opts: &mut HashMap<String, String>, secret: &str) -> Result<()> {
    if secret.starts_with('{') {
        let map: BTreeMap<String, String> = serde_json::from_str(secret)
            .map_err(|e| Error::Config(format!("secret is not a JSON string map: {e}")))?;
        opts.extend(map);
        return Ok(());
    }
    let key = match scheme {
        "oss" => "access_key_secret",
        "webdav" => "password",
        // 有 client_id 时是可自动续期的 refresh token，否则视为长期 access token。
        "dropbox" if opts.contains_key("client_id") => "refresh_token",
        "dropbox" => "access_token",
        _ => "secret",
    };
    opts.insert(key.into(), secret.into());
    Ok(())
}

/// 代理取值与 S3 后端一致：空为直连，`system`/`env` 用系统与环境变量代理，否则为代理 URL
/// （可带 `user:pass@`）。
fn http_client(proxy: Option<&str>) -> Result<reqwest::Client> {
    let b = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(60));
    let b = match proxy.map(str::trim).filter(|p| !p.is_empty()) {
        None => b.no_proxy(),
        Some("system" | "env") => b,
        Some(url) => b.proxy(
            reqwest::Proxy::all(url)
                .map_err(|e| Error::Config(format!("invalid proxy '{url}': {e}")))?,
        ),
    };
    b.build()
        .map_err(|e| Error::Config(format!("http client: {e}")))
}

async fn read_full(f: &mut tokio::fs::File, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let r = f.read(&mut buf[n..]).await?;
        if r == 0 {
            break;
        }
        n += r;
    }
    Ok(n)
}

fn conflict_or(e: opendal::Error, key: &str, opts: &PutOptions) -> Error {
    match e.kind() {
        ErrorKind::ConditionNotMatch | ErrorKind::AlreadyExists if opts.if_match.is_some() => {
            Error::Conflict {
                key: key.into(),
                expected: opts.if_match.clone(),
                actual: None,
            }
        }
        _ => map_err(e),
    }
}

fn map_err(e: opendal::Error) -> Error {
    match e.kind() {
        ErrorKind::NotFound => Error::NotFound(e.message().to_string()),
        ErrorKind::AlreadyExists => Error::AlreadyExists(e.message().to_string()),
        ErrorKind::PermissionDenied => Error::PermissionDenied(e.to_string()),
        ErrorKind::NotADirectory => Error::NotADirectory(e.message().to_string()),
        ErrorKind::IsADirectory => Error::IsADirectory(e.message().to_string()),
        ErrorKind::ConfigInvalid => Error::Config(e.to_string()),
        ErrorKind::Unsupported => Error::Unsupported("operation not supported by this service"),
        // 限流、网络与 5xx：交给上层退避重试。
        _ => Error::backend(e),
    }
}

/// 直连 WebDAV 的流式上传。OpenDAL 的 WebDAV 写入是一次性缓冲整块，大文件会占满内存
/// （File Provider 扩展有内存上限）；这里用 reqwest 把文件作为流式请求体 PUT 上去。
struct DavDirect {
    client: reqwest::Client,
    /// `endpoint` + `root`，以 `/` 结尾。
    base: String,
    auth: DavAuth,
}

enum DavAuth {
    None,
    Basic(String, Option<String>),
    Bearer(String),
}

#[derive(Debug)]
enum DavPutError {
    ParentMissing,
    Status(reqwest::StatusCode, String),
    Io(std::io::Error),
    Http(reqwest::Error),
}

impl From<DavPutError> for Error {
    fn from(e: DavPutError) -> Self {
        match e {
            DavPutError::ParentMissing => Error::NotFound("parent collection".into()),
            DavPutError::Status(s, key) => match s.as_u16() {
                412 => Error::Conflict {
                    key,
                    expected: None,
                    actual: None,
                },
                401 | 403 => Error::PermissionDenied(format!("PUT {key}: {s}")),
                // 5xx、429 等交给上层退避重试。
                _ => Error::backend(std::io::Error::other(format!("PUT {key}: {s}"))),
            },
            DavPutError::Io(e) => Error::Io(e),
            DavPutError::Http(e) => Error::backend(e),
        }
    }
}

/// 路径段中需要编码的字符：除 RFC 3986 unreserved 外全部编码。
const SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

impl DavDirect {
    fn new(opts: &HashMap<String, String>, client: reqwest::Client) -> Result<Self> {
        let endpoint = opts
            .get("endpoint")
            .map(|e| e.trim().trim_end_matches('/').to_string())
            .filter(|e| !e.is_empty())
            .ok_or_else(|| Error::Config("webdav: endpoint is required".into()))?;
        let root = opts.get("root").map(String::as_str).unwrap_or("/");
        let root = root.trim_matches('/');
        let base = if root.is_empty() {
            format!("{endpoint}/")
        } else {
            format!("{endpoint}/{}/", encode_path(root))
        };
        let auth = match (opts.get("token"), opts.get("username")) {
            (Some(t), _) if !t.is_empty() => DavAuth::Bearer(t.clone()),
            (_, Some(u)) if !u.is_empty() => {
                DavAuth::Basic(u.clone(), opts.get("password").cloned())
            }
            _ => DavAuth::None,
        };
        Ok(Self { client, base, auth })
    }

    fn url(&self, key: &str) -> String {
        format!("{}{}", self.base, encode_path(key))
    }

    async fn put_file(
        &self,
        key: &str,
        path: &Path,
        opts: &PutOptions,
    ) -> std::result::Result<(), DavPutError> {
        let f = tokio::fs::File::open(path).await.map_err(DavPutError::Io)?;
        let len = f.metadata().await.map_err(DavPutError::Io)?.len();
        let body =
            reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::with_capacity(f, 256 * 1024));
        let mut req = self
            .client
            .put(self.url(key))
            .header(reqwest::header::CONTENT_LENGTH, len)
            .body(body);
        if let Some(ct) = &opts.content_type {
            req = req.header(reqwest::header::CONTENT_TYPE, ct);
        }
        req = match &self.auth {
            DavAuth::None => req,
            DavAuth::Basic(u, p) => req.basic_auth(u, p.as_ref()),
            DavAuth::Bearer(t) => req.bearer_auth(t),
        };
        let resp = req.send().await.map_err(DavPutError::Http)?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        // RFC 4918：父集合不存在时 PUT 返回 409。
        if status == reqwest::StatusCode::CONFLICT {
            return Err(DavPutError::ParentMissing);
        }
        Err(DavPutError::Status(status, key.to_string()))
    }
}

fn encode_path(p: &str) -> String {
    p.split('/')
        .map(|seg| percent_encoding::utf8_percent_encode(seg, SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_merging() {
        let mut o = HashMap::new();
        merge_secret("webdav", &mut o, "pw").unwrap();
        assert_eq!(o["password"], "pw");
        let mut o = HashMap::from([("client_id".to_string(), "c".to_string())]);
        merge_secret("dropbox", &mut o, "rt").unwrap();
        assert_eq!(o["refresh_token"], "rt");
        let mut o = HashMap::new();
        merge_secret("dropbox", &mut o, "at").unwrap();
        assert_eq!(o["access_token"], "at");
        let mut o = HashMap::new();
        merge_secret(
            "oss",
            &mut o,
            r#"{"access_key_secret":"s","security_token":"t"}"#,
        )
        .unwrap();
        assert_eq!(o["security_token"], "t");
        assert!(merge_secret("oss", &mut o, "{bad").is_err());
    }

    #[test]
    fn dav_urls() {
        let client = reqwest::Client::new();
        let opts = |root: &str| {
            HashMap::from([
                ("endpoint".to_string(), "https://nas.local/dav/".to_string()),
                ("root".to_string(), root.to_string()),
                ("username".to_string(), "me".to_string()),
            ])
        };
        let d = DavDirect::new(&opts("/"), client.clone()).unwrap();
        assert_eq!(
            d.url("a b/中文#?.txt"),
            "https://nas.local/dav/a%20b/%E4%B8%AD%E6%96%87%23%3F.txt"
        );
        let d = DavDirect::new(&opts("/team space/"), client.clone()).unwrap();
        assert_eq!(d.url("x.txt"), "https://nas.local/dav/team%20space/x.txt");
        assert!(matches!(d.auth, DavAuth::Basic(ref u, None) if u == "me"));
        assert!(DavDirect::new(&HashMap::new(), client).is_err());
    }

    #[test]
    fn entry_mapping() {
        let mut m = MetadataBuilder::file(5);
        m.etag("\"abc\"");
        let e = entry("a/b.txt", &m.build());
        assert_eq!(
            (e.kind, e.size, e.etag.as_deref()),
            (Kind::File, 5, Some("abc"))
        );
        assert_eq!(normalize_etag("W/\"c-65e1\""), "c-65e1");
        assert_eq!(normalize_etag("\"c-65e1\""), "c-65e1");
        let d = entry("a/c", &MetadataBuilder::dir().build());
        assert_eq!((d.key.as_str(), d.kind), ("a/c/", Kind::Dir));
    }

    #[tokio::test]
    async fn memory_roundtrip() {
        let b = OpendalBackend::connect("memory", &BTreeMap::new(), None, None)
            .await
            .unwrap();
        b.mkdir("docs").await.unwrap();
        b.put(
            "docs/a.txt",
            Bytes::from_static(b"hello"),
            Default::default(),
        )
        .await
        .unwrap();
        b.put("top.txt", Bytes::from_static(b"x"), Default::default())
            .await
            .unwrap();
        let mut root: Vec<_> = b.list("", None).await.unwrap().entries;
        root.sort_by(|a, b| a.key.cmp(&b.key));
        let keys: Vec<_> = root.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["docs/", "top.txt"]);
        let sub = b.list("docs/", None).await.unwrap().entries;
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].key, "docs/a.txt");
        assert_eq!(&b.read("docs/a.txt", 1..4).await.unwrap()[..], b"ell");
        assert!(b.stat("nope").await.unwrap().is_none());

        // 流式上传
        let dir = std::env::temp_dir().join(format!("ndisk-od-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("big");
        std::fs::write(&f, vec![7u8; CHUNK + 123]).unwrap();
        let e = b.put_file("big.bin", &f, Default::default()).await.unwrap();
        assert_eq!(e.size, (CHUNK + 123) as u64);
        std::fs::remove_dir_all(&dir).unwrap();

        // 模拟的条件写入：基准不符时报冲突
        let cur = b.stat("top.txt").await.unwrap().unwrap().cache_key();
        let stale = PutOptions {
            if_match: Some("stale".into()),
            ..Default::default()
        };
        assert!(matches!(
            b.put("top.txt", Bytes::from_static(b"y"), stale).await,
            Err(Error::Conflict { .. })
        ));
        let fresh = PutOptions {
            if_match: Some(cur),
            ..Default::default()
        };
        b.put("top.txt", Bytes::from_static(b"y"), fresh)
            .await
            .unwrap();
        let must_be_new = PutOptions {
            if_match: Some(String::new()),
            ..Default::default()
        };
        assert!(matches!(
            b.put("top.txt", Bytes::from_static(b"z"), must_be_new)
                .await,
            Err(Error::Conflict { .. })
        ));

        // 复制与改名
        b.copy("top.txt", "copy.txt").await.unwrap();
        b.rename("copy.txt", "docs/moved.txt").await.unwrap();
        assert!(b.stat("copy.txt").await.unwrap().is_none());
        assert_eq!(&b.read("docs/moved.txt", 0..1).await.unwrap()[..], b"y");
        b.delete("docs/moved.txt").await.unwrap();
        assert!(b.stat("docs/moved.txt").await.unwrap().is_none());
    }
}
