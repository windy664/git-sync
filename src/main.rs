//! git-sync：PR 模型的 git 双向同步（本地腿）。
//!
//! 本地只做三件事：把编辑提交成待合并分支、推给服务器、取回服务器合完的 master。
//! 本地永远不做 merge / rebase —— 冲突交给服务器解。

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::time::Duration;

use git_sync::config::{self, Config};
use git_sync::git;
use git_sync::schedule;
use git_sync::state::State;
use git_sync::sync::{self, Outcome};

#[derive(Parser, Debug)]
#[command(
    name = "git-sync",
    version,
    about = "PR 模型的 git 双向同步：提交 → 推待合并分支 → 取回服务器合并结果",
    long_about = None
)]
struct Cli {
    /// 配置文件路径（默认查 GIT_SYNC_CONFIG → ~/.config/git-sync/config.toml → ./git-sync.toml）
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 生成配置文件
    Init {
        /// 要同步的仓库目录（默认自动找带 .obsidian 的目录）
        #[arg(long, alias = "vault")]
        repo: Option<PathBuf>,
        /// 远端名
        #[arg(long, default_value = "origin")]
        remote: String,
        /// master 分支名
        #[arg(long, default_value = "master")]
        master: String,
        /// 待合并分支名
        #[arg(long, default_value = "sync/pending")]
        pending: String,
        /// 调度间隔（秒）
        #[arg(long, default_value_t = 120)]
        interval: u64,
        /// 降级通道的 git remote URL（主远端推不上去时用）
        #[arg(long)]
        fallback_remote: Option<String>,
        /// 覆盖已有配置
        #[arg(long)]
        force: bool,
    },
    /// 跑一轮同步
    Run {
        /// 只看会发生什么，不动任何东西
        #[arg(long)]
        dry_run: bool,
    },
    /// 看当前状态
    Status,
    /// 常驻循环跑（自己管调度器时用）
    Daemon {
        /// 循环间隔（秒）
        #[arg(long, default_value_t = 120)]
        interval: u64,
    },
    /// 管理定时调度
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
    },
    /// 打印当前用的配置文件路径
    ConfigPath,
}

#[derive(Subcommand, Debug)]
enum ScheduleAction {
    /// 装调度器
    Install {
        /// 指定后端（cron / systemd / launchd / none），默认按平台自动挑
        #[arg(long)]
        backend: Option<String>,
    },
    /// 卸调度器
    Uninstall,
    /// 看调度器状态
    Status,
}

fn main() {
    reset_sigpipe();
    if let Err(err) = real_main() {
        eprintln!("错误：{err:#}");
        std::process::exit(1);
    }
}

/// 恢复 SIGPIPE 的默认行为：`git-sync status | head` 应该像 `git log | head`
/// 那样安静退出，而不是 panic「failed printing to stdout」。
#[cfg(unix)]
fn reset_sigpipe() {
    // SAFETY：把 SIGPIPE 置回默认处理，与所有标准 Unix 工具一致。
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn reset_sigpipe() {}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init {
            repo,
            remote,
            master,
            pending,
            interval,
            fallback_remote,
            force,
        } => cmd_init(
            cli.config.clone(),
            repo,
            remote,
            master,
            pending,
            interval,
            fallback_remote,
            force,
        ),
        Command::Run { dry_run } => cmd_run(&load_config(cli.config.as_deref())?, dry_run),
        Command::Status => {
            let (cfg, path) = load_config_with_path(cli.config.as_deref())?;
            cmd_status(&cfg, path.as_deref())
        }
        Command::Daemon { interval } => cmd_daemon(&load_config(cli.config.as_deref())?, interval),
        Command::Schedule { action } => cmd_schedule(&load_config(cli.config.as_deref())?, action),
        Command::ConfigPath => {
            println!("{}", config_path_display(cli.config.as_deref()));
            Ok(())
        }
    }
}

/// 找到实际使用的配置文件路径。
fn config_path_display(explicit: Option<&Path>) -> String {
    if let Some(path) = explicit {
        return path.display().to_string();
    }
    if let Ok(env) = std::env::var("GIT_SYNC_CONFIG")
        && !env.is_empty()
    {
        return env;
    }
    if let Some(dir) = dirs::config_dir() {
        let path = dir.join("git-sync").join("config.toml");
        if path.is_file() {
            return path.display().to_string();
        }
    }
    "（未配置）".to_owned()
}

fn load_config(explicit: Option<&Path>) -> Result<Config> {
    Ok(load_config_with_path(explicit)?.0)
}

