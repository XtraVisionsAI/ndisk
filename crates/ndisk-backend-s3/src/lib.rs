//! S3 后端。兼容 AWS、MinIO、Cloudflare R2、阿里云 OSS S3 接口等。
//!
//! 语义映射见 `docs/DESIGN.md` 3.2。目录用零字节 `dir/` 标记对象表示（可关闭），
//! 同时识别只有隐式前缀的目录。

use std::ops::Range;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::Client;
use bytes::Bytes;
use ndisk_core::config::{S3Auth, S3Config};
use ndisk_core::{
    Backend, Capabilities, Entry, Error, Kind, ListPage, PartEtag, PutOptions, Result, UploadId,
};

/// 与 s3fs 兼容的 mtime 元数据键。
const META_MTIME: &str = "mtime";
/// 单次 CopyObject 上限，超过走 UploadPartCopy。
const COPY_LIMIT: u64 = 5 * 1024 * 1024 * 1024;
/// 分片复制的分片大小：512 MiB，最多 10000 片，覆盖 S3 单对象 5 TiB 上限。
const COPY_PART_SIZE: u64 = 512 * 1024 * 1024;
/// S3 分片最小 5 MiB（最后一片除外）。
const MIN_PART_SIZE: u64 = 5 * 1024 * 1024;

pub struct S3Backend {
    client: Client,
    bucket: String,
    dir_markers: bool,
    copy_threshold: u64,
}

impl S3Backend {
    /// 根据配置构建客户端。`secret` 由平台层从系统凭据库取出后传入。
    pub async fn connect(
        cfg: &S3Config,
        secret: Option<String>,
        proxy: Option<&str>,
    ) -> Result<Self> {
        let http_client = build_http_client(proxy)?;
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(c) = &http_client {
            loader = loader.http_client(c.clone());
        }
        if let Some(r) = &cfg.region {
            loader = loader.region(aws_config::Region::new(r.clone()));
        }
        loader = match &cfg.auth {
            S3Auth::Default => loader,
            S3Auth::Static { access_key_id, .. } => {
                let secret = secret
                    .ok_or_else(|| Error::Config("missing secret for static credentials".into()))?;
                loader.credentials_provider(aws_credential_types::Credentials::new(
                    access_key_id,
                    secret,
                    None,
                    None,
                    "ndisk",
                ))
            }
            S3Auth::Profile { name } | S3Auth::Sso { profile: name } => loader.profile_name(name),
            S3Auth::AssumeRole {
                role_arn,
                session_name,
                external_id,
            } => {
                let mut base = aws_config::defaults(aws_config::BehaviorVersion::latest());
                if let Some(c) = &http_client {
                    base = base.http_client(c.clone());
                }
                if let Some(r) = &cfg.region {
                    base = base.region(aws_config::Region::new(r.clone()));
                }
                let base = base.load().await;
                let sts = aws_sdk_sts::Client::new(&base);
                let mut req = sts
                    .assume_role()
                    .role_arn(role_arn)
                    .role_session_name(session_name.clone().unwrap_or_else(|| "ndisk".into()));
                if let Some(e) = external_id {
                    req = req.external_id(e);
                }
                let out = req.send().await.map_err(Error::backend)?;
                let c = out
                    .credentials()
                    .ok_or_else(|| Error::Config("STS returned no credentials".into()))?;
                loader.credentials_provider(aws_credential_types::Credentials::new(
                    c.access_key_id(),
                    c.secret_access_key(),
                    Some(c.session_token().to_string()),
                    Some(UNIX_EPOCH + Duration::from_secs(c.expiration().secs().max(0) as u64)),
                    "ndisk-sts",
                ))
            }
            S3Auth::WebIdentity { .. } => {
                // aws-config 通过 AWS_ROLE_ARN / AWS_WEB_IDENTITY_TOKEN_FILE 环境变量支持；
                // 平台层在调用前设置这两个变量即可。
                loader
            }
        };
        let shared = loader.load().await;
        let mut b = aws_sdk_s3::config::Builder::from(&shared);
        if let Some(ep) = &cfg.endpoint {
            b = b.endpoint_url(ep);
        }
        b = b.force_path_style(cfg.force_path_style);
        let client = Client::from_conf(b.build());
        Ok(Self {
            client,
            bucket: cfg.bucket.clone(),
            dir_markers: cfg.dir_markers,
            copy_threshold: cfg
                .copy_threshold
                .unwrap_or(COPY_LIMIT)
                .clamp(MIN_PART_SIZE, COPY_LIMIT),
        })
    }

