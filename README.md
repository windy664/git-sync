<div align="center">
  <h1><code>git-sync</code></h1>
  <p><sub>Two-way git sync for setups where the machine that can merge lives on a LAN.</sub></p>
  <br>
  <a href="https://github.com/windy664/git-sync/actions/workflows/ci.yml">
    <img src="https://img.shields.io/github/actions/workflow/status/windy664/git-sync/ci.yml?style=flat-square&label=CI" alt="CI">
  </a>
  <a href="https://github.com/windy664/git-sync/blob/main/LICENSE">
    <img src="https://img.shields.io/github/license/windy664/git-sync?style=flat-square" alt="License">
  </a>
  <img src="https://img.shields.io/badge/rust-%3E%3D1.85-0f7666?style=flat-square" alt="Rust version">
  <img src="https://img.shields.io/badge/git-required-8f5a2f?style=flat-square" alt="Requires git">
</div>

---

Your git remote **is** the server. `git-sync` is just the client half of a sync
that most people never need — because the usual setup has one writer.

The usual setup: a laptop, a phone, maybe a desktop. All of them push to
`main`, GitHub merges nothing, nobody fights over a file. Done.

The setup this tool is for: **two writers, one of them behind a LAN.** Say a
laptop that edits notes, and a home server that runs an agent over those same
notes. The server is on `192.168.x.x` with no public address. It can't be
pushed to directly, and it can't reach you either. But both sides *can* reach
a git remote.

So the remote becomes the meeting point:

```text
        laptop ──── push ────┐         ┌──── fetch ──── laptop
                             │         │
                    ┌────────▼─────────▼────────┐
                    │   git remote (the server) │
                    └────────┬─────────▲────────┘
                             │         │
                    merge ───┘         └─── push
                             │
                    home server (LAN only)
                    merges, resolves conflicts,
                    pushes back to master
```

The laptop never merges. It pushes a **pending branch**, and waits for the
machine that *can* merge to fold it into `master`:

```text
   laptop edits ──commit──> pending branch ──push──> remote
                                                       │ merge
   laptop edits <─────────fetch master──────────────────┘
```

Why not just merge locally? Because the laptop usually has no way to resolve
conflicts — the merge helper, the API key, the AI that understands your notes
all live on the server. And a local `git merge` that goes wrong puts conflict
markers *inside your notes*, which is worse than a sync that stalls.

So `git-sync` never runs `merge`, `rebase`, or `abort`. It commits, pushes,
fetches, and gets out of the way.

## Highlights

- **Never merges locally.** No conflict markers in your files, ever. The
  merge-capable machine does the merging.
- **Never loses work.** A failed push reports the error and leaves your commit
  in place. A working tree that changed mid-round is never `reset --hard`ed
  over.
- **Debounced commits.** An editor that saves on every keystroke would
  otherwise produce hundreds of commits a day. `git-sync` waits.
- **Falls back to a second channel.** If the primary remote refuses the push
  (quota, outage), it can write straight into another host's refs over SSH.
- **Plain git underneath.** No libgit2, no custom object store, no daemon.
  Your credential helpers, hooks, and `git` config all keep working.
- **Cross-platform.** Linux, macOS, Windows. Scheduling via `systemd`, `cron`,
  `launchd`, or a built-in daemon.

## Installation

```bash
cargo install --git https://github.com/windy664/git-sync
```

Requires `git` on `PATH`. Nothing else.

## Quick start

```bash
# 1. Write a config (the repo is auto-detected if it has an .obsidian folder)
git-sync init --repo ~/Documents/Obsidian\ Vault

# 2. See what a round would do — touches nothing
git-sync run --dry-run

# 3. Run a round for real
git-sync run

# 4. Keep it running every couple of minutes
git-sync schedule install

# 5. Look at what it has been doing
git-sync status
```

## What one round does

