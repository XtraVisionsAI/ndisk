//! ndisk 核心引擎。
//!
//! 所有平台（macOS File Provider、Linux FUSE、Windows Cloud Files API）共用本 crate，
//! 平台适配层只负责把系统回调翻译为对 [`Engine`] 的调用，不持有状态。
//!
//! 模块划分见 `docs/DESIGN.md` 第 3 章。

pub mod backend;
pub mod cache;
pub mod config;
pub mod engine;
pub mod error;
pub mod sched;
pub mod types;
pub mod vfs;
pub mod writeback;

pub use backend::Backend;
pub use engine::{Engine, MaintainReport};
pub use error::{Error, Result};
pub use types::*;
