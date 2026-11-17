//! 状态与日志：给人和给 cron 看。
//!
//! `status` 文件永远是「最后一句话」，`log` 是滚动日志（超过 1MB 轮转一次）。

use anyhow::{Context, Result};
use std::path::PathBuf;

const LOG_MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct State {
    dir: PathBuf,
}

impl State {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn status_path(&self) -> PathBuf {
        self.dir.join("status")
    }

    pub fn log_path(&self) -> PathBuf {
        self.dir.join("log")
    }

    pub fn ensure_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("创建状态目录 {} 失败", self.dir.display()))
    }

    /// 写最后状态（覆盖）。
    pub fn write_status(&self, line: &str) -> Result<()> {
        self.ensure_dir()?;
        std::fs::write(self.status_path(), format!("{line}\n"))
            .with_context(|| format!("写状态文件失败：{}", self.status_path().display()))
    }

    pub fn read_status(&self) -> Option<String> {
        std::fs::read_to_string(self.status_path())
            .ok()
            .map(|s| s.trim().to_owned())
    }

    /// 追加一行日志，超过上限就轮转。
    pub fn append_log(&self, line: &str) -> Result<()> {
        self.ensure_dir()?;
        let path = self.log_path();
        if let Ok(meta) = std::fs::metadata(&path)
            && meta.len() > LOG_MAX_BYTES
        {
            let _ = std::fs::rename(&path, path.with_extension("log.1"));
        }
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("打开日志 {} 失败", path.display()))?;
        writeln!(file, "{line}").with_context(|| "写日志失败")?;
        Ok(())
    }
}
