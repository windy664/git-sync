//! 同步一轮的完整逻辑。
//!
//! 模型是 PR（Pull Request）模型的本地腿：
//!
//! ```text
//!   本地编辑 ──commit──> 待合并分支 ──push──> 服务器
//!                                              │ merge（冲突由服务器解）
//!   本地编辑 <──fetch master──────────────────┘
//! ```
//!
//! 本地**永远不做** merge / rebase / abort：本地没有解冲突的能力，
//! 一旦冲突标记插进笔记正文就很难收场。所以本地只负责「把编辑交出去」，
//! 冲突留给服务器（那边有 AI 语义合并）。
//!
//! 每一步都比 bash 版多一道安全检查：
//! - 仓库停在 merge/rebase 中 → 拒绝，不动用户的冲突现场；
//! - 对齐前工作区又脏了 → 跳过本轮，不 reset --hard 覆盖新改动；
//! - 远端 master 与本地提交内容一致但提交号不同（服务器 squash 过）→ 也算已合并。

use anyhow::{Context, Result, bail};
use chrono::Local;

use crate::config::Config;
use crate::git::Git;
use crate::state::State;

/// 一轮同步的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 本地编辑已被服务器合并，本地已对齐到 master。
    Synced,
    /// 本地编辑已推送，等服务器合并。
    Pending { commit: String },
    /// 工作区是脏的，但还在防抖窗口内，本轮不动它。
    Debounced { age: u64, wait: u64 },
    /// 因为安全检查跳过（没有做任何改动）。
    Skipped(String),
}

impl Outcome {
    pub fn label(&self) -> &'static str {
        match self {
            Outcome::Synced => "已同步",
            Outcome::Pending { .. } => "待合并",
            Outcome::Debounced { .. } => "防抖中",
            Outcome::Skipped(_) => "已跳过",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Outcome::Synced => "本地编辑并入 master，服务器内容已取回".to_owned(),
            Outcome::Pending { commit } => {
                format!("提交 {commit} 已推送，等服务器合并进 master")
            }
            Outcome::Debounced { age, wait } => {
                format!("工作区有改动，{age}s 前刚提交过，{wait}s 后随窗口提交")
            }
            Outcome::Skipped(why) => why.clone(),
        }
    }
}

/// 跑一轮同步，并把结果写进状态文件与日志。
pub fn sync_once(cfg: &Config, dry_run: bool) -> Result<Outcome> {
    let state = State::new(&cfg.state_dir);
    let result = sync_inner(cfg, dry_run);
    let stamp = Local::now().to_rfc3339();
    match &result {
        Ok(outcome) => {
            let _ = state.write_status(&outcome.message());
            let _ = state.append_log(&format!(
                "{stamp} {} {}",
                outcome.label(),
                outcome.message()
            ));
        }
        Err(err) => {
            let _ = state.write_status(&format!("错误：{err:#}"));
            let _ = state.append_log(&format!("{stamp} ERROR {err:#}"));
        }
    }
    result
}

