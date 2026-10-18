//! `ndisk` 命令行。Linux 服务器场景的主要入口，也用于各平台的调试。
//!
//! 子命令规划：mount / umount / ls / cat / status / cache pin|unpin|evict / config / auth。
//! mount 子命令在 Linux 上通过 ndisk-fuse 挂载，其他平台返回不支持。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ndisk_core::config::{BackendConfig, GlobalSettings, MountConfig};
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
    /// 块缓存目录（可清理）。默认按系统惯例：Linux $XDG_CACHE_HOME/ndisk/<id>，macOS ~/Library/Caches/ndisk/<id>。
    #[arg(long, global = true)]
    cache_dir: Option<PathBuf>,
    /// 全局设置文件（TOML）。默认读取数据目录上级的 settings.toml（存在时）。
    #[arg(long, global = true)]
    settings: Option<PathBuf>,
    /// 持久数据目录（元数据库、未上传文件）。默认 Linux $XDG_DATA_HOME/ndisk/<id>，macOS ~/Library/Application Support/ndisk/<id>。
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
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
    /// 固定为始终缓存（目录递归）。
    Pin { path: String },
    /// 取消固定。
    Unpin { path: String },
    /// 清理本地缓存（目录递归；有未上传修改时拒绝）。
    Evict { path: String },
    /// 立即下载到本地。
    Download { path: String },
    /// 设置缓存策略：`ttl:<秒>`、`always`、`none`、`inherit`。
    Policy { path: String, policy: String },
    /// 立即执行一次维护（TTL 与容量驱逐、固定预取）。
    Gc,
}

fn parse_policy(s: &str) -> Result<Option<ndisk_core::CachePolicy>> {
    use ndisk_core::CachePolicy;
    Ok(match s {
        "inherit" | "default" => None,
        "always" | "pin" => Some(CachePolicy::Always),
        "none" => Some(CachePolicy::None),
        _ => {
            let secs = s
                .strip_prefix("ttl:")
                .and_then(|v| v.parse::<u64>().ok())
                .context("policy must be ttl:<seconds> | always | none | inherit")?;
            Some(CachePolicy::Ttl(std::time::Duration::from_secs(secs)))
        }
    })
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
    let dirs = AppDirs::resolve(&cfg.id, cli.data_dir, cli.cache_dir);
    let settings_path = cli.settings.or_else(|| {
        let p = dirs.data.parent()?.join("settings.toml");
        p.exists().then_some(p)
    });
    let global = match settings_path {
        Some(p) => GlobalSettings::from_toml(&std::fs::read_to_string(&p)?)?,
        None => GlobalSettings::default(),
    };
    let cfg = cfg.apply(&global, 1);
    let engine = Arc::new(open_engine(&cfg, dirs).await?);

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
                    match maint.maintain().await {
                        Ok(r) if r.ttl_evicted + r.capacity_evicted + r.prefetched > 0 => {
                            tracing::info!(
                                ttl_evicted = r.ttl_evicted,
                                capacity_evicted = r.capacity_evicted,
                                freed = r.bytes_freed,
                                prefetched = r.prefetched,
                                cached = r.cached_bytes,
                                "maintenance"
                            )
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(error = %e, "maintenance failed"),
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
                "policy     : {:?} (max {} bytes)",
                cfg.cache().policy,
                cfg.cache().max_bytes
            );
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
            CacheOp::Policy { path, policy } => {
                let id = resolve(&engine, &path).await?;
                engine.set_policy(id, parse_policy(&policy)?).await?;
                println!(
                    "{path}: effective policy {:?}",
                    engine.effective_policy(id)?
                );
            }
            CacheOp::Gc => println!(
                "{}",
                serde_json::to_string_pretty(&engine.maintain().await?)?
            ),
        },
        Cmd::Config => unreachable!(),
    }
    Ok(())
}

async fn open_engine(cfg: &MountConfig, dirs: AppDirs) -> Result<Engine> {
    // 骨架阶段：密钥从环境变量 NDISK_SECRET（兼容旧名 NDISK_S3_SECRET）读取；正式版走系统凭据库。
    let secret = std::env::var("NDISK_SECRET")
        .or_else(|_| std::env::var("NDISK_S3_SECRET"))
        .ok();
    let proxy = cfg.network().proxy;
    let backend: Arc<dyn ndisk_core::Backend> = match &cfg.backend {
        BackendConfig::S3(s3) => {
            Arc::new(ndisk_backend_s3::S3Backend::connect(s3, secret, proxy.as_deref()).await?)
        }
        BackendConfig::Opendal {
            scheme, options, ..
        } => Arc::new(
            ndisk_backend_opendal::OpendalBackend::connect(
                scheme,
                options,
                secret,
                proxy.as_deref(),
            )
            .await?,
        ),
    };
    Ok(Engine::open(cfg.clone(), backend, dirs.data, dirs.cache)?)
}

async fn resolve(engine: &Engine, path: &str) -> Result<ItemId> {
    let mut id = ItemId::ROOT;
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        id = engine.vfs.lookup(id, seg).await?.id;
    }
    Ok(id)
}

/// 按系统惯例确定的目录。
struct AppDirs {
    data: PathBuf,
    cache: PathBuf,
}

impl AppDirs {
    fn resolve(mount_id: &str, data: Option<PathBuf>, cache: Option<PathBuf>) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let (data_base, cache_base) = if cfg!(target_os = "macos") {
            (
                home.join("Library/Application Support"),
                home.join("Library/Caches"),
            )
        } else if cfg!(windows) {
            (
                std::env::var_os("APPDATA")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.clone()),
                std::env::var_os("LOCALAPPDATA")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.clone())
                    .join("cache"),
            )
        } else {
            (
                std::env::var_os("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".local/share")),
                std::env::var_os("XDG_CACHE_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".cache")),
            )
        };
        Self {
            data: data.unwrap_or_else(|| data_base.join("ndisk").join(mount_id)),
            cache: cache.unwrap_or_else(|| cache_base.join("ndisk").join(mount_id)),
        }
    }
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
            copy_threshold: None,
        }),
        read_only: false,
        overrides: Default::default(),
        cache: Some(Default::default()),
        network: Some(Default::default()),
    })
}
