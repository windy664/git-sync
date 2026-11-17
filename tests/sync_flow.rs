//! 端到端同步流程测试：真的建裸仓库当「服务器」，真的 clone 出 vault，
//! 真的跑同步轮次。断言的是外部可观察的结果——分支上有什么、工作区干不干净、
//! 文件还在不在。

use std::path::{Path, PathBuf};
use std::process::Command;

use git_sync::config::Config;
use git_sync::sync::{Outcome, sync_once};
use tempfile::TempDir;

const PENDING: &str = "sync/pending";

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("执行 git {args:?} 失败：{e}"));
    assert!(
        out.status.success(),
        "git {args:?} 失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

struct Fixture {
    tmp: TempDir,
    server: PathBuf,
    vault: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("建临时目录");
        let server = tmp.path().join("server.git");
        let vault = tmp.path().join("vault");
        let state = tmp.path().join("state");

        git(
            tmp.path(),
            &[
                "init",
                "--bare",
                "--initial-branch=master",
                server.to_str().unwrap(),
            ],
        );
        git(
            tmp.path(),
            &["clone", server.to_str().unwrap(), vault.to_str().unwrap()],
        );

        // 仓库内配好身份：工具的提交走仓库/全局配置，不能依赖跑测试的机器
        // 恰好配了 user.name（CI runner 就没有）。
        git(&vault, &["config", "user.name", "test"]);
        git(&vault, &["config", "user.email", "test@example.com"]);

        std::fs::write(vault.join("hello.md"), "# hello\n").unwrap();
        git(&vault, &["add", "-A"]);
        git(&vault, &["commit", "-m", "init"]);
        git(&vault, &["push", "-u", "origin", "master"]);
        // 待合并分支：编辑都落在这里，等服务器合。
        git(&vault, &["checkout", "-b", PENDING]);

        Self {
            tmp,
            server,
            vault,
            state,
        }
    }

    fn cfg(&self) -> Config {
        let mut cfg = Config {
            vault_path: self.vault.clone(),
            remote: "origin".into(),
            master_branch: "master".into(),
            pending_branch: PENDING.into(),
            state_dir: self.state.clone(),
            ..Default::default()
        };
        cfg.debounce.min_commit_interval_secs = 0;
        cfg
    }

    fn write(&self, name: &str, body: &str) {
        std::fs::write(self.vault.join(name), body).unwrap();
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.vault.join(name)).unwrap()
    }

    fn git(&self, args: &[&str]) -> String {
        git(&self.vault, args)
    }

    /// 模拟服务器：把待合并分支 merge 进 master 推回去，然后删掉待合并分支。
    fn server_merges_pending(&self) {
        let work = self.tmp.path().join("server-work");
        git(
            self.tmp.path(),
            &[
                "clone",
                self.server.to_str().unwrap(),
                work.to_str().unwrap(),
            ],
        );
        git(
            &work,
            &[
                "fetch",
                "origin",
                &format!("refs/heads/{PENDING}:refs/heads/incoming"),
            ],
        );
        git(&work, &["checkout", "-q", "master"]);
        git(
            &work,
            &["merge", "--no-ff", "-m", "merge pending", "incoming"],
        );
        git(&work, &["push", "origin", "master"]);
        git(&work, &["push", "origin", "--delete", PENDING]);
    }

    /// 模拟服务器：合并待合并分支后，服务器自己又改了同一份文件（制造冲突用）。
    fn server_merges_pending_with_edit(&self, content: &str) {
        let work = self.tmp.path().join("server-work");
        git(
            self.tmp.path(),
            &[
                "clone",
                self.server.to_str().unwrap(),
                work.to_str().unwrap(),
            ],
        );
        git(
            &work,
            &[
                "fetch",
                "origin",
                &format!("refs/heads/{PENDING}:refs/heads/incoming"),
            ],
        );
        git(&work, &["checkout", "-q", "master"]);
        git(
            &work,
            &["merge", "--no-ff", "-m", "merge pending", "incoming"],
        );
        std::fs::write(work.join("note.md"), content).unwrap();
        git(&work, &["commit", "-am", "server side edit"]);
        git(&work, &["push", "origin", "master"]);
    }

    /// 模拟服务器用 squash 合（内容一样但提交号不同）。
    fn server_squashes_pending(&self) {
        let work = self.tmp.path().join("server-work");
        git(
            self.tmp.path(),
            &[
                "clone",
                self.server.to_str().unwrap(),
                work.to_str().unwrap(),
            ],
        );
        git(
            &work,
            &[
                "fetch",
                "origin",
                &format!("refs/heads/{PENDING}:refs/heads/incoming"),
            ],
        );
        git(&work, &["checkout", "-q", "master"]);
        git(&work, &["merge", "--squash", "incoming"]);
        git(&work, &["commit", "-m", "squashed pending"]);
        git(&work, &["push", "origin", "master"]);
    }

    fn remote_branch_sha(&self, branch: &str) -> Option<String> {
        let out = Command::new("git")
            .current_dir(&self.vault)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["ls-remote", "origin", &format!("refs/heads/{branch}")])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.split_whitespace().next().map(|s| s.to_owned())
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    fn origin_master(&self) -> String {
        self.git(&["rev-parse", "origin/master"])
    }
}