fn sync_inner(cfg: &Config, dry_run: bool) -> Result<Outcome> {
    let git = Git::new(&cfg.vault_path);

    // ---------- 前置检查 ----------
    if !cfg.vault_path.is_dir() {
        bail!("vault 目录不存在：{}", cfg.vault_path.display());
    }
    if !git.is_repo() {
        bail!("{} 不是 git 仓库", cfg.vault_path.display());
    }
    if let Some(op) = git.in_progress_op() {
        bail!("仓库停在 {op} 中（有人正在手工解决冲突），本轮拒绝同步");
    }
    let branch = git
        .current_branch()?
        .ok_or_else(|| anyhow::anyhow!("处于 detached HEAD 状态，拒绝同步"))?;
    if branch == cfg.master_branch {
        bail!("当前就在 master 分支上（master 由服务器唯一写者维护），拒绝同步");
    }
    if branch != cfg.pending_branch {
        // 工具只认待合并分支：别让编辑落到别的分支上去。
        if git.checkout(&cfg.pending_branch).is_err() {
            return Ok(Outcome::Skipped(format!(
                "当前在 {branch} 分支，切不到待合并分支 {}（多半是有未提交改动挡着）",
                cfg.pending_branch
            )));
        }
    }

    // ---------- 脏检查与提交 ----------
    // dry-run 只记录「打算做什么」，不下手；只读的 fetch 照常做，
    // 否则看到的合并状态是上一次的旧账。
    let mut plan: Vec<String> = Vec::new();

    if git.is_dirty()? {
        let last = git.last_commit_unix_time().unwrap_or(0);
        let age = Local::now().timestamp().saturating_sub(last).max(0) as u64;
        if age < cfg.debounce.min_commit_interval_secs {
            return Ok(Outcome::Debounced {
                age,
                wait: cfg.debounce.min_commit_interval_secs - age,
            });
        }
        if dry_run {
            plan.push(format!("提交工作区改动（{age}s 前刚提交过，防抖窗口已过）"));
        } else {
            git.add_all().context("暂存改动失败")?;
            git.commit(&cfg.commit_message).context("提交失败")?;
        }
    }

    let head = git.rev_parse("HEAD")?;
    let short = head.chars().take(8).collect::<String>();

    // ---------- 本地提交是否已经在 master 里 ----------
    let mut merged = match git.try_rev_parse(&cfg.remote_master()) {
        Some(master) => git.is_ancestor(&head, &master) || git.trees_equal(&head, &master),
        None => false,
    };

    // ---------- 推送待合并分支 ----------
    if !merged {
        if dry_run {
            plan.push(format!(
                "推送 {short} 到 {}/{}",
                cfg.remote, cfg.pending_branch
            ));
        } else {
            let refspec = format!("HEAD:refs/heads/{}", cfg.pending_branch);
            match git.push(&cfg.remote, &refspec) {
                Ok(()) => {}
                Err(primary) => {
                    if cfg.fallback.enabled && !cfg.fallback.remote.is_empty() {
                        git.push(
                            &cfg.fallback.remote,
                            &format!("HEAD:{}", cfg.fallback_refspec()),
                        )
                        .with_context(|| {
                            format!(
                                "主远端推送失败（{primary}），降级通道 {} 也失败",
                                cfg.fallback.remote
                            )
                        })?;
                    } else {
                        return Err(primary).context("推送待合并分支失败（本地提交仍在，未丢失）");
                    }
                }
            }
        }
    }

    // ---------- 取回 master ----------
    // fetch 只动 remote-tracking 引用，不碰工作区，dry-run 也照做。
    git.fetch(&cfg.remote, &cfg.master_branch)
        .with_context(|| {
            format!(
                "拉取 {}/{} 失败（网络？）——本地编辑安全保留在待合并分支",
                cfg.remote, cfg.master_branch
            )
        })?;

    // ---------- 重新判断（这次用的是刚拉下来的 master） ----------
    merged = match git.try_rev_parse(&cfg.remote_master()) {
        Some(master) => git.is_ancestor(&head, &master) || git.trees_equal(&head, &master),
        None => false,
    };
    if !merged {
        if dry_run {
            plan.push("服务器还没合并，本轮到此为止".to_owned());
            return Ok(Outcome::Skipped(plan.join("；")));
        }
        return Ok(Outcome::Pending { commit: short });
    }

    // ---------- 对齐到 master ----------
    // 对齐要做 reset --hard：此刻工作区必须还是干净的，否则会覆盖新改动。
    if git.is_dirty()? {
        return Ok(Outcome::Skipped(
            "服务器已合并，但工作区又出现新改动，本轮不对齐（避免 reset --hard 覆盖）".to_owned(),
        ));
    }
    if dry_run {
        plan.push(format!(
            "把待合并分支对齐到 {}（服务器已合并）",
            cfg.remote_master()
        ));
        return Ok(Outcome::Skipped(plan.join("；")));
    }
    let master_ref = cfg.remote_master();
    git.branch_force(&cfg.master_branch, &master_ref)
        .context("更新本地 master 指针失败")?;
    git.reset_hard(&master_ref).context("对齐到 master 失败")?;

    Ok(Outcome::Synced)
}
