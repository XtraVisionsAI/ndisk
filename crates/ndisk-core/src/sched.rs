//! 任务调度：并发上限、限速、重试。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Semaphore};

use crate::Result;

/// 令牌桶限速器。`rate == 0` 表示不限速。
pub struct RateLimiter {
    rate: AtomicU64,
    state: Mutex<(f64, Instant)>, // (tokens, last_refill)
}

impl RateLimiter {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            rate: AtomicU64::new(bytes_per_sec),
            state: Mutex::new((0.0, Instant::now())),
        }
    }

    pub fn set_rate(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Ordering::Relaxed);
    }

    /// 等待直到允许传输 `n` 字节。
    pub async fn acquire(&self, n: u64) {
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 {
            return;
        }
        loop {
            let wait = {
                let mut st = self.state.lock().await;
                let now = Instant::now();
                let elapsed = now.duration_since(st.1).as_secs_f64();
                let burst = rate as f64; // 允许 1 秒突发
                st.0 = (st.0 + elapsed * rate as f64).min(burst);
                st.1 = now;
                if st.0 >= n as f64 {
                    st.0 -= n as f64;
                    None
                } else {
                    Some(Duration::from_secs_f64((n as f64 - st.0) / rate as f64))
                }
            };
            match wait {
                None => return,
                Some(d) => tokio::time::sleep(d).await,
            }
        }
    }
}

/// 三类并发池：下载、上传、元数据。
#[derive(Clone)]
pub struct Scheduler {
    pub download: Arc<Semaphore>,
    pub upload: Arc<Semaphore>,
    pub meta: Arc<Semaphore>,
    pub download_limit: Arc<RateLimiter>,
    pub upload_limit: Arc<RateLimiter>,
}

impl Scheduler {
    pub fn new(cfg: &crate::config::NetworkConfig) -> Self {
        Self {
            download: Arc::new(Semaphore::new(cfg.download_concurrency.max(1))),
            upload: Arc::new(Semaphore::new(cfg.upload_concurrency.max(1))),
            meta: Arc::new(Semaphore::new(cfg.meta_concurrency.max(1))),
            download_limit: Arc::new(RateLimiter::new(cfg.download_limit)),
            upload_limit: Arc::new(RateLimiter::new(cfg.upload_limit)),
        }
    }
}

/// 指数退避重试。只对 `Error::is_retryable()` 为真的错误重试。
pub async fn retry<F, Fut, T>(max_attempts: u32, mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut delay = Duration::from_millis(200);
    let mut attempt = 0;
    loop {
        attempt += 1;
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < max_attempts && e.is_retryable() => {
                tracing::warn!(attempt, error = %e, "retrying after backoff");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(30));
            }
            Err(e) => return Err(e),
        }
    }
}
