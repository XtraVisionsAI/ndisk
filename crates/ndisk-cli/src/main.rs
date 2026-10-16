//! `ndisk` 命令行。Linux 服务器场景的主要入口，也用于各平台的调试。
//!
//! 子命令规划：mount / umount / ls / cat / status / cache pin|unpin|evict / config / auth。
//! mount 子命令在 Linux 上通过 ndisk-fuse 挂载，其他平台返回不支持。

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
    /// 挂载为 FUSE 文件系统（Linux），前台运行直到卸载或 Ctrl-C。
    Mount {
        mountpoint: PathBuf,
        /// 允许其他用户访问（需要 /etc/fuse.conf 开启 user_allow_other）。
        #[arg(long)]
        allow_other: bool,
        /// 后台维护（容量驱逐）间隔秒数。
        #[arg(long, default_value_t = 60)]
        maintain_interval: u64,
    },
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
    let engine = Arc::new(open_engine(&cfg, cache_root).await?);

    match cli.cmd {
        Cmd::Mount {
            mountpoint,
            allow_other,
            maintain_interval,
        } => {
            anyhow::ensure!(
                mountpoint.is_dir(),
                "mountpoint {} is not a directory",
                mountpoint.display()
            );
            // 启动时恢复未完成的上传。
            engine.resume_uploads().await?;
            // 后台维护任务。
            let maint = engine.clone();
            tokio::spawn(async move {
                let mut tick =
                    tokio::time::interval(std::time::Duration::from_secs(maintain_interval.max(5)));
                loop {
                    tick.tick().await;
                    if let Err(e) = maint.maintain().await {
                        tracing::warn!(error = %e, "maintenance failed");
                    }
                }
            });
            tracing::info!(mountpoint = %mountpoint.display(), "mounting");
            // fuser 的回调是同步的，必须在 runtime 之外的线程里跑，回调内用 Handle::block_on。
            let rt = tokio::runtime::Handle::current();
            let eng = engine.clone();
            let opts = ndisk_fuse::MountOptions {
                allow_other,
                auto_unmount: true,
            };
            let mp = mountpoint.clone();
            let join = tokio::task::spawn_blocking(move || ndisk_fuse::mount(eng, rt, &mp, opts));
            join.await??;
            tracing::info!("unmounted");
        }
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
