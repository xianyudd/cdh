#!/usr/bin/env bash
set -euo pipefail

msg_file="${1:-}"
if [[ -z "$msg_file" || ! -f "$msg_file" ]]; then
  echo "commit-msg-check: missing commit message file" >&2
  exit 1
fi

subject="$(sed -n '/^[[:space:]]*#/d;/^[[:space:]]*$/d;p;q' "$msg_file" | tr -d '\r')"
if [[ -z "$subject" ]]; then
  echo "提交信息不能为空。" >&2
  exit 1
fi

type_re='feat|fix|docs|refactor|test|chore|ci|perf'
scope_re='install|bash|fish|zsh|history|paths|recommend|controller|readme|release|ci|tui'
pattern="^(${type_re})(\\((${scope_re})\\))?: .+"

if ! printf '%s' "$subject" | grep -Eq "$pattern"; then
  cat >&2 <<'EOF'
提交信息格式不符合约定。

期望格式：
  <type>(<scope>): <summary>
  或
  <type>: <summary>

允许的 type：
  feat, fix, docs, refactor, test, chore, ci, perf

推荐的 scope：
  install, bash, fish, zsh, history, paths, recommend, controller, readme, release, ci, tui

示例：
  feat(history): 在目录访问时维护 uniq 历史
  fix(install): 统一通过 cdh log 记录 shell 历史
  chore(release): 准备 v0.2.0 发布
EOF
  exit 1
fi

if [[ ${#subject} -gt 72 ]]; then
  echo "提交标题过长（>${#subject} 字符）。请尽量控制在 72 字符以内。" >&2
  exit 1
fi

case "$subject" in
  *'.'|*'。'|*'!'|*'！'|*'?'|*'？')
    echo "提交标题末尾不要添加句号或感叹号等标点。" >&2
    exit 1
    ;;
esac

exit 0
