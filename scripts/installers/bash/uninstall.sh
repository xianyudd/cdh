#!/usr/bin/env bash
# Bash 集成卸载器（远端版）
# - 从 ~/.bashrc 移除带标记片段
# - 删除 payload
# - 尝试在当前会话移除 PROMPT_COMMAND 钩子与函数
set -Eeuo pipefail

unset LC_ALL || true
unset LANG   || true

PAYDIR="$HOME/.config/cdh/bash"
BASHRC="$HOME/.bashrc"

# 1) 移除 ~/.bashrc 标记块
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

# 2) 删除 payload
rm -f "$PAYDIR/cdh.bash" "$PAYDIR/cdh_log.bash" 2>/dev/null || true
rmdir -p "$PAYDIR" 2>/dev/null || true || true

# 3) 当前会话：去除 PROMPT_COMMAND 里的 __cdh_log，并移除函数
unset -f __cdh_log cdh 2>/dev/null || true
if [ -n "${PROMPT_COMMAND:-}" ]; then
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
echo "[cdh][bash] 卸载完成："
echo " - 已清理 ~/.bashrc 中的注入块"
echo " - 已删除 payload：$PAYDIR/cdh.bash, $PAYDIR/cdh_log.bash"
echo " - 让当前会话立即生效："
echo "   打开新 bash：exec bash -l"
