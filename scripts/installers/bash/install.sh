#!/usr/bin/env bash
# Bash 集成安装器（远端版）
# - 仅负责“集成”：下载 payload → 写入 ~/.config/cdh/bash → 注入 ~/.bashrc
# - 不下载二进制（由顶层 scripts/install.sh 负责）
set -Eeuo pipefail

# 避免 locale 警告
unset LC_ALL || true
unset LANG   || true

OWNER="xianyudd"
REPO="cdh"
BRANCH="main"
RAW_BASE="https://raw.githubusercontent.com/${OWNER}/${REPO}/${BRANCH}/scripts"

# ----- 阶段目录（复用父级 STAGE_DIR；否则自建并清理） -----
if [[ -n "${STAGE_DIR:-}" ]]; then
  CLEAN_STAGE=0
else
  STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/cdh-stage.XXXXXX")"
  CLEAN_STAGE=1
fi
_cleanup() { [[ "${CLEAN_STAGE}" -eq 1 ]] && rm -rf "${STAGE_DIR}" || true; }
trap _cleanup EXIT

# ----- 下载器 -----
_fetch() {
  local url="$1" out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 1 --connect-timeout 4 -o "$out" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$out" "$url"
  else
    echo "[cdh][bash] 需要 curl 或 wget 以下载：$url" >&2
    return 127
  fi
}

# ----- 目录与标记 -----
PAYDIR="$HOME/.config/cdh/bash"
BASHRC="$HOME/.bashrc"
BASH_PROFILE="$HOME/.bash_profile"
MARK_BEGIN="# >>> cdh installer >>>"
MARK_END="# <<< cdh installer <<<"

mkdir -p "$PAYDIR" "$STAGE_DIR"

# 1) 下载 payload 到阶段目录
PAYLOAD_DIR="${STAGE_DIR}/payload"
mkdir -p "$PAYLOAD_DIR"

_fetch "${RAW_BASE}/installers/bash/payload/cdh_log.bash" "${PAYLOAD_DIR}/cdh_log.bash"
_fetch "${RAW_BASE}/installers/bash/payload/cdh.bash"     "${PAYLOAD_DIR}/cdh.bash"

# 2) 安装 payload 到用户目录（0644）
install -m 0644 "${PAYLOAD_DIR}/cdh_log.bash" "$PAYDIR/cdh_log.bash"
install -m 0644 "${PAYLOAD_DIR}/cdh.bash"     "$PAYDIR/cdh.bash"

# 3) 注入 ~/.bashrc（幂等：先去旧块，再写新块）
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
# cdh: bash 集成（按需加载 payload；不改动 set -e/-u）
[ -f "$HOME/.config/cdh/bash/cdh_log.bash" ] && . "$HOME/.config/cdh/bash/cdh_log.bash"
[ -f "$HOME/.config/cdh/bash/cdh.bash" ] && . "$HOME/.config/cdh/bash/cdh.bash"
# <<< cdh installer <<<
MARK

mv "$TMP" "$BASHRC"

# 4) 确保登录 shell 也能加载 .bashrc（如存在 .bash_profile 且尚未包含）
if [ -f "$BASH_PROFILE" ] && ! grep -qE '(^|\s)source\s+~/.bashrc' "$BASH_PROFILE" 2>/dev/null; then
  printf '\n[[ -f ~/.bashrc ]] && source ~/.bashrc\n' >> "$BASH_PROFILE"
fi

echo "[cdh][bash] 已写入："
echo " - $PAYDIR/cdh_log.bash"
echo " - $PAYDIR/cdh.bash"
echo " - 注入 ~/.bashrc 标记块（幂等）"
echo "[cdh][bash] 使其生效：source ~/.bashrc"