    fn entry_from_head(
        &self,
        key: &str,
        size: i64,
        etag: Option<&str>,
        last_modified: Option<&aws_smithy_types::DateTime>,
        meta: Option<&std::collections::HashMap<String, String>>,
    ) -> Entry {
        let kind = if key.ends_with('/') {
            Kind::Dir
        } else {
            Kind::File
        };
        let mtime = meta
            .and_then(|m| m.get(META_MTIME))
            .and_then(|v| v.parse::<f64>().ok())
            .map(|s| UNIX_EPOCH + Duration::from_secs_f64(s))
            .or_else(|| {
                last_modified.map(|d| UNIX_EPOCH + Duration::from_secs(d.secs().max(0) as u64))
            });
        Entry {
            key: key.to_string(),
            kind,
            size: size.max(0) as u64,
            mtime,
            etag: etag.map(normalize_etag),
            version: None,
        }
    }
}

fn normalize_etag(e: &str) -> String {
    e.trim_matches('"').to_string()
}

fn mtime_meta(t: Option<SystemTime>) -> Option<String> {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| format!("{:.6}", d.as_secs_f64()))
}

impl S3Backend {
    /// 大于 5 GiB 的对象用 UploadPartCopy 分片做服务端复制。元数据（含 mtime）需要显式带上。
    async fn multipart_copy(&self, source: &str, to: &str, size: u64) -> Result<()> {
        let head = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(source_key(source, &self.bucket))
            .send()
            .await
            .map_err(Error::backend)?;
        let mut create = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(to)
            .set_metadata(head.metadata().cloned());
        if let Some(ct) = head.content_type() {
            create = create.content_type(ct);
        }
        let upload_id = create
            .send()
            .await
            .map_err(Error::backend)?
            .upload_id()
            .map(str::to_string)
            .ok_or_else(|| Error::Config("no upload id returned".into()))?;

        let result: Result<Vec<CompletedPart>> = async {
            let mut parts = Vec::new();
            for (i, r) in copy_ranges(size, COPY_PART_SIZE.min(self.copy_threshold))
                .into_iter()
                .enumerate()
            {
                let part_number = i as i32 + 1;
                let out = self
                    .client
                    .upload_part_copy()
                    .bucket(&self.bucket)
                    .key(to)
                    .upload_id(&upload_id)
                    .part_number(part_number)
                    .copy_source(source)
                    .copy_source_range(format!("bytes={}-{}", r.start, r.end - 1))
                    .send()
                    .await
                    .map_err(Error::backend)?;
                let etag = out
                    .copy_part_result()
                    .and_then(|c| c.e_tag())
                    .ok_or_else(|| Error::Config("no part etag returned".into()))?;
                parts.push(
                    CompletedPart::builder()
                        .part_number(part_number)
                        .e_tag(etag)
                        .build(),
                );
            }
            Ok(parts)
        }
        .await;

        match result {
            Ok(parts) => {
                self.client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(to)
                    .upload_id(&upload_id)
                    .multipart_upload(
                        CompletedMultipartUpload::builder()
                            .set_parts(Some(parts))
                            .build(),
                    )
                    .send()
                    .await
                    .map_err(Error::backend)?;
                Ok(())
            }
            Err(e) => {
                let _ = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(to)
                    .upload_id(&upload_id)
                    .send()
                    .await;
                Err(e)
            }
        }
    }
}