#[test]
fn dirty_tree_is_committed_and_pushed_as_pending() {
    let fx = Fixture::new();
    fx.write("note.md", "本地编辑\n");

    let outcome = sync_once(&fx.cfg(), false).expect("同步应成功");

    assert!(
        matches!(outcome, Outcome::Pending { .. }),
        "期望 Pending，实际 {outcome:?}"
    );
    assert!(
        fx.remote_branch_sha(PENDING).is_some(),
        "待合并分支没推上去"
    );
    assert!(
        fx.git(&["status", "--porcelain"]).is_empty(),
        "工作区应该干净"
    );
    let body = fx.git(&["show", "HEAD:note.md"]);
    assert!(body.contains("本地编辑"), "提交里没有本地内容：{body}");
}

#[test]
fn debounce_window_holds_back_the_commit() {
    let fx = Fixture::new();
    let mut cfg = fx.cfg();
    cfg.debounce.min_commit_interval_secs = 3600;

    fx.write("note.md", "改动一\n");
    sync_once(&cfg, false).expect("第一轮应成功");
    let first = fx.head();

    fx.write("note.md", "改动二\n");
    let outcome = sync_once(&cfg, false).expect("第二轮应成功");

    assert!(
        matches!(outcome, Outcome::Debounced { .. }),
        "期望 Debounced，实际 {outcome:?}"
    );
    assert_eq!(fx.head(), first, "防抖窗口内不该产生新提交");
    assert_eq!(fx.read("note.md"), "改动二\n", "工作区内容必须原样保留");
}

#[test]
fn after_server_merges_local_aligns_to_master() {
    let fx = Fixture::new();
    fx.write("note.md", "本地编辑\n");
    sync_once(&fx.cfg(), false).expect("第一轮应成功");
    assert!(matches!(
        sync_once(&fx.cfg(), false).expect("第二轮应成功"),
        Outcome::Pending { .. }
    ));

    fx.server_merges_pending();

    let outcome = sync_once(&fx.cfg(), false).expect("对齐轮应成功");
    assert!(
        matches!(outcome, Outcome::Synced),
        "期望 Synced，实际 {outcome:?}"
    );
    assert_eq!(fx.head(), fx.origin_master(), "本地应对齐到 origin/master");
    assert!(
        fx.git(&["status", "--porcelain"]).is_empty(),
        "对齐后工作区应干净"
    );
    assert_eq!(fx.read("note.md"), "本地编辑\n", "内容不能丢");
    // 本地 master 指针也跟着走
    assert_eq!(
        fx.git(&["rev-parse", "master"]),
        fx.origin_master(),
        "本地 master 指针应与 origin/master 一致"
    );
}

