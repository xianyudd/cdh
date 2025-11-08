#!/usr/bin/env bash
# scripts/installers/fish/install.sh
# 作用：真正落盘安装 fish 集成；仅使用「远端」payload 来源
set -Eeuo pipefail

# 不设置/覆盖 locale，避免 setlocale 警告（若通过管道执行，调用方也可用：env -u LC_ALL -u LANG）
unset LC_ALL || true
unset LANG   || true

# ---- 远端源配置 ----
OWNER="xianyudd"
REPO="cdh"
BRANCH="main"
RAW_BASE="https://raw.githubusercontent.com/${OWNER}/${REPO}/${BRANCH}/scripts"

# ---- 下载器（curl 优先，wget 兜底）----
fetch_stdout() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 --connect-timeout 5 "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- --tries=3 --timeout=5 "$1"
  else
    echo "[cdh] 需要 curl 或 wget 用于下载远端文件。" >&2
    exit 127
  fi
}

# ---- 目标文件与标记 ----
FISH_FUNC_DIR="${HOME}/.config/fish/functions"
FISH_CONF_DIR="${HOME}/.config/fish/conf.d"
MARK_BEGIN="# >>> cdh installer >>>"
MARK_END="# <<< cdh installer <<<"

mkdir -p "${FISH_FUNC_DIR}" "${FISH_CONF_DIR}"

# ---- 原子写入（仅远端）：带 marker，先写临时文件再覆盖 ----
write_remote_with_marker() {
  # $1=dst_path, $2=remote_rel (相对 scripts/)
  local dst="$1" remote_rel="$2"
  local url="${RAW_BASE}/${remote_rel}"
  local tmp; tmp="$(mktemp "${dst}.XXXXXX")"

  {
    printf "%s\n" "${MARK_BEGIN}"
    fetch_stdout "${url}"
    printf "%s\n" "${MARK_END}"
  } > "${tmp}"

  install -m 0644 "${tmp}" "${dst}"
  rm -f "${tmp}"
}

# ---- 执行写入（函数入口 + PWD 日志钩子）----
write_remote_with_marker \
  "${FISH_FUNC_DIR}/cdh.fish" \
  "installers/fish/payload/cdh.fish"

write_remote_with_marker \
  "${FISH_CONF_DIR}/cdh_log.fish" \
  "installers/fish/payload/cdh_log.fish"

# ---- 结果与后续动作提示 ----
printf "[cdh] 已安装：\n - %s\n - %s\n" \
  "${FISH_FUNC_DIR}/cdh.fish" "${FISH_CONF_DIR}/cdh_log.fish" >&2
printf "[cdh] 请执行：exec fish -l 以刷新当前会话。\n" >&2