/// 根据网络配置构造 HTTP 客户端。`None` 表示使用 SDK 默认客户端（直连）。
///
/// - `"env"` / `"system"`：读取 `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY`（macOS 系统代理由平台层转成环境变量传入）；
/// - `http://[user:pass@]host:port`：所有流量走该代理。
fn build_http_client(
    proxy: Option<&str>,
) -> Result<Option<aws_smithy_runtime_api::client::http::SharedHttpClient>> {
    use aws_smithy_http_client::{proxy::ProxyConfig, tls, Builder, Connector};
    let Some(p) = proxy.map(str::trim).filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    let cfg = match p {
        "env" | "system" => ProxyConfig::from_env(),
        url => {
            let (url, auth) = split_userinfo(url);
            let mut c = ProxyConfig::all(url.as_str())
                .map_err(|e| Error::Config(format!("invalid proxy '{p}': {e}")))?;
            if let Some((u, pw)) = auth {
                c = c.with_basic_auth(u, pw);
            }
            c
        }
    };
    let client = Builder::new().build_with_connector_fn(move |settings, components| {
        let mut b = Connector::builder().proxy_config(cfg.clone());
        b.set_connector_settings(settings.cloned());
        if let Some(rc) = components {
            b.set_sleep_impl(rc.sleep_impl());
        }
        b.tls_provider(tls::Provider::Rustls(
            tls::rustls_provider::CryptoMode::AwsLc,
        ))
        .build()
    });
    Ok(Some(client))
}

