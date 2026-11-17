//! 薄薄的 git 命令包装层。
//!
//! 刻意**不**用 libgit2/gix：git CLI 已经把凭据助手、hooks、gc、packed refs、
//! 稀疏检出这些都处理好了，自己实现一遍只会更差。这里的每个方法都只用
//! 稳定输出的命令（`--porcelain`、`--format=%ct` 之类），不解析人类可读文案。

use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Command;

/// 一次 git 调用的结果。失败时 `ok = false`，不直接返回 Err——
/// 有些地方要区分「命令失败」和「答案是否定」。
#[derive(Debug, Clone)]
pub struct GitOut {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

impl GitOut {
    pub fn trimmed(&self) -> &str {
        self.stdout.trim()
    }
}

#[derive(Debug, Clone)]
pub struct Git {
    cwd: PathBuf,
}

impl Git {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.cwd)
            // cron 里没人能回答交互式口令提问：宁可失败也别把进程挂死。
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(args);
        cmd
    }

    /// 跑一条 git 命令，成功返回 stdout（已去尾部空白），失败返回 Err。
    pub fn run(&self, args: &[&str]) -> Result<String> {
        let out = self.try_run(args)?;
        if !out.ok {
            let detail = if out.stderr.trim().is_empty() {
                "无错误输出".to_owned()
            } else {
                out.stderr.trim().to_owned()
            };
            bail!("git {} 失败：{detail}", args.join(" "));
        }
        Ok(out.stdout.trim().to_owned())
    }

    /// 跑一条 git 命令，永不因为非零退出码失败。
    pub fn try_run(&self, args: &[&str]) -> Result<GitOut> {
        let out = self
            .cmd(args)
            .output()
            .with_context(|| format!("执行 git {} 失败", args.join(" ")))?;
        Ok(GitOut {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    pub fn is_repo(&self) -> bool {
        self.try_run(&["rev-parse", "--is-inside-work-tree"])
            .map(|o| o.ok && o.trimmed() == "true")
            .unwrap_or(false)
    }

    /// 当前分支名；处于 detached HEAD 时返回 None。
    pub fn current_branch(&self) -> Result<Option<String>> {
        let name = self.run(&["branch", "--show-current"])?;
        Ok((!name.is_empty()).then_some(name))
    }

    pub fn git_dir(&self) -> Result<PathBuf> {
        Ok(PathBuf::from(
            self.run(&["rev-parse", "--absolute-git-dir"])?,
        ))
    }

    /// 仓库是否停在某个未完成的手工操作上（merge / rebase / cherry-pick）。
    ///
    /// 这时候**绝不能**自动 reset --hard：会把用户的冲突解决现场抹掉。
    pub fn in_progress_op(&self) -> Option<&'static str> {
        let dir = self.git_dir().ok()?;
        if dir.join("MERGE_HEAD").exists() {
            return Some("merge");
        }
        if dir.join("CHERRY_PICK_HEAD").exists() {
            return Some("cherry-pick");
        }
        if dir.join("REVERT_HEAD").exists() {
            return Some("revert");
        }
        if dir.join("rebase-merge").exists() || dir.join("rebase-apply").exists() {
            return Some("rebase");
        }
        None
    }

    /// 工作区是否脏（含未跟踪文件）。
    pub fn is_dirty(&self) -> Result<bool> {
        Ok(!self.status_porcelain()?.is_empty())
    }

    pub fn status_porcelain(&self) -> Result<String> {
        self.run(&["status", "--porcelain", "-uall"])
    }

    pub fn rev_parse(&self, rev: &str) -> Result<String> {
        self.run(&["rev-parse", "--verify", rev])
    }

    pub fn try_rev_parse(&self, rev: &str) -> Option<String> {
        self.try_run(&["rev-parse", "--verify", rev])
            .ok()
            .filter(|o| o.ok)
            .map(|o| o.stdout.trim().to_owned())
    }

    /// a 是否是 b 的祖先（b 包含 a）。
    pub fn is_ancestor(&self, a: &str, b: &str) -> bool {
        self.try_run(&["merge-base", "--is-ancestor", a, b])
            .map(|o| o.ok)
            .unwrap_or(false)
    }

    /// 两个 rev 的树是否一致（内容相同，哪怕提交号不同）。
    pub fn trees_equal(&self, a: &str, b: &str) -> bool {
        match (
            self.try_rev_parse(&format!("{a}^{{tree}}")),
            self.try_rev_parse(&format!("{b}^{{tree}}")),
        ) {
            (Some(ta), Some(tb)) => ta == tb,
            _ => false,
        }
    }

    /// 最近一次提交的时间戳（秒）。空仓库返回 None。
    pub fn last_commit_unix_time(&self) -> Option<i64> {
        let out = self.try_run(&["log", "-1", "--format=%ct"]).ok()?;
        out.trimmed().parse().ok()
    }

    pub fn add_all(&self) -> Result<()> {
        self.run(&["add", "-A"])?;
        Ok(())
    }

    pub fn commit(&self, message: &str) -> Result<()> {
        self.run(&["commit", "-q", "-m", message])?;
        Ok(())
    }

    pub fn push(&self, remote: &str, refspec: &str) -> Result<()> {
        self.run(&["push", "-q", remote, refspec])?;
        Ok(())
    }

    pub fn fetch(&self, remote: &str, branch: &str) -> Result<()> {
        self.run(&["fetch", "-q", remote, branch])?;
        Ok(())
    }

    /// 强制移动本地分支指针（分支不存在则创建）。
    pub fn branch_force(&self, name: &str, start: &str) -> Result<()> {
        self.run(&["branch", "-f", name, start])?;
        Ok(())
    }

    /// 把当前分支硬重置到给定 rev（丢弃工作区与索引的改动）。
    pub fn reset_hard(&self, rev: &str) -> Result<()> {
        self.run(&["reset", "--hard", rev])?;
        Ok(())
    }

    pub fn checkout(&self, name: &str) -> Result<()> {
        self.run(&["checkout", "-q", name])?;
        Ok(())
    }
}