#[test]
fn squashed_server_commit_still_counts_as_merged() {
    let fx = Fixture::new();
    fx.write("note.md", "本地编辑\n");
    sync_once(&fx.cfg(), false).expect("第一轮应成功");

    fx.server_squashes_pending();

    let outcome = sync_once(&fx.cfg(), false).expect("对齐轮应成功");
    assert!(
        matches!(outcome, Outcome::Synced),
        "服务器 squash 过（内容同、提交号不同）也应算已合并，实际 {outcome:?}"
    );
    assert_eq!(fx.head(), fx.origin_master());
}

#[test]
fn push_failure_keeps_local_commit() {
    let fx = Fixture::new();
    fx.write("note.md", "本地编辑\n");
    // 把远端指向不存在的路径 → 推送必然失败
    fx.git(&["remote", "set-url", "origin", "/tmp/不存在的远端.git"]);

    let result = sync_once(&fx.cfg(), false);

    assert!(result.is_err(), "推送失败应该报错");
    let err = format!("{:#}", result.unwrap_err());
    assert!(err.contains("推送"), "错误信息应说清是推送失败：{err}");
    // 关键：本地提交还在，内容没丢（失败发生在提交之后，所以工作区是干净的）
    assert!(
        fx.git(&["status", "--porcelain"]).is_empty(),
        "工作区应干净"
    );
    let body = fx.git(&["show", "HEAD:note.md"]);
    assert!(body.contains("本地编辑"), "提交丢了：{body}");
}

#[test]
fn fallback_remote_is_used_when_primary_fails() {
    let fx = Fixture::new();
    let fallback = fx.tmp.path().join("fallback.git");
    git(
        fx.tmp.path(),
        &[
            "init",
            "--bare",
            "--initial-branch=master",
            fallback.to_str().unwrap(),
        ],
    );

    let mut cfg = fx.cfg();
    cfg.fallback = git_sync::config::Fallback {
        remote: fallback.to_string_lossy().into_owned(),
        refspec: None,
        enabled: true,
    };
    fx.git(&["remote", "set-url", "origin", "/tmp/不存在的远端.git"]);
    fx.write("note.md", "本地编辑\n");

    let result = sync_once(&cfg, false);

    // 主远端整个坏了 → 这一轮最终会因 fetch 失败而报错；
    // 但降级推送必须已经成功，否则本地编辑就两头落空。
    assert!(result.is_err(), "主远端坏了，fetch 应该失败：{result:?}");
    // 降级通道的目标 ref 上确实有东西
    let out = Command::new("git")
        .current_dir(&fx.vault)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "ls-remote",
            fallback.to_str().unwrap(),
            &format!("refs/remotes/origin/{PENDING}"),
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(!text.trim().is_empty(), "降级通道没写成：{text}");
}

#[test]
fn new_local_edits_survive_the_alignment_round() {
    let fx = Fixture::new();
    fx.write("note.md", "第一版\n");
    sync_once(&fx.cfg(), false).expect("第一轮应成功");
    fx.server_merges_pending();

    // 服务器合了，本地又有新改动 → 不该被 reset --hard 覆盖
    fx.write("note.md", "第二版\n");
    let outcome = sync_once(&fx.cfg(), false).expect("对齐轮应成功");

    assert_eq!(fx.read("note.md"), "第二版\n", "本地新改动被覆盖了！");
    assert!(
        matches!(outcome, Outcome::Pending { .. }),
        "第二版是新提交，服务器还没合，实际 {outcome:?}"
    );
}