/// 把 `scheme://user:pass@host` 拆成 `scheme://host` 与凭据。
fn split_userinfo(url: &str) -> (String, Option<(String, String)>) {
    let Some((scheme, rest)) = url.split_once("://") else {
        return (url.to_string(), None);
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_end].rsplit_once('@') {
        Some((userinfo, host)) => {
            let (u, p) = userinfo.split_once(':').unwrap_or((userinfo, ""));
            (
                format!("{scheme}://{host}{}", &rest[authority_end..]),
                Some((percent_decode(u), percent_decode(p))),
            )
        }
        None => (url.to_string(), None),
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// CopySource 头需要 URL 编码（保留 `/`），否则中文、空格、`+` 等键名会复制失败。
fn copy_source(bucket: &str, key: &str) -> String {
    let mut out = String::with_capacity(bucket.len() + key.len() + 1);
    out.push_str(bucket);
    out.push('/');
    for b in key.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 从已编码的 CopySource 反推原始键（仅用于 HEAD 源对象）。
fn source_key(source: &str, bucket: &str) -> String {
    percent_decode(
        source
            .strip_prefix(bucket)
            .and_then(|s| s.strip_prefix('/'))
            .unwrap_or(source),
    )
}

fn copy_ranges(size: u64, part: u64) -> Vec<Range<u64>> {
    (0..size.div_ceil(part))
        .map(|i| i * part..((i + 1) * part).min(size))
        .collect()
}

#[async_trait]
impl Backend for S3Backend {
    fn name(&self) -> &'static str {
        "s3"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_rename: false,
            range_read: true,
            multipart: true,
            etag: true,
            conditional_put: true,
            versioning: false,
        }
    }

    async fn list(&self, prefix: &str, cursor: Option<&str>) -> Result<ListPage> {
        let mut req = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .delimiter("/")
            .max_keys(1000);
        if let Some(c) = cursor {
            req = req.continuation_token(c);
        }
        let out = req.send().await.map_err(Error::backend)?;
        let mut entries = Vec::new();
        for p in out.common_prefixes() {
            if let Some(k) = p.prefix() {
                entries.push(Entry {
                    key: k.to_string(),
                    kind: Kind::Dir,
                    size: 0,
                    mtime: None,
                    etag: None,
                    version: None,
                });
            }
        }
        for o in out.contents() {
            let Some(k) = o.key() else { continue };
            if k == prefix {
                continue; // 目录自身标记
            }
            if k.ends_with('/') {
                // 显式标记对象；若同名 common prefix 已存在则跳过
                if entries.iter().any(|e| e.key == k) {
                    continue;
                }
                entries.push(Entry {
                    key: k.to_string(),
                    kind: Kind::Dir,
                    size: 0,
                    mtime: None,
                    etag: None,
                    version: None,
                });
                continue;
            }
            entries.push(self.entry_from_head(
                k,
                o.size().unwrap_or(0),
                o.e_tag(),
                o.last_modified(),
                None,
            ));
        }
        let next = if out.is_truncated().unwrap_or(false) {
            out.next_continuation_token().map(str::to_string)
        } else {
            None
        };
        Ok(ListPage { entries, next })
    }

    async fn stat(&self, key: &str) -> Result<Option<Entry>> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(h) => Ok(Some(self.entry_from_head(
                key,
                h.content_length().unwrap_or(0),
                h.e_tag(),
                h.last_modified(),
                h.metadata(),
            ))),
            Err(e) => {
                let svc = e.as_service_error();
                if svc.map(|s| s.is_not_found()).unwrap_or(false)
                    || e.raw_response()
                        .map(|r| r.status().as_u16() == 404)
                        .unwrap_or(false)
                {
                    if key.ends_with('/') {
                        // 隐式目录：只要有该前缀下的对象即视为目录
                        let out = self
                            .client
                            .list_objects_v2()
                            .bucket(&self.bucket)
                            .prefix(key)
                            .max_keys(1)
                            .send()
                            .await
                            .map_err(Error::backend)?;
                        if out.key_count().unwrap_or(0) > 0 {
                            return Ok(Some(Entry {
                                key: key.to_string(),
                                kind: Kind::Dir,
                                size: 0,
                                mtime: None,
                                etag: None,
                                version: None,
                            }));
                        }
                    }
                    Ok(None)
                } else {
                    Err(Error::backend(e))
                }
            }
        }
    }

    async fn read(&self, key: &str, range: Range<u64>) -> Result<Bytes> {
        if range.is_empty() {
            return Ok(Bytes::new());
        }
        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .range(format!("bytes={}-{}", range.start, range.end - 1))
            .send()
            .await
            .map_err(Error::backend)?;
        let data = out.body.collect().await.map_err(Error::backend)?;
        Ok(data.into_bytes())
    }

    async fn put(&self, key: &str, data: Bytes, opts: PutOptions) -> Result<Entry> {
        let size = data.len() as u64;
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(data));
        if let Some(m) = mtime_meta(opts.mtime) {
            req = req.metadata(META_MTIME, m);
        }
        if let Some(ct) = &opts.content_type {
            req = req.content_type(ct);
        }
        match opts.if_match.as_deref() {
            Some("") => req = req.if_none_match("*"),
            Some(tag) => req = req.if_match(format!("\"{tag}\"")),
            None => {}
        }
        let out = req
            .send()
            .await
            .map_err(|e| map_conditional(e, key, opts.if_match.clone()))?;
        Ok(Entry {
            key: key.to_string(),
            kind: if key.ends_with('/') {
                Kind::Dir
            } else {
                Kind::File
            },
            size,
            mtime: opts.mtime.or(Some(SystemTime::now())),
            etag: out.e_tag().map(normalize_etag),
            version: out.version_id().map(str::to_string),
        })
    }

    async fn multipart_start(&self, key: &str, opts: PutOptions) -> Result<UploadId> {
        let mut req = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key);
        if let Some(m) = mtime_meta(opts.mtime) {
            req = req.metadata(META_MTIME, m);
        }
        let out = req.send().await.map_err(Error::backend)?;
        out.upload_id()
            .map(|u| UploadId(u.to_string()))
            .ok_or_else(|| Error::Config("no upload id returned".into()))
    }

    async fn multipart_put_part(
        &self,
        key: &str,
        upload: &UploadId,
        part_number: u32,
        data: Bytes,
    ) -> Result<PartEtag> {
        let out = self
            .client
            .upload_part()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(&upload.0)
            .part_number(part_number as i32)
            .body(ByteStream::from(data))
            .send()
            .await
            .map_err(Error::backend)?;
        out.e_tag()
            .map(|e| PartEtag(e.to_string()))
            .ok_or_else(|| Error::Config("no part etag returned".into()))
    }

    async fn multipart_complete(
        &self,
        key: &str,
        upload: &UploadId,
        parts: &[(u32, PartEtag)],
    ) -> Result<Entry> {
        let completed = CompletedMultipartUpload::builder()
            .set_parts(Some(
                parts
                    .iter()
                    .map(|(n, e)| {
                        CompletedPart::builder()
                            .part_number(*n as i32)
                            .e_tag(&e.0)
                            .build()
                    })
                    .collect(),
            ))
            .build();
        let out = self
            .client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(&upload.0)
            .multipart_upload(completed)
            .send()
            .await
            .map_err(Error::backend)?;
        let etag = out.e_tag().map(normalize_etag);
        // 分片完成后 HEAD 一次拿到准确大小与 mtime。
        match self.stat(key).await? {
            Some(mut e) => {
                if e.etag.is_none() {
                    e.etag = etag;
                }
                Ok(e)
            }
            None => Ok(Entry {
                key: key.to_string(),
                kind: Kind::File,
                size: 0,
                mtime: Some(SystemTime::now()),
                etag,
                version: None,
            }),
        }
    }

    async fn multipart_abort(&self, key: &str, upload: &UploadId) -> Result<()> {
        self.client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(&upload.0)
            .send()
            .await
            .map_err(Error::backend)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(Error::backend)?;
        Ok(())
    }

    async fn copy(&self, from: &str, to: &str) -> Result<Entry> {
        let src = self
            .stat(from)
            .await?
            .ok_or_else(|| Error::NotFound(from.to_string()))?;
        let source = copy_source(&self.bucket, from);
        if src.size > self.copy_threshold {
            self.multipart_copy(&source, to, src.size).await?;
        } else {
            self.client
                .copy_object()
                .bucket(&self.bucket)
                .copy_source(source)
                .key(to)
                .send()
                .await
                .map_err(Error::backend)?;
        }
        self.stat(to)
            .await?
            .ok_or_else(|| Error::NotFound(to.to_string()))
    }

    async fn mkdir(&self, key: &str) -> Result<Entry> {
        let key = if key.ends_with('/') {
            key.to_string()
        } else {
            format!("{key}/")
        };
        if self.dir_markers {
            self.put(&key, Bytes::new(), PutOptions::default()).await
        } else {
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
}

fn map_conditional<E>(
    e: aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>,
    key: &str,
    expected: Option<String>,
) -> Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    if let Some(r) = e.raw_response() {
        if r.status().as_u16() == 412 {
            return Error::Conflict {
                key: key.to_string(),
                expected,
                actual: None,
            };
        }
    }
    Error::backend(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_source_encodes_non_ascii_and_spaces() {
        assert_eq!(
            copy_source("b", "a/中 文+x.txt"),
            "b/a/%E4%B8%AD%20%E6%96%87%2Bx.txt"
        );
        assert_eq!(
            source_key(&copy_source("b", "a/中 文+x.txt"), "b"),
            "a/中 文+x.txt"
        );
    }

    #[test]
    fn copy_ranges_cover_object_exactly() {
        let r = copy_ranges(10, 4);
        assert_eq!(r, vec![0..4, 4..8, 8..10]);
        assert_eq!(copy_ranges(8, 4), vec![0..4, 4..8]);
        let big = 6 * 1024 * 1024 * 1024u64 + 7;
        let parts = copy_ranges(big, COPY_PART_SIZE);
        assert_eq!(parts.len(), 13);
        assert_eq!(parts.last().unwrap().end, big);
    }

    #[test]
    fn split_userinfo_extracts_credentials() {
        assert_eq!(
            split_userinfo("http://proxy:8080"),
            ("http://proxy:8080".into(), None)
        );
        assert_eq!(
            split_userinfo("http://u%40x:p%3Aw@proxy:8080"),
            (
                "http://proxy:8080".into(),
                Some(("u@x".into(), "p:w".into()))
            )
        );
    }

    #[test]
    fn proxy_client_builds() {
        assert!(build_http_client(None).unwrap().is_none());
        assert!(build_http_client(Some("env")).unwrap().is_some());
        assert!(build_http_client(Some("http://u:p@127.0.0.1:3128"))
            .unwrap()
            .is_some());
    }
}
