//! 定时调度：把 `git-sync run` 挂到系统自带的调度器上。
//!
//! - Linux：`systemd --user` 定时器（首选，机器睡醒后会补跑），回落 cron
//! - macOS：launchd agent
//! - Windows：只给说明——用任务计划程序跑 `git-sync daemon`

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{Config, ScheduleBackend};

const CRON_MARK: &str = "git-sync";
const SYSTEMD_UNIT: &str = "git-sync";
const LAUNCHD_LABEL: &str = "io.github.windy664.git-sync";

/// 实际使用的调度器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Cron,
    Systemd,
    Launchd,
    None,
}

impl Backend {
    pub fn name(&self) -> &'static str {
        match self {
            Backend::Cron => "cron",
            Backend::Systemd => "systemd",
            Backend::Launchd => "launchd",
            Backend::None => "none",
        }
    }
}

/// 按配置 + 平台挑一个调度器。
pub fn detect(cfg: &Config) -> Backend {
    match cfg.schedule.backend {
        ScheduleBackend::Cron => Backend::Cron,
        ScheduleBackend::Systemd => Backend::Systemd,
        ScheduleBackend::Launchd => Backend::Launchd,
        ScheduleBackend::None => Backend::None,
        ScheduleBackend::Auto => {
            if cfg!(target_os = "macos") {
                Backend::Launchd
            } else if cfg!(target_os = "linux") && systemd_user_available() {
                Backend::Systemd
            } else {
                Backend::Cron
            }
        }
    }
}

fn systemd_user_available() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn binary_path() -> Result<PathBuf> {
    std::env::current_exe().context("拿不到当前可执行文件路径")
}

fn config_path_or_die() -> Result<PathBuf> {
    if let Ok(env) = std::env::var("GIT_SYNC_CONFIG")
        && !env.is_empty()
    {
        return Ok(PathBuf::from(env));
    }
    if let Some(dir) = dirs::config_dir() {
        let path = dir.join("git-sync").join("config.toml");
        if path.is_file() {
            return Ok(path);
        }
    }
    bail!("还没配置文件——先跑 `git-sync init`");
}

fn log_path(cfg: &Config) -> PathBuf {
    cfg.state_dir.join("scheduler.log")
}

// ---------------------------------------------------------------- cron

fn cron_line(cfg: &Config, bin: &Path, cfg_path: &Path) -> String {
    let minutes = (cfg.schedule.interval_secs / 60).max(1);
    format!(
        "*/{minutes} * * * * {} run --config {} >> {} 2>&1  # {CRON_MARK}",
        bin.display(),
        cfg_path.display(),
        log_path(cfg).display()
    )
}

fn read_crontab() -> Vec<String> {
    Command::new("crontab")
        .arg("-l")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
        .lines()
        .map(|l| l.to_owned())
        .collect()
}

fn write_crontab(lines: &[String]) -> Result<()> {
    let body = if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    };
    let mut child = Command::new("crontab")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("启动 crontab 失败")?;
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().context("拿不到 crontab 的 stdin")?;
        stdin
            .write_all(body.as_bytes())
            .context("写 crontab 失败")?;
    }
    let status = child.wait().context("等 crontab 结束失败")?;
    if !status.success() {
        bail!("crontab 写入失败：{status}");
    }
    Ok(())
}

fn cron_install(cfg: &Config) -> Result<String> {
    let bin = binary_path()?;
    let cfg_path = config_path_or_die()?;
    std::fs::create_dir_all(&cfg.state_dir).ok();
    let line = cron_line(cfg, &bin, &cfg_path);
    let mut lines: Vec<String> = read_crontab()
        .into_iter()
        .filter(|l| !l.contains(CRON_MARK))
        .collect();
    lines.push(line.clone());
    write_crontab(&lines)?;
    Ok(format!("已写入 crontab：\n  {line}"))
}

fn cron_uninstall() -> Result<String> {
    let lines: Vec<String> = read_crontab()
        .into_iter()
        .filter(|l| !l.contains(CRON_MARK))
        .collect();
    write_crontab(&lines)?;
    Ok("已从 crontab 移除 git-sync 条目".to_owned())
}

fn cron_status(cfg: &Config) -> Result<String> {
    let found: Vec<String> = read_crontab()
        .into_iter()
        .filter(|l| l.contains(CRON_MARK))
        .collect();
    if found.is_empty() {
        return Ok("cron：未安装".to_owned());
    }
    let expected = cron_line(cfg, &binary_path()?, &config_path_or_die()?);
    let stale = found.iter().any(|l| l.trim() != expected.trim());
    Ok(format!(
        "cron：已安装{}\n  {}",
        if stale {
            "（条目与当前配置不一致，建议重装）"
        } else {
            ""
        },
        found.join("\n  ")
    ))
}

// ------------------------------------------------------------ systemd

fn systemd_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("拿不到用户配置目录")?
        .join("systemd")
        .join("user");
    std::fs::create_dir_all(&dir).with_context(|| format!("创建 {} 失败", dir.display()))?;
    Ok(dir)
}

