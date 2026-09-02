#!/usr/bin/env bash
# 把仓库自带的 git 钩子接上。装法：`just hooks-install`（幂等，可以反复跑）。
#
# 这个脚本以前把两个薄壳 heredoc 进 `.git/hooks/` 并设 `core.hooksPath=.git/hooks`，
# 那套做法在 linked worktree 里是**静默失效**的：
#   - linked worktree 的 `.git` 是一个**文件**（内容是 `gitdir: …`），不是目录；
#   - `core.hooksPath` 的相对路径按「钩子运行时的工作树顶层」解析，于是在每个
#     linked worktree 里 `.git/hooks` 都解析到一个不存在的路径；
#   - **git 对「钩子目录不存在」不报错也不提示**，直接当没有钩子。
# 结果是 commit-msg 与 pre-commit 在所有 linked worktree 里一次都没跑过，而且没有
# 任何迹象——校验全靠人自觉。现在钩子改成仓库内的 `.githooks/`（进版本控制），
# 每个工作树用自己签出的那一份，主树与 linked worktree 都实测会拦。
#
# 顺带解决了「绝对路径不能进版本控制」和「相对路径在 worktree 里可靠吗」的张力：
# 相对路径可靠，实测过，所以不需要写绝对路径。
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
hooks_dir="$repo_root/.githooks"

if [[ ! -d "$hooks_dir" ]]; then
  echo "install-hooks: 找不到 $hooks_dir。当前签出的提交里没有 .githooks/？" >&2
  exit 1
fi

# 钩子必须带可执行位，否则 git 会跳过它（这一种 git 会 hint 出来，不算静默，
# 但既然签出的文件权限可能被文件系统吃掉，这里主动补上）。
missing=0
for hook in pre-commit commit-msg; do
  if [[ ! -f "$hooks_dir/$hook" ]]; then
    echo "install-hooks: 缺少 .githooks/$hook" >&2
    missing=1
    continue
  fi
  chmod +x "$hooks_dir/$hook"
done
[[ "$missing" -eq 0 ]] || exit 1

# 相对路径，这样每个工作树用自己那一份 .githooks/。
# `--local` 写的是共用的 .git/config，一次安装覆盖所有 worktree。
git config --local core.hooksPath .githooks

echo "install-hooks: core.hooksPath = $(git config --local --get core.hooksPath)"
echo "install-hooks: 已接上 .githooks/pre-commit 与 .githooks/commit-msg"

# 旧装法留下的残留只**报告**、不删除：它们在 .git/ 下，不属于本脚本创建的东西，
# 而且删别人机器上的文件不该由一条安装命令顺手做。它们现在是惰性的（core.hooksPath
# 一旦指向别处，git 就不看 .git/hooks 了；即便有人取消这里的 --local 设置，落回去的
# 也可能是某个全局 hooksPath 而不是 .git/hooks）。真正的危害是误导：读 .git/hooks/
# 的人会以为那才是生效的路径，所以要说一声。
#
# 这里用字符串累加而不是数组：macOS 自带 bash 3.2，在 `set -u` 下展开空数组
# （`${arr[@]}` / `${#arr[@]}`）会直接报 unbound variable。同理不写
# `[[ -f x ]] && stale+=…`：条件为假时整条 `&&` 返回 1，`set -e` 会让脚本当场退出。
stale=""
for hook in pre-commit commit-msg; do
  if [[ -f "$repo_root/.git/hooks/$hook" ]]; then
    stale="${stale}  .git/hooks/${hook}"$'\n'
  fi
done
if [[ -n "$stale" ]]; then
  echo
  echo "install-hooks: 注意，旧装法在 .git/hooks 下留了文件，现在已经不生效了："
  printf '%s' "$stale"
  echo "  它们已经是惰性的，确认后可自行删除；本脚本不替你删 .git/ 下的东西。"
fi
