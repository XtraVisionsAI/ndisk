//! 挂载配置。密钥不落在这里，只存系统凭据库中的引用名。

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::types::CachePolicy;
use crate::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountConfig {
    /// 挂载点唯一 id，用作缓存目录名与 File Provider domain id。
    pub id: String,
    pub display_name: String,
    pub backend: BackendConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BackendConfig {
    S3(S3Config),
    /// 预留给 OpenDAL 接入的其他后端。
    Opendal {
        scheme: String,
        #[serde(default)]
        options: std::collections::BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Config {
    pub bucket: String,
    /// 挂载的前缀，空表示整个 bucket。
    #[serde(default)]
    pub prefix: String,
    pub region: Option<String>,
    /// MinIO / OSS / R2 等兼容实现的自定义 endpoint。
    pub endpoint: Option<String>,
    #[serde(default)]
    pub force_path_style: bool,
    #[serde(default)]
    pub auth: S3Auth,
    /// 是否写 `dir/` 零字节标记对象来表示目录。
    #[serde(default = "default_true")]
    pub dir_markers: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum S3Auth {
    /// 使用 aws-config 默认链：环境变量、~/.aws、IMDS、SSO 等。
    #[default]
    Default,
    /// 静态密钥，`secret_ref` 是系统凭据库中的条目名。
    Static {
        access_key_id: String,
        secret_ref: String,
    },
    Profile {
        name: String,
    },
    AssumeRole {
        role_arn: String,
        session_name: Option<String>,
        external_id: Option<String>,
    },
    Sso {
        profile: String,
    },
    WebIdentity {
        role_arn: String,
        token_file: PathBuf,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// 缓存根目录；为空时由平台层给出默认（App Group 容器 / XDG cache）。
    pub root: Option<PathBuf>,
    #[serde(default = "default_block_size")]
    pub block_size: u64,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
    #[serde(default)]
    pub policy: CachePolicy,
    #[serde(default = "default_dir_ttl", with = "secs")]
    pub dir_ttl: Duration,
    #[serde(default = "default_stat_ttl", with = "secs")]
    pub stat_ttl: Duration,
    #[serde(default = "default_multipart_threshold")]
    pub multipart_threshold: u64,
    #[serde(default = "default_part_size")]
    pub part_size: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            root: None,
            block_size: default_block_size(),
            max_bytes: default_max_bytes(),
            policy: CachePolicy::default(),
            dir_ttl: default_dir_ttl(),
            stat_ttl: default_stat_ttl(),
            multipart_threshold: default_multipart_threshold(),
            part_size: default_part_size(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// `None` 直连；`Some("system")` 使用系统代理；否则为 `http://host:port`。
    pub proxy: Option<String>,
    /// 字节/秒，0 表示不限。
    #[serde(default)]
    pub upload_limit: u64,
    #[serde(default)]
    pub download_limit: u64,
    #[serde(default = "default_dl_concurrency")]
    pub download_concurrency: usize,
    #[serde(default = "default_ul_concurrency")]
    pub upload_concurrency: usize,
    #[serde(default = "default_meta_concurrency")]
    pub meta_concurrency: usize,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            proxy: None,
            upload_limit: 0,
            download_limit: 0,
            download_concurrency: default_dl_concurrency(),
            upload_concurrency: default_ul_concurrency(),
            meta_concurrency: default_meta_concurrency(),
        }
    }
}

impl MountConfig {
    pub fn from_toml(s: &str) -> Result<Self> {
        toml::from_str(s).map_err(|e| Error::Config(e.to_string()))
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|e| Error::Config(e.to_string()))
    }

    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty() || self.id.contains(['/', '\\']) {
            return Err(Error::Config(
                "mount id must be non-empty and contain no slashes".into(),
            ));
        }
        if self.cache.block_size == 0 || self.cache.part_size < 5 * 1024 * 1024 {
            return Err(Error::Config(
                "block_size must be >0 and part_size >= 5 MiB".into(),
            ));
        }
        Ok(())
    }
}

fn default_true() -> bool {
    true
}
fn default_block_size() -> u64 {
    8 * 1024 * 1024
}
fn default_max_bytes() -> u64 {
    20 * 1024 * 1024 * 1024
}
fn default_dir_ttl() -> Duration {
    Duration::from_secs(30)
}
fn default_stat_ttl() -> Duration {
    Duration::from_secs(60)
}
fn default_multipart_threshold() -> u64 {
    16 * 1024 * 1024
}
fn default_part_size() -> u64 {
    8 * 1024 * 1024
}
fn default_dl_concurrency() -> usize {
    8
}
fn default_ul_concurrency() -> usize {
    4
}
fn default_meta_concurrency() -> usize {
    16
}

mod secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        d.as_secs().serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_secs)
    }
}