/// 顺便把「实际用的是哪个文件」带出来，`status` 要报给用户看。
fn load_config_with_path(explicit: Option<&Path>) -> Result<(Config, Option<PathBuf>)> {
    match explicit {
        Some(path) => Ok((Config::load(path)?, Some(path.to_path_buf()))),
        None => Config::discover(),
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_init(
    config_path: Option<PathBuf>,
    repo: Option<PathBuf>,
    remote: String,
    master: String,
    pending: String,
    interval: u64,
    fallback_remote: Option<String>,
    force: bool,
) -> Result<()> {
    let target = config_path.unwrap_or_else(|| {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("git-sync")
            .join("config.toml")
    });
    if target.exists() && !force {
        bail!("配置已存在：{}（要覆盖加 --force）", target.display());
    }

    let repo_path = match repo {
        Some(v) => config::expand_tilde(&v),
        None => detect_repo()?,
    };

    let mut cfg = Config {
        repo_path: repo_path.clone(),
        remote,
        master_branch: master,
        pending_branch: pending,
        schedule: config::Schedule {
            backend: config::ScheduleBackend::Auto,
            interval_secs: interval,
        },
        ..Config::default()
    };
    if let Some(url) = fallback_remote {
        cfg.fallback = config::Fallback {
            remote: url,
            refspec: None,
            enabled: true,
        };
    }

    cfg.save(&target)?;
    println!("已写入配置：{}", target.display());
    println!("  仓库：{}", cfg.repo_path.display());
    println!("  待合并分支：{}", cfg.pending_branch);
    println!("  远端：{} / master={}", cfg.remote, cfg.master_branch);
    if cfg.fallback.enabled {
        println!("  降级通道：{}", cfg.fallback.remote);
    }
    println!();
    println!("下一步：");
    println!("  git-sync run --dry-run    # 看一轮会发生什么");
    println!("  git-sync run              # 真跑一轮");
    println!("  git-sync schedule install # 装定时调度");
    Ok(())
}

/// 猜仓库目录：家目录下的常见位置里找带 `.obsidian` 的。
fn detect_repo() -> Result<PathBuf> {
    let home = dirs::home_dir().context("拿不到 home 目录")?;
    let mut candidates: Vec<PathBuf> = Vec::new();
    for parent in ["Documents", "Desktop", "OneDrive", "笔记", "Notes"] {
        let dir = home.join(parent);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            candidates.push(entry.path());
        }
    }
    candidates.push(home.clone());
    for path in candidates {
        if path.join(".obsidian").is_dir() {
            return Ok(path);
        }
    }
    bail!("没找到 Obsidian vault（家目录下带 .obsidian 的目录）——用 --vault 指定路径")
}

fn cmd_run(cfg: &Config, dry_run: bool) -> Result<()> {
    let outcome = sync::sync_once(cfg, dry_run)?;
    println!("{}：{}", outcome.label(), outcome.message());
    Ok(())
}

fn cmd_status(cfg: &Config, config_path: Option<&Path>) -> Result<()> {
    let state = State::new(&cfg.state_dir);
    println!("配置文件：{}", config_path_display(config_path));
    println!("仓库：{}", cfg.repo_path.display());
    println!("待合并分支：{}", cfg.pending_branch);
    println!("远端：{} / master={}", cfg.remote, cfg.master_branch);
    println!();

    match state.read_status() {
        Some(line) => println!("上次同步：{line}"),
        None => println!("上次同步：还没跑过"),
    }
    println!();

    let git = git::Git::new(&cfg.repo_path);
    if git.is_repo() {
        match git.current_branch() {
            Ok(Some(branch)) => println!("当前分支：{branch}"),
            Ok(None) => println!("当前分支：detached HEAD"),
            Err(_) => println!("当前分支：读不到"),
        }
        if let Ok(status) = git.status_porcelain()
            && !status.is_empty()
        {
            let count = status.lines().count();
            println!("工作区：{count} 处未提交改动");
        } else {
            println!("工作区：干净");
        }
        let head = git.try_rev_parse("HEAD");
        let master = git.try_rev_parse(&cfg.remote_master());
        if let (Some(head), Some(master)) = (head.as_deref(), master.as_deref()) {
            if git.is_ancestor(head, master) {
                println!("与 master：已合并（内容一致）");
            } else if git.is_ancestor(master, head) {
                println!(
                    "与 master：本地领先 {} 个提交（等服务器合并）",
                    count_commits(&git, master, head)
                );
            } else {
                println!("与 master：分叉了——服务器还没合，本地也别乱动");
            }
        }
    } else {
        println!("（{} 还不是 git 仓库）", cfg.repo_path.display());
    }
    println!();

    let backend = schedule::detect(cfg);
    println!("调度器（{}）：", backend.name());
    println!(
        "{}",
        schedule::status(cfg, backend).unwrap_or_else(|e| format!("读不到：{e}"))
    );
    Ok(())
}

fn count_commits(git: &git::Git, from: &str, to: &str) -> usize {
    git.try_run(&["rev-list", "--count", &format!("{from}..{to}")])
        .ok()
        .filter(|o| o.ok)
        .and_then(|o| o.trimmed().parse().ok())
        .unwrap_or(0)
}

fn cmd_daemon(cfg: &Config, interval: u64) -> Result<()> {
    let interval = interval.max(10);
    eprintln!(
        "git-sync daemon：每 {interval}s 跑一轮（Ctrl-C 退出）——状态见 {}",
        State::new(&cfg.state_dir).status_path().display()
    );
    loop {
        match sync::sync_once(cfg, false) {
            Ok(outcome) => eprintln!("{}：{}", outcome.label(), outcome.message()),
            Err(err) => eprintln!("错误：{err:#}"),
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
}

fn cmd_schedule(cfg: &Config, action: ScheduleAction) -> Result<()> {
    match action {
        ScheduleAction::Install { backend } => {
            let backend = match backend.as_deref() {
                Some("cron") => schedule::Backend::Cron,
                Some("systemd") => schedule::Backend::Systemd,
                Some("launchd") => schedule::Backend::Launchd,
                Some("none") => schedule::Backend::None,
                Some(other) => bail!("不认识的调度器：{other}"),
                None => schedule::detect(cfg),
            };
            println!("{}", schedule::install(cfg, backend)?);
        }
        ScheduleAction::Uninstall => {
            let backend = schedule::detect(cfg);
            println!("{}", schedule::uninstall(backend)?);
        }
        ScheduleAction::Status => {
            let backend = schedule::detect(cfg);
            println!("{}", schedule::status(cfg, backend)?);
        }
    }
    Ok(())
}

/// 让 clippy 知道 Outcome 变体都被用到了。
#[allow(dead_code)]
fn _outcome_variants(o: &Outcome) -> bool {
    matches!(
        o,
        Outcome::Synced | Outcome::Pending { .. } | Outcome::Debounced { .. } | Outcome::Skipped(_)
    )
}
