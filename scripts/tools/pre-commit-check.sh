#!/usr/bin/env bash
set -euo pipefail

# git 跑钩子时会向环境注入一批 GIT_* 变量，普通仓库里至少有 GIT_INDEX_FILE（相对
# .git/index），linked worktree 里还有 GIT_DIR / GIT_INDEX_FILE（都是指向
# .git/worktrees/<name>/ 的绝对路径）。它们顺着 just → cargo 一路传进测试进程，
# 于是测试里自己 `git init` 出来的临时仓库被劫持：read_git_info_reports_clean_and_
# modified_status 会把干净树判成脏、或干脆找不到仓库（实测 GIT_DIR 与 GIT_INDEX_FILE
# **各自单独**注入都能让这条测试红）。后果是 pre-commit 在任何 linked worktree 里必红，
# 提交只能 --no-verify 绕过 —— 校验又被关掉了。所以在起任何检查之前先清掉它们，
# 让子进程里的 git 和人在命令行里手跑一样，从 cwd 的 .git（文件或目录）做仓库发现。
#
# unset 一个未定义的变量不是错误（set -u 不涉及，macOS bash 3.2 相同），所以这一行
# 在「手动直接跑本脚本」和「被钩子调用」两种场景下都成立。
unset GIT_DIR GIT_INDEX_FILE GIT_WORK_TREE GIT_OBJECT_DIRECTORY \
  GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_CONFIG GIT_CONFIG_GLOBAL \
  GIT_CONFIG_SYSTEM GIT_CONFIG_PARAMETERS

REPO_ROOT="$(git rev-parse --show-toplevel 2> /dev/null || pwd)"
cd "$REPO_ROOT"

if command -v just > /dev/null 2>&1; then
  just fmt-check
  just shell-lint
  cargo test --locked --all
else
  # 与 Justfile 的 shellfmt / shellfmt-check / shell-lint 三条配方对齐：.githooks 里的
  # 钩子没有 .sh 后缀（git 按文件名认钩子），所以那条 find 不带 -name 过滤。没 just 的
  # 机器上这两条走不到 .githooks 的话，「检查存在」照样会漏掉钩子自己。
  cargo fmt --check
  shfmt -d -i 2 -ci -bn -sr docs/install.sh $(find scripts -type f -name '*.sh' | sort) $(find .githooks -type f | sort)
  for f in $(find scripts -type f -name '*.sh' | sort) $(find .githooks -type f | sort) docs/install.sh; do
    bash -n "$f"
  done
  cargo test --locked --all
fi