```text
1. Preflight
   - Is the path a git repo?
   - Is the repo parked inside an unfinished merge / rebase?  → refuse, don't
     touch the user's conflict scene
   - Are we on master?  → refuse (master belongs to the merging side)
   - Not on the pending branch?  → switch to it

2. Commit
   - Clean tree?  → nothing to do
   - Dirty, but inside the debounce window?  → wait (worktree untouched)
   - Dirty and past the window?  → git add -A && git commit

3. Push the pending branch
   - Already in master?  → no push needed
   - Otherwise: git push HEAD:refs/heads/<pending>
     - Primary remote refuses?  → try the fallback channel

4. Fetch master
   - git fetch <remote> <master>

5. Align
   - Not merged yet?  → stop here, try again next round
   - Merged (our commit is an ancestor of master, or both trees match)?
     → move the pending branch to master
```

## Safety

| Situation | What happens |
|---|---|
| Repo is inside an unfinished merge / rebase / cherry-pick | **Refuses to sync.** Nothing is touched. |
| Working tree got dirty again right before the reset | **Skips the round.** New edits are never overwritten. |
| Push fails | **Errors out**, but the local commit stays. Nothing is lost. |
| Files change while the round is running | Next round sees the dirty tree and skips the reset. |
| The merging side squashed instead of merged | Detected by tree comparison — still treated as merged. |
| No git identity configured | Tells you the exact `git config` command to run. |
| No credentials / offline | `GIT_TERMINAL_PROMPT=0`: fails fast instead of hanging on a prompt. |

"Merged" is decided two ways: our commit is an **ancestor** of
`origin/master` (the merging side used `merge`), or both trees are
**identical** (the merging side used `squash`).

## Configuration

`~/.config/git-sync/config.toml` by default. Override with `--config` or the
`GIT_SYNC_CONFIG` environment variable.

```toml
repo_path = "/home/you/Documents/Obsidian Vault"
remote = "origin"
master_branch = "master"          # written by the merging side only
pending_branch = "sync/pending"   # where your edits wait to be merged
commit_message = "[local] local edits"

state_dir = "~/.local/state/git-sync"

[debounce]
# Your editor saves on every keystroke. Without a window, that's hundreds of
# commits a day.
min_commit_interval_secs = 600

[fallback]
# A second channel for when the primary remote won't take the push
# (quota exhausted, service down).
enabled = false
remote = "ssh://user@host/path/to/repo.git"
refspec = "refs/remotes/origin/sync/pending"

[schedule]
backend = "auto"        # auto | cron | systemd | launchd | none
interval_secs = 120
```

## The other end

Someone (or something) has to merge the pending branch. It can be a person, a
cron job, or an agent. The minimum viable version is a shell script:

```bash
#!/usr/bin/env bash
# Merge the pending branch into master. Run this wherever the merging
# capability lives — often a home server on the LAN.
set -euo pipefail
REPO=/path/to/repo.git
PENDING=sync/pending
cd "$REPO"

git show-ref --verify --quiet "refs/heads/$PENDING" || exit 0

git checkout -q master
if git merge --no-ff -m "merge $PENDING" "$PENDING"; then
  git push origin master
  git push origin --delete "$PENDING"
else
  # Conflict: hand it to a human, a merge tool, or an AI that understands
  # your files. Never `git reset --hard` here — that erases the conflict.
  echo "merge conflict on $PENDING" >&2
  exit 1
fi
```

Plug whatever you like into that conflict branch — `git mergetool`, a script,
an LLM. `git-sync` doesn't care how the merge happens; it only cares that
`origin/master` ends up containing its commit.

## Commands

| Command | What it does |
|---|---|
| `git-sync init` | Write a config file |
| `git-sync run [--dry-run]` | Run one round |
| `git-sync status` | Last result, branch, worktree state, divergence from master, scheduler state |
| `git-sync daemon [--interval N]` | Loop forever (use when you don't want a system scheduler) |
| `git-sync schedule install\|uninstall\|status` | Manage the timer |
| `git-sync config-path` | Print the config file in use |

## Development

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

The tests run real git: every case builds a bare repo as the "server", clones
a working repo from it, then asserts on branches, worktree state, and file
contents.

## Status and logs

- `~/.local/state/git-sync/status` — the last result, one line
- `~/.local/state/git-sync/log` — rolling log (rotates at 1 MB)
- `~/.local/state/git-sync/scheduler.log` — scheduler stdout/stderr

## License

MIT — see [LICENSE](LICENSE).

中文说明见 [README.zh-CN.md](README.zh-CN.md)。
