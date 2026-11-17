//! 配置：一份 TOML 描述「同步哪、推到哪、什么时候推」。
//!
//! 查找顺序：命令行 `--config` > 环境变量 `VAULT_SYNC_CONFIG` >
//! `~/.config/vault-sync/config.toml` > 当前目录 `./vault-sync.toml`。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 提交防抖：Obsidian 每次击键都会把文件改脏，Obsidian 那边一次自动保存
/// 就是一次「脏」，不定时就会攒出成百上千个碎片提交。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Debounce {
    /// 两次自动提交之间的最小间隔（秒）。
    pub min_commit_interval_secs: u64,
}

impl Default for Debounce {
    fn default() -> Self {
        Self {
            min_commit_interval_secs: 600,
        }
    }
}

/// 降级通道：主远端推不上去时的第二条路。
///
/// 典型场景：国内 Gitee 配额满了，但内网服务器还能直连——把待合并分支直接
/// 写进服务器的 remote-tracking ref，服务器照样能合。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Fallback {
    /// 另一个 git remote 的 URL（不要求在 .git/config 里登记过）。
    pub remote: String,
    /// 推到哪里。为空时默认 `refs/remotes/<remote>/<pending_branch>`。
    pub refspec: Option<String>,
    pub enabled: bool,
}

/// 调度方式：谁来定时调用 `vault-sync run`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleBackend {
    /// 自动挑（Linux 优先 systemd，回落 cron；macOS 用 launchd）。
    Auto,
    Cron,
    Systemd,
    Launchd,
    /// 不装调度器，自己手动跑或用 `vault-sync daemon`。
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Schedule {
    pub backend: ScheduleBackend,
    /// 调度间隔（秒）。cron 下按分钟取整（向下）。
    pub interval_secs: u64,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            backend: ScheduleBackend::Auto,
            interval_secs: 120,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// vault 仓库所在目录。
    pub vault_path: PathBuf,
    /// 主远端名（git remote 的名字，通常是 origin）。
    pub remote: String,
    /// master 分支名：服务器是它的唯一写者。
    pub master_branch: String,
    /// 待合并分支名：本地编辑推到这里，等服务器 merge 进 master。
    pub pending_branch: String,
    /// 自动提交的提交信息前缀。
    pub commit_message: String,
    pub debounce: Debounce,
    pub fallback: Fallback,
    /// 状态与日志目录。
    pub state_dir: PathBuf,
    pub schedule: Schedule,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            vault_path: default_vault_path(),
            remote: "origin".to_owned(),
            master_branch: "master".to_owned(),
            pending_branch: "sync/pending".to_owned(),
            commit_message: "[local] local edits".to_owned(),
            debounce: Debounce::default(),
            fallback: Fallback::default(),
            state_dir: default_state_dir(),
            schedule: Schedule::default(),
        }
    }
}

fn default_vault_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join("Documents").join("Obsidian Vault"))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn default_state_dir() -> PathBuf {
    dirs::state_dir()
        .map(|d| d.join("vault-sync"))
        .unwrap_or_else(|| PathBuf::from(".vault-sync"))
}

/// 把 `~/foo` 展开成绝对路径。
pub fn expand_tilde(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    path.to_path_buf()
}

impl Config {
    /// 配置文件候选路径（按优先级）。
    pub fn candidate_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Ok(env) = std::env::var("VAULT_SYNC_CONFIG") {
            if !env.is_empty() {
                paths.push(PathBuf::from(env));
            }
        }
        if let Some(dir) = dirs::config_dir() {
            paths.push(dir.join("vault-sync").join("config.toml"));
        }
        paths.push(PathBuf::from("vault-sync.toml"));
        paths
    }

    /// 找到第一个存在的配置文件；都没有就用默认值。
    pub fn discover() -> Result<(Config, Option<PathBuf>)> {
        for path in Self::candidate_paths() {
            if path.is_file() {
                let cfg = Config::load(&path)?;
                return Ok((cfg, Some(path)));
            }
        }
        Ok((Config::default(), None))
    }

    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读不到配置文件 {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text)
            .with_context(|| format!("配置文件 {} 解析失败", path.display()))?;
        cfg.vault_path = expand_tilde(&cfg.vault_path);
        cfg.state_dir = expand_tilde(&cfg.state_dir);
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录 {} 失败", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("序列化配置失败")?;
        std::fs::write(path, text).with_context(|| format!("写配置 {} 失败", path.display()))?;
        Ok(())
    }

    /// 降级通道实际推的 refspec。
    pub fn fallback_refspec(&self) -> String {
        self.fallback
            .refspec
            .clone()
            .unwrap_or_else(|| format!("refs/remotes/{}/{}", self.remote, self.pending_branch))
    }

    /// 远端 master 的引用名。
    pub fn remote_master(&self) -> String {
        format!("{}/{}", self.remote, self.master_branch)
    }
}
