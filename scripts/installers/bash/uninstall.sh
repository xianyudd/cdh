#!/usr/bin/env bash
# Bash 集成卸载器（本地版）
# - 从 ~/.bashrc 移除带标记片段
# - 删除 payload
# - 当前会话移除 PROMPT_COMMAND 中的 __cdh_log（若存在）
set -Eeuo pipefail
unset LC_ALL || true
unset LANG   || true

PAYDIR="$HOME/.config/cdh/bash"
BASHRC="$HOME/.bashrc"

# 移除 ~/.bashrc 标记块
if [ -f "$BASHRC" ]; then
  TMP="$(mktemp)"
  awk '
    BEGIN{skip=0}
    /^\s*#\s*>>>\s*cdh installer\s*>>>\s*$/ {skip=1; next}
    /^\s*#\s*<<<\s*cdh installer\s*<<<\s*$/ {skip=0; next}
    skip==0 {print}
  ' "$BASHRC" > "$TMP"
  mv "$TMP" "$BASHRC"
fi

# 删除 payload
rm -f "$PAYDIR/cdh.bash" "$PAYDIR/cdh_log.bash" 2>/dev/null || true
rmdir -p "$PAYDIR" 2>/dev/null || true || true

# 当前会话：去除 PROMPT_COMMAND 里的 __cdh_log
unset -f __cdh_log cdh 2>/dev/null || true
if [ -n "${PROMPT_COMMAND:-}" ]; then
  # 兼容数组/字符串两种形式
  if declare -p PROMPT_COMMAND 2>/dev/null | grep -q 'declare \-a PROMPT_COMMAND='; then
    eval "pc=(\"\${PROMPT_COMMAND[@]}\")"
    new=()
    for h in "${pc[@]}"; do
      [ "$h" = "__cdh_log" ] || new+=("$h")
    done
    if ((${#new[@]})); then
      PROMPT_COMMAND=("${new[@]}")
    else
      unset PROMPT_COMMAND
    fi
  else
    PROMPT_COMMAND="$(printf '%s' "${PROMPT_COMMAND}" \
      | sed -E 's/(^|;)[[:space:]]*__cdh_log[[:space:]]*;?/\1/g; s/;;+;/;/g; s/^;|;$//g')"
    [[ "$PROMPT_COMMAND" == *"__cdh_log"* ]] && unset PROMPT_COMMAND || true
  fi
fi

echo "[cdh][bash] 已卸载：移除 ~/.bashrc 标记块并删除 payload"
echo "[cdh][bash] 如仍在当前 bash 会话，请执行：source ~/.bashrc"
