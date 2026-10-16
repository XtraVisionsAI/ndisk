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

pub struct S3Backend {
    client: Client,
    bucket: String,
    dir_markers: bool,
}

impl S3Backend {
    /// 根据配置构建客户端。`secret` 由平台层从系统凭据库取出后传入。
    pub async fn connect(
        cfg: &S3Config,
        secret: Option<String>,
        proxy: Option<&str>,
    ) -> Result<Self> {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
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
                let base = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
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
        if let Some(p) = proxy {
            // TODO: 注入自定义 hyper connector 以支持代理；SDK 默认不读取环境变量代理。
            tracing::warn!(
                proxy = p,
                "proxy configured but not yet wired into the S3 client"
            );
        }
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
        if src.size > COPY_LIMIT {
            // TODO: UploadPartCopy 分片复制
            return Err(Error::Unsupported(
                "copy of objects > 5 GiB not implemented yet",
            ));
        }
        let source = format!("{}/{}", self.bucket, from);
        self.client
            .copy_object()
            .bucket(&self.bucket)
            .copy_source(source)
            .key(to)
            .send()
            .await
            .map_err(Error::backend)?;
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
