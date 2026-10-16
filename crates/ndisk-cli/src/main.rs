//! `ndisk` 命令行。Linux 服务器场景的主要入口，也用于各平台的调试。
//!
//! 子命令规划：mount / umount / ls / cat / status / cache pin|unpin|evict / config / auth。
//! 当前骨架实现了 ls、cat、status、cache 三组，mount 在 ndisk-fuse 中实现。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ndisk_core::config::{BackendConfig, MountConfig};
use ndisk_core::{Engine, ItemId, Kind};

#[derive(Parser)]
#[command(
    name = "ndisk",
    version,
    about = "Mount cloud object storage as local files"
)]
struct Cli {
    /// 挂载配置文件（TOML）。
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    /// 缓存根目录，默认 ~/.cache/ndisk/<mount id>。
    #[arg(long, global = true)]
    cache_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 列出目录（相对挂载根的路径）。
    Ls {
        #[arg(default_value = "/")]
        path: String,
        #[arg(long)]
        refresh: bool,
    },
    /// 输出文件内容到 stdout。
    Cat { path: String },
    /// 缓存状态与待上传队列。
    Status,
    /// 缓存控制。
    Cache {
        #[command(subcommand)]
        op: CacheOp,
    },
    /// 打印默认配置模板。
    Config,
}

#[derive(Subcommand)]
enum CacheOp {
    Pin { path: String },
    Unpin { path: String },
    Evict { path: String },
    Download { path: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();

    if let Cmd::Config = cli.cmd {
        println!("{}", template()?.to_toml()?);
        return Ok(());
    }

    let cfg_path = cli.config.context("--config is required")?;
    let cfg = MountConfig::from_toml(&std::fs::read_to_string(&cfg_path)?)?;
    let cache_root = cli.cache_dir.unwrap_or_else(|| default_cache_dir(&cfg.id));
    let engine = open_engine(&cfg, cache_root).await?;

    match cli.cmd {
        Cmd::Ls { path, refresh } => {
            let id = resolve(&engine, &path).await?;
            for n in engine.vfs.list(id, refresh).await? {
                let kind = if n.kind == Kind::Dir { "d" } else { "-" };
                println!(
                    "{kind} {:>12} {:<9} {}",
                    n.size,
                    format!("{:?}", n.local).to_lowercase(),
                    n.name
                );
            }
        }
        Cmd::Cat { path } => {
            let id = resolve(&engine, &path).await?;
            let n = engine.vfs.get(id)?;
            let data = engine.read(id, 0..n.size).await?;
            use std::io::Write;
            std::io::stdout().write_all(&data)?;
        }
        Cmd::Status => {
            println!("mount      : {} ({})", cfg.display_name, cfg.id);
            println!("backend    : {}", engine.vfs.backend().name());
            println!(
                "cached     : {} bytes",
                engine.vfs.meta().total_cached_bytes()?
            );
            let pending = engine.vfs.meta().uploads_pending()?;
            println!("uploads    : {} pending", pending.len());
            for u in pending {
                println!(
                    "  {:?} {} parts_done={}",
                    u.state,
                    u.remote_key,
                    u.parts_done.len()
                );
            }
        }
        Cmd::Cache { op } => match op {
            CacheOp::Pin { path } => engine.pin(resolve(&engine, &path).await?).await?,
            CacheOp::Unpin { path } => engine.unpin(resolve(&engine, &path).await?)?,
            CacheOp::Evict { path } => engine.evict(resolve(&engine, &path).await?).await?,
            CacheOp::Download { path } => engine.download(resolve(&engine, &path).await?).await?,
        },
        Cmd::Config => unreachable!(),
    }
    Ok(())
}

async fn open_engine(cfg: &MountConfig, cache_root: PathBuf) -> Result<Engine> {
    let backend: Arc<dyn ndisk_core::Backend> = match &cfg.backend {
        BackendConfig::S3(s3) => {
            // 骨架阶段：静态密钥从环境变量 NDISK_S3_SECRET 读取；正式版走系统凭据库。
            let secret = std::env::var("NDISK_S3_SECRET").ok();
            Arc::new(
                ndisk_backend_s3::S3Backend::connect(s3, secret, cfg.network.proxy.as_deref())
                    .await?,
            )
        }
        BackendConfig::Opendal { scheme, .. } => {
            anyhow::bail!("backend '{scheme}' not implemented yet")
        }
    };
    Ok(Engine::open(cfg.clone(), backend, cache_root)?)
}

async fn resolve(engine: &Engine, path: &str) -> Result<ItemId> {
    let mut id = ItemId::ROOT;
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        id = engine.vfs.lookup(id, seg).await?.id;
    }
    Ok(id)
}

fn default_cache_dir(mount_id: &str) -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("ndisk").join(mount_id)
}

fn template() -> Result<MountConfig> {
    Ok(MountConfig {
        id: "my-bucket".into(),
        display_name: "My Bucket".into(),
        backend: BackendConfig::S3(ndisk_core::config::S3Config {
            bucket: "my-bucket".into(),
            prefix: String::new(),
            region: Some("us-east-1".into()),
            endpoint: None,
            force_path_style: false,
            auth: ndisk_core::config::S3Auth::Default,
            dir_markers: true,
        }),
        cache: Default::default(),
        network: Default::default(),
        read_only: false,
    })
}
