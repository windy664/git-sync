# git-sync

**PR 模型的 git 双向同步（本地腿）**：把本地编辑安全地交给服务器合并，再把服务器合完的结果取回来。

它解决的是一个很具体的问题：**同一份仓库有两个写者（笔记本和服务器），而两边都会改同一批文件。**

直接共用一个分支必然打架：本地推 master 被拒，rebase 会把冲突标记插进文件正文——而本地通常没有解冲突的能力（密钥、模型、AI 合并器都在服务器上），冲突一旦落地就很难收场。

git-sync 的做法是借用 **PR 模型**（同构于 GitHub PR）：

```text
   本地编辑 ──commit──> 待合并分支 ──push──> 服务器
                                              │ merge（冲突由服务器解）
   本地编辑 <──────────fetch master──────────┘
```

关键点：**本地永远不做 merge / rebase / abort。** 本地只负责「把编辑交出去」，
冲突留给服务器（那边可以接 AI 语义合并）。所以本地文件里永远不会出现冲突标记，
同步也不会因为撞车而停摆。

最常见的用法是拿它同步 [Obsidian](https://obsidian.md/) 笔记库——笔记在手机上改、
在服务器上被 agent 整理，两边都是写者。本仓库的作者就是这么用的：
笔记本每两分钟推一次，宿舍服务器上的 AI 把待合并分支合进 master，冲突由 AI 解。

## 为什么是 Rust

这个工具原本是一个 4KB 的 bash 脚本。重写后的收益：

- **版本控制**：脚本进 git，换机/重装不再靠玄学。
- **更安全**：bash 版会 `reset --hard`；Rust 版在动手前会检查工作区是否又脏了、
  仓库是否停在未完成的 merge 里，条件不满足就跳过这一轮而不是覆盖。
- **配置化**：路径、远端、分支名、降级通道全在一份 TOML 里，不再硬编码。
- **跨平台**：bash + cron 只在 Unix 上能用；这个工具 Linux/macOS/Windows 都能跑
  （Windows 用 `git-sync daemon` 或任务计划程序）。

## 安装

```bash
cargo install --path .          # 从源码装
# 或直接下载发行版二进制（见 Releases）
```

要求：`git` 在 PATH 上，其余无依赖。

## 快速开始

```bash
# 1. 生成配置（vault 目录会自动找带 .obsidian 的那个）
git-sync init --vault ~/Documents/Obsidian\ Vault --pending sync/pending

# 2. 看一轮会发生什么（不动任何东西）
git-sync run --dry-run

# 3. 真跑一轮
git-sync run

# 4. 装定时调度（Linux: systemd user timer 或 cron；macOS: launchd）
git-sync schedule install

# 5. 看状态
git-sync status
```

## 配置

默认位置 `~/.config/git-sync/config.toml`（也可以用 `--config` 或环境变量
`GIT_SYNC_CONFIG` 指定）：

```toml
vault_path = "/home/you/Documents/Obsidian Vault"   # 要同步的仓库
remote = "origin"
master_branch = "master"          # 服务器是 master 的唯一写者
pending_branch = "sync/pending"   # 本地编辑推到这里，等服务器合
commit_message = "[local] local edits"

state_dir = "~/.local/state/git-sync"

[debounce]
# 编辑器每次击键都会弄脏文件：不设防抖的话，一昼夜能攒出上百个碎片提交
min_commit_interval_secs = 600

[fallback]
# 主远端推不上去时的降级通道：直写另一台机器的 remote-tracking ref。
# 典型场景：远端配额满了，但内网服务器还能直连。
enabled = false
remote = "ssh://user@host/path/to/repo.git"
refspec = "refs/remotes/origin/sync/pending"

[schedule]
backend = "auto"        # auto | cron | systemd | launchd | none
interval_secs = 120
```

## 一轮同步做了什么

```text
1. 前置检查
   - 仓库目录在吗？是 git 仓库吗？
   - 仓库停在未完成的 merge/rebase 里吗？→ 拒绝（别动用户的冲突现场）
   - 当前在 master 上吗？→ 拒绝（master 由服务器唯一写者维护）
   - 不在待合并分支上？→ 切过去

2. 提交
   - 工作区干净？→ 跳过
   - 脏但还在防抖窗口内？→ 跳过（工作区原样保留）
   - 脏且过窗口 → git add -A && git commit

3. 推送待合并分支
   - 本地提交已经在 master 里了？→ 不用推
   - 否则 push HEAD:refs/heads/<pending_branch>
     - 主远端失败 → 走降级通道

4. 取回 master
   - git fetch <remote> <master>

5. 对齐
   - 服务器还没合？→ 停在这里，等下一轮
   - 已经合了（本地提交是 origin/master 的祖先，或两者内容一致）→
     把待合并分支对齐到 origin/master
```

## 安全保证

这个工具会动你的工作区，所以几条硬保证：

| 情况 | 行为 |
|---|---|
| 仓库停在未完成的 merge / rebase / cherry-pick 里 | **拒绝同步**，不碰任何东西 |
| 对齐（`reset --hard`）前工作区又变脏了 | **跳过这一轮**，不覆盖新改动 |
| 推送失败 | **报错但本地提交还在**，内容不丢 |
| 同步进行到一半用户又改了文件 | 下一轮检测到脏，跳过对齐 |
| 服务器用 squash 合的（内容一样但提交号不同） | 也算已合并，照样对齐 |
| 机器上没有 `git` 凭据助手 / 断网 | `GIT_TERMINAL_PROMPT=0`，直接失败退出，不会挂死等输入 |

「服务器合了」的判定有两种：本地提交是 `origin/master` 的**祖先**（服务器用 merge，
保留提交号），或两者**树内容一致**（服务器 squash 过）。

## 另一半：服务器端

本地只管交出去，合并要服务器自己做。一个最小可用的服务器端（bash 版）：

```bash
#!/usr/bin/env bash
# 服务器：定期把待合并分支 merge 进 master
set -euo pipefail
REPO=/path/to/repo.git
PENDING=sync/pending
cd "$REPO"

# 有待合并分支才动
if ! git show-ref --verify --quiet "refs/heads/$PENDING"; then
  exit 0
fi

git checkout -q master
if git merge --no-ff -m "merge $PENDING" "$PENDING"; then
  git push origin master
  git push origin --delete "$PENDING"     # 合完删掉待合并分支
else
  # 冲突：交给 AI 语义合并，或者通知人工处理。
  # 千万别在这里 reset --hard —— 会把冲突现场抹掉。
  echo "merge conflict on $PENDING" >&2
  exit 1
fi
```

真要处理冲突，就在这里接你的合并器（AI 语义合并、`git mergetool`、或干脆通知人工）。
本工具不关心服务器怎么合，它只认「`origin/master` 里已经包含了我的提交」这件事。

## 命令

| 命令 | 作用 |
|---|---|
| `git-sync init` | 生成配置文件 |
| `git-sync run [--dry-run]` | 跑一轮同步 |
| `git-sync status` | 看当前状态：上次同步结果、分支、工作区、与 master 的关系、调度器 |
| `git-sync daemon [--interval N]` | 常驻循环跑（自己管调度器时用） |
| `git-sync schedule install\|uninstall\|status` | 管理定时调度 |
| `git-sync config-path` | 打印当前用的配置文件路径 |

## 开发

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

测试是真跑 git：每个用例都建一个裸仓库当「服务器」、clone 出一个工作仓库、
然后断言分支上有什么、工作区干不干净、文件还在不在。

## 状态与日志

- `~/.local/state/git-sync/status` —— 最后一句话（给人和脚本看）
- `~/.local/state/git-sync/log` —— 滚动日志（超过 1MB 轮转）
- `~/.local/state/git-sync/scheduler.log` —— 调度器的 stdout/stderr

## License

MIT
