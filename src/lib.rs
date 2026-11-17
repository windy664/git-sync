//! git-sync —— Obsidian vault 双向同步的本地腿。
//!
//! 模型是 PR 模型的本地腿：本地只负责「把编辑交出去」，冲突留给服务器
//! （那边有 AI 语义合并）。详见 `sync` 模块的文档。

pub mod config;
pub mod git;
pub mod schedule;
pub mod state;
pub mod sync;

pub use config::Config;
pub use sync::Outcome;
