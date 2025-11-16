#!/usr/bin/env bash
set -Eeuo pipefail

CONFIG_DIR="${HOME}/.config/cdh/zsh"
ZSHRC="${HOME}/.zshrc"

BLOCK_START="# >>> cdh zsh integration >>>"
BLOCK_END="# <<< cdh zsh integration <<<"

echo "[cdh][zsh] 开始卸载 zsh 集成..."

if [[ -f "${ZSHRC}" ]]; then
  if grep -q "${BLOCK_START}" "${ZSHRC}"; then
    echo "[cdh][zsh] 从 ${ZSHRC} 中移除 cdh zsh integration 代码块。"
    backup="${ZSHRC}.bak.cdh-zsh-$(date +%s)"
    cp "${ZSHRC}" "${backup}"
    awk -v s="${BLOCK_START}" -v e="${BLOCK_END}" '
      $0 ~ s {flag=1; next}
      $0 ~ e {flag=0; next}
      !flag {print}
    ' "${backup}" > "${ZSHRC}"
    echo "[cdh][zsh] 已备份原始 .zshrc 为：${backup}"
  else
    echo "[cdh][zsh] 在 ${ZSHRC} 中未发现 cdh zsh integration 代码块，跳过。"
  fi
else
  echo "[cdh][zsh] 未找到 ${ZSHRC}，跳过修改。"
fi

if [[ -d "${CONFIG_DIR}" ]]; then
  echo "[cdh][zsh] 删除目录：${CONFIG_DIR}"
  rm -rf "${CONFIG_DIR}"
else
  echo "[cdh][zsh] 未找到 ${CONFIG_DIR}，跳过删除。"
fi

echo "[cdh][zsh] zsh 集成卸载完成。"

