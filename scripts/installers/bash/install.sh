#!/usr/bin/env bash
# Bash 集成安装器（本地版）
# - 复制 payload 到 ~/.config/cdh/bash/
# - 向 ~/.bashrc 注入带标记的 source 片段（幂等：存在则替换）
set -Eeuo pipefail
unset LC_ALL || true
unset LANG   || true

SCRIPT_DIR="$(cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
SRC_PAYLOAD="${SCRIPT_DIR}/payload"
PAYDIR="$HOME/.config/cdh/bash"
BASHRC="$HOME/.bashrc"
BASH_PROFILE="$HOME/.bash_profile"

mkdir -p "$PAYDIR"

# 复制 payload
install -m 0644 "${SRC_PAYLOAD}/cdh_log.bash" "$PAYDIR/cdh_log.bash"
install -m 0644 "${SRC_PAYLOAD}/cdh.bash"     "$PAYDIR/cdh.bash"

# 注入 ~/.bashrc（带标记，先删旧块再写新块）
touch "$BASHRC"
TMP="$(mktemp)"
awk '
  BEGIN{skip=0}
  /^\s*#\s*>>>\s*cdh installer\s*>>>\s*$/ {skip=1; next}
  /^\s*#\s*<<<\s*cdh installer\s*<<<\s*$/ {skip=0; next}
  skip==0 {print}
' "$BASHRC" > "$TMP"

cat >> "$TMP" <<'MARK'
# >>> cdh installer >>>
# cdh: bash 集成（按需加载 payload；不修改 set -e/-u）
[ -f "$HOME/.config/cdh/bash/cdh_log.bash" ] && . "$HOME/.config/cdh/bash/cdh_log.bash"
[ -f "$HOME/.config/cdh/bash/cdh.bash" ] && . "$HOME/.config/cdh/bash/cdh.bash"
# <<< cdh installer <<<
MARK

mv "$TMP" "$BASHRC"

# 确保登录 shell 也加载 .bashrc
if [ -f "$BASH_PROFILE" ] && ! grep -qE '(^|\s)source\s+~/.bashrc' "$BASH_PROFILE" 2>/dev/null; then
  printf '\n[[ -f ~/.bashrc ]] && source ~/.bashrc\n' >> "$BASH_PROFILE"
fi

echo "[cdh][bash] 已安装："
echo " - $PAYDIR/cdh_log.bash"
echo " - $PAYDIR/cdh.bash"
echo " - 注入 ~/.bashrc 标记块（幂等）"
echo "[cdh][bash] 使其生效：source ~/.bashrc"