fn systemd_install(cfg: &Config) -> Result<String> {
    let bin = binary_path()?;
    let cfg_path = config_path_or_die()?;
    let dir = systemd_dir()?;
    let service = format!(
        "[Unit]\nDescription=git-sync - 本地同步腿\n\n[Service]\nType=oneshot\nExecStart={} run --config {}\n",
        bin.display(),
        cfg_path.display()
    );
    let timer = format!(
        "[Unit]\nDescription=定时跑 git-sync\n\n[Timer]\nOnBootSec=2min\nOnUnitActiveSec={}s\nPersistent=true\n\n[Install]\nWantedBy=timers.target\n",
        cfg.schedule.interval_secs.max(60)
    );
    let svc_path = dir.join(format!("{SYSTEMD_UNIT}.service"));
    let tmr_path = dir.join(format!("{SYSTEMD_UNIT}.timer"));
    std::fs::write(&svc_path, service).context("写 service 单元失败")?;
    std::fs::write(&tmr_path, timer).context("写 timer 单元失败")?;
    run_systemctl(&["--user", "daemon-reload"])?;
    run_systemctl(&[
        "--user",
        "enable",
        "--now",
        &format!("{SYSTEMD_UNIT}.timer"),
    ])?;
    Ok(format!(
        "已安装 systemd user 定时器：\n  {}\n  {}",
        svc_path.display(),
        tmr_path.display()
    ))
}

fn systemd_uninstall() -> Result<String> {
    let _ = Command::new("systemctl")
        .args([
            "--user",
            "disable",
            "--now",
            &format!("{SYSTEMD_UNIT}.timer"),
        ])
        .output();
    let dir = systemd_dir().unwrap_or_default();
    let _ = std::fs::remove_file(dir.join(format!("{SYSTEMD_UNIT}.timer")));
    let _ = std::fs::remove_file(dir.join(format!("{SYSTEMD_UNIT}.service")));
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();
    Ok("已卸载 systemd 定时器".to_owned())
}

fn systemd_status() -> Result<String> {
    let out = Command::new("systemctl")
        .args([
            "--user",
            "status",
            &format!("{SYSTEMD_UNIT}.timer"),
            "--no-pager",
        ])
        .output()
        .context("执行 systemctl 失败")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let out = Command::new("systemctl")
        .args(args)
        .output()
        .with_context(|| format!("执行 systemctl {} 失败", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "systemctl {} 失败：{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

// ------------------------------------------------------------ launchd

fn launchd_plist_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("拿不到 home 目录")?;
    let dir = home.join("Library").join("LaunchAgents");
    std::fs::create_dir_all(&dir).with_context(|| format!("创建 {} 失败", dir.display()))?;
    Ok(dir.join(format!("{LAUNCHD_LABEL}.plist")))
}

fn launchd_install(cfg: &Config) -> Result<String> {
    let bin = binary_path()?;
    let bin_display = bin.display().to_string();
    let cfg_path = config_path_or_die()?;
    let cfg_path_display = cfg_path.display().to_string();
    let path = launchd_plist_path()?;
    let log = log_path(cfg);
    let log_display = log.display().to_string();
    std::fs::create_dir_all(&cfg.state_dir).ok();
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{bin_display}</string>
    <string>run</string>
    <string>--config</string>
    <string>{cfg_path_display}</string>
  </array>
  <key>StartInterval</key><integer>{interval}</integer>
  <key>RunAtLoad</key><true/>
  <key>StandardOutPath</key><string>{log_display}</string>
  <key>StandardErrorPath</key><string>{log_display}</string>
</dict>
</plist>
"#,
        interval = cfg.schedule.interval_secs.max(60)
    );
    std::fs::write(&path, plist).context("写 launchd plist 失败")?;
    let _ = Command::new("launchctl")
        .args(["unload", path.to_str().unwrap_or("")])
        .output();
    let out = Command::new("launchctl")
        .args(["load", "-w", path.to_str().unwrap_or("")])
        .output()
        .context("执行 launchctl 失败")?;
    if !out.status.success() {
        bail!(
            "launchctl load 失败：{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(format!("已安装 launchd agent：{}", path.display()))
}

fn launchd_uninstall() -> Result<String> {
    if let Ok(path) = launchd_plist_path() {
        let _ = Command::new("launchctl")
            .args(["unload", path.to_str().unwrap_or("")])
            .output();
        let _ = std::fs::remove_file(&path);
    }
    Ok("已卸载 launchd agent".to_owned())
}

fn launchd_status() -> Result<String> {
    match launchd_plist_path() {
        Ok(path) if path.is_file() => Ok(format!("launchd：已安装（{}）", path.display())),
        _ => Ok("launchd：未安装".to_owned()),
    }
}

// ----------------------------------------------------------------- 门面

pub fn install(cfg: &Config, backend: Backend) -> Result<String> {
    match backend {
        Backend::Cron => cron_install(cfg),
        Backend::Systemd => systemd_install(cfg),
        Backend::Launchd => launchd_install(cfg),
        Backend::None => {
            bail!("schedule.backend = none，不装调度器；用 `git-sync daemon` 常驻跑")
        }
    }
}

pub fn uninstall(backend: Backend) -> Result<String> {
    match backend {
        Backend::Cron => cron_uninstall(),
        Backend::Systemd => systemd_uninstall(),
        Backend::Launchd => launchd_uninstall(),
        Backend::None => Ok("本来就没装".to_owned()),
    }
}

pub fn status(cfg: &Config, backend: Backend) -> Result<String> {
    match backend {
        Backend::Cron => cron_status(cfg),
        Backend::Systemd => systemd_status(),
        Backend::Launchd => launchd_status(),
        Backend::None => Ok("未安装调度器".to_owned()),
    }
}