#[test]
fn dry_run_changes_nothing() {
    let fx = Fixture::new();
    fx.write("note.md", "本地编辑\n");
    let before = fx.head();

    let outcome = sync_once(&fx.cfg(), true).expect("dry-run 不该报错");

    assert!(
        matches!(outcome, Outcome::Skipped(_)),
        "dry-run 应该只报告不执行，实际 {outcome:?}"
    );
    assert_eq!(fx.head(), before, "dry-run 不该产生提交");
    assert!(fx.remote_branch_sha(PENDING).is_none(), "dry-run 不该推送");
    assert!(
        !fx.git(&["status", "--porcelain"]).is_empty(),
        "工作区应保持脏"
    );
}

#[test]
fn refuses_to_run_on_master_branch() {
    let fx = Fixture::new();
    fx.git(&["checkout", "-q", "master"]);

    let result = sync_once(&fx.cfg(), false);

    assert!(result.is_err(), "在 master 上跑应该拒绝");
    let err = format!("{:#}", result.unwrap_err());
    assert!(err.contains("master"), "错误信息应提到 master：{err}");
}

#[test]
fn refuses_to_run_inside_an_unfinished_merge() {
    let fx = Fixture::new();
    // 造一个真实的冲突现场：本地改了 note.md，服务器也改了同一份文件
    fx.write("note.md", "本地版\n");
    fx.git(&["add", "-A"]);
    fx.git(&["commit", "-m", "local"]);
    fx.git(&["push", "origin", &format!("HEAD:refs/heads/{PENDING}")]);
    fx.server_merges_pending_with_edit("服务器版\n");

    fx.write("note.md", "远端版\n");
    fx.git(&["commit", "-am", "remote side"]);
    fx.git(&["fetch", "origin", "master"]);

    let merge = Command::new("git")
        .current_dir(&fx.vault)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["merge", "origin/master"])
        .output()
        .unwrap();
    assert!(!merge.status.success(), "应该制造出一个冲突");
    assert!(
        fx.vault.join(".git").join("MERGE_HEAD").exists(),
        "冲突现场没留下 MERGE_HEAD"
    );

    let result = sync_once(&fx.cfg(), false);

    assert!(result.is_err(), "有未完成 merge 时应该拒绝同步");
    let err = format!("{:#}", result.unwrap_err());
    assert!(err.contains("merge"), "错误信息应提到 merge：{err}");
}

#[test]
fn edits_appearing_mid_round_are_not_overwritten() {
    let fx = Fixture::new();
    fx.write("note.md", "本地版\n");
    sync_once(&fx.cfg(), false).expect("第一轮应成功");
    fx.server_merges_pending();

    // 装一个钩子：fetch 更新引用时往 vault 里塞一个新文件——
    // 模拟「同步进行到一半，用户又改了东西」。
    let injected = fx.vault.join("中途出现.md");
    let hook = fx.vault.join(".git/hooks/reference-transaction");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(
        &hook,
        format!("#!/bin/sh\nprintf '中途\n' > '{}'\n", injected.display()),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&hook).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&hook, perm).unwrap();
    }

    let outcome = sync_once(&fx.cfg(), false).expect("这一轮应成功");

    assert!(
        injected.exists(),
        "同步途中出现的新改动被覆盖了（outcome={outcome:?}）"
    );
    assert!(
        matches!(outcome, Outcome::Skipped(_)),
        "工作区中途变脏时应该跳过对齐，实际 {outcome:?}"
    );
}

#[test]
fn missing_git_identity_gives_a_helpful_error() {
    let fx = Fixture::new();
    // 把仓库里的身份清空（本地配置优先级高于全局，所以任何机器上都是空的）
    fx.git(&["config", "user.name", ""]);
    fx.git(&["config", "user.email", ""]);
    fx.write("note.md", "本地编辑\n");

    let result = sync_once(&fx.cfg(), false);

    assert!(result.is_err(), "没配身份时提交应该失败并说清楚怎么修");
    let err = format!("{:#}", result.unwrap_err());
    assert!(
        err.contains("user.name") && err.contains("git config"),
        "错误信息应告诉用户怎么配身份：{err}"
    );
}
