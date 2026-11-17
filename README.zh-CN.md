# git-sync

> 面向「会合并的机器在局域网里」这种场景的双向 git 同步工具。
> 英文版：[README.md](README.md)

你的 git 远端**本身就是服务器**。`git-sync` 只是同步的客户端那一半——而绝大多数人
用不上它，因为通常只有一个写者。

## 什么时候需要它

常见配置：笔记本、手机、可能还有台台式机，全都往 `main` 推，谁也不打架，用不上这工具。

这工具针对的是另一种配置：**两个写者，其中一个在局域网里**。比如笔记本编辑笔记，
家里的服务器跑 agent 整理同一批笔记。服务器在 `192.168.x.x` 上、没有公网地址，
既推不到它、它也够不着你——但两边都能连同一个 git 远端。

于是远端成了会合点：

```text
        笔记本 ──── push ────┐         ┌──── fetch ──── 笔记本
                             │         │
                    ┌────────▼─────────▼────────┐
                    │      git 远端（服务器）      │
                    └────────┬─────────▲────────┘
                             │         │
                    merge ───┘         └─── push
                             │
                    家里的服务器（仅局域网）
                    负责合并、解冲突、推回 master
```

笔记本**从不合并**。它推一个**待合并分支**，等那台「能合并」的机器把它并进 `master`：

```text
   本地编辑 ──commit──> 待合并分支 ──push──> 远端
                                                 │ merge
   本地编辑 <──────────fetch master────────────┘
```

为什么不干脆在本地合并？因为本地通常没有解冲突的能力——合并器、API key、那个懂你笔记的
AI 都在服务器上。而本地一次走歪的 `git merge` 会把冲突标记**写进你的笔记正文**，
比同步停摆更糟。

所以 `git-sync` 永远不跑 `merge` / `rebase` / `abort`。它只负责提交、推送、拉取。

## 亮点

- **本地永不合并**：文件里不会出现冲突标记，合并交给有能力的机器。
- **不丢东西**：推送失败就报错并保留本地提交；同步途中工作区又变脏，绝不会被 `reset --hard` 覆盖。
- **提交防抖**：编辑器每次击键都保存的话，一天能攒出几百个提交；`git-sync` 会等。
- **降级通道**：主远端拒收（配额、故障）时，可以改走 SSH 直写另一台主机的 refs。
- **底下就是 git**：没有 libgit2、没有自定义对象库、没有常驻守护进程——你的凭据助手、
  hooks、`git` 配置全都照常生效。
- **跨平台**：Linux / macOS / Windows；调度可用 `systemd`、`cron`、`launchd` 或自带 daemon。

## 安装

```bash
cargo install --git https://github.com/windy664/git-sync
```

只依赖 `PATH` 上的 `git`。

## 快速开始

```bash
# 1. 写配置（仓库有 .obsidian 目录时会自动找到）
git-sync init --repo ~/Documents/Obsidian\ Vault

# 2. 看一轮会发生什么（什么都不动）
git-sync run --dry-run

# 3. 真跑一轮
git-sync run

# 4. 每两分钟跑一次
git-sync schedule install

# 5. 看它干了什么
git-sync status
```

## 一轮同步做什么

```text
1. 前置检查
   - 是 git 仓库吗？
   - 仓库停在未完成的 merge / rebase 里吗？ → 拒绝，不碰冲突现场
   - 当前在 master 上吗？ → 拒绝（master 归合并方写）
   - 不在待合并分支上？ → 切过去

2. 提交
   - 工作区干净？ → 没事干
   - 脏但还在防抖窗口内？ → 等（工作区原样不动）
   - 脏且已过窗口 → git add -A && git commit

3. 推待合并分支
   - 已经在 master 里了？ → 不用推
   - 否则 git push HEAD:refs/heads/<pending>
     - 主远端拒收？ → 走降级通道

4. 拉 master
   - git fetch <remote> <master>

5. 对齐
   - 还没合？ → 停在这，下一轮再说
   - 已经合了（本地提交是 master 的祖先，或两者树一致）→ 把待合并分支对齐到 master
```

## 安全保证

| 情况 | 行为 |
|---|---|
| 仓库停在未完成的 merge / rebase / cherry-pick 里 | **拒绝同步**，什么都不动 |
| 对齐（`reset --hard`）前工作区又脏了 | **跳过这一轮**，不覆盖新改动 |
| 推送失败 | **报错**，但本地提交还在，内容不丢 |
| 同步进行到一半用户又改了文件 | 下一轮检测到脏，跳过对齐 |
| 合并方用了 squash | 按树内容比对判定，照样算已合并 |
| 没配 git 身份 | 直接告诉你该敲哪条 `git config` |
| 没凭据 / 断网 | `GIT_TERMINAL_PROMPT=0`，快速失败而不是卡在交互提示上 |

「已合并」有两种判据：本地提交是 `origin/master` 的**祖先**（合并方用了 merge），
或两者**树内容一致**（合并方用了 squash）。

## 配置

默认 `~/.config/git-sync/config.toml`，可用 `--config` 或环境变量 `GIT_SYNC_CONFIG` 覆盖。

```toml
repo_path = "/home/you/Documents/Obsidian Vault"
remote = "origin"
master_branch = "master"          # 只由合并方写
pending_branch = "sync/pending"   # 你的编辑在这里等着被合
commit_message = "[local] local edits"

state_dir = "~/.local/state/git-sync"

[debounce]
# 编辑器每次击键都保存，没有窗口的话一天几百个提交
min_commit_interval_secs = 600

[fallback]
# 主远端推不上去时的第二条路
enabled = false
remote = "ssh://user@host/path/to/repo.git"
refspec = "refs/remotes/origin/sync/pending"

[schedule]
backend = "auto"        # auto | cron | systemd | launchd | none
interval_secs = 120
```

## 另一头

总得有人（或者什么东西）去合并待合并分支。人也行、cron 也行、agent 也行。最小版本是个
shell 脚本：

```bash
#!/usr/bin/env bash
# 把待合并分支并进 master。跑在「有合并能力」的那台机器上，通常是家里的服务器。
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
  # 冲突：交给人、交给 merge tool、或者交给懂你文件的 AI。
  # 千万别在这里 git reset --hard —— 会把冲突现场抹掉。
  echo "merge conflict on $PENDING" >&2
  exit 1
fi
```

冲突那一步接什么都行：`git mergetool`、自己写的脚本、一个大模型。`git-sync` 不关心
怎么合，它只认「`origin/master` 里已经有我的提交」这件事。

## 命令

| 命令 | 作用 |
|---|---|
| `git-sync init` | 生成配置文件 |
| `git-sync run [--dry-run]` | 跑一轮同步 |
| `git-sync status` | 上次结果、分支、工作区、与 master 的差距、调度器状态 |
| `git-sync daemon [--interval N]` | 常驻循环跑 |
| `git-sync schedule install\|uninstall\|status` | 管理定时调度 |
| `git-sync config-path` | 打印当前用的配置文件 |

## 开发

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

测试是真跑 git：每个用例建一个裸仓库当「服务器」、clone 出一个工作仓库，然后断言分支、
工作区状态和文件内容。

## License

MIT，见 [LICENSE](LICENSE)。
