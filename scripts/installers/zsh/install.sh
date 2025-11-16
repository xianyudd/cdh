#!/usr/bin/env bash
set -Eeuo pipefail

# 允许从顶层继承 OWNER/REPO/BRANCH/RAW_BASE，没有则使用默认
: "${OWNER:=xianyudd}"
: "${REPO:=cdh}"
: "${BRANCH:=main}"
: "${RAW_BASE:=https://raw.githubusercontent.com/${OWNER}/${REPO}/${BRANCH}/scripts}"

_fetch() {
  local url="$1" out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 1 --connect-timeout 4 -o "$out" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$out" "$url"
  else
    echo "[cdh][zsh] 需要 curl 或 wget 以下载：$url" >&2
    return 127
  fi
}

CONFIG_DIR="${HOME}/.config/cdh/zsh"
ZSHRC="${HOME}/.zshrc"

echo "[cdh][zsh] 安装 zsh 集成到：${CONFIG_DIR}"
mkdir -p "${CONFIG_DIR}"

for name in cdh.zsh cdh_log.zsh; do
  url="${RAW_BASE}/installers/zsh/payload/${name}"
  dst="${CONFIG_DIR}/${name}"
  echo "[cdh][zsh] 下载 ${name} ..."
  _fetch "${url}" "${dst}"
done

BLOCK_START="# >>> cdh zsh integration >>>"
BLOCK_END="# <<< cdh zsh integration <<<"
BLOCK_CONTENT=$(cat <<'EOF'
# >>> cdh zsh integration >>>
# cdh zsh support
[ -f "$HOME/.config/cdh/zsh/cdh.zsh" ] && source "$HOME/.config/cdh/zsh/cdh.zsh"
[ -f "$HOME/.config/cdh/zsh/cdh_log.zsh" ] && source "$HOME/.config/cdh/zsh/cdh_log.zsh"
# <<< cdh zsh integration <<<
EOF
)

# 确保 .zshrc 存在
if [[ ! -f "${ZSHRC}" ]]; then
  touch "${ZSHRC}"
fi

# 避免重复插入
if grep -q "${BLOCK_START}" "${ZSHRC}"; then
  echo "[cdh][zsh] 检测到 .zshrc 已存在 cdh zsh integration，跳过追加。"
else
  echo "[cdh][zsh] 向 ${ZSHRC} 追加 cdh zsh integration 代码块。"
  {
    echo ""
    echo "${BLOCK_CONTENT}"
  } >> "${ZSHRC}"
fi

echo "[cdh][zsh] zsh 集成安装完成。请重新打开 zsh，或执行：source \"${ZSHRC}\""

