#!/usr/bin/env bash
# scripts/installers/fish/install.sh
# 作用：真正落盘安装 fish 集成；仅使用「远端」payload 来源（一次 curl 同时下载两个文件）
set -Eeuo pipefail

# 避免 locale 警告
unset LC_ALL || true
unset LANG   || true

# ---- 远端源配置 ----
OWNER="xianyudd"
REPO="cdh"
BRANCH="main"
RAW_BASE="https://raw.githubusercontent.com/${OWNER}/${REPO}/${BRANCH}/scripts"

# ---- 目标与标记 ----
FISH_FUNC_DIR="${HOME}/.config/fish/functions"
FISH_CONF_DIR="${HOME}/.config/fish/conf.d"
MARK_BEGIN="# >>> cdh installer >>>"
MARK_END="# <<< cdh installer <<<"

mkdir -p "${FISH_FUNC_DIR}" "${FISH_CONF_DIR}"

# ---- 临时目录（原子写入）----
TMPDIR="$(mktemp -d "${TMPDIR:-/tmp}/cdh-fish.XXXXXX")"
trap 'rm -rf "${TMPDIR}" || true' EXIT

# ---- 一次性下载两个 payload（单进程，多目标）----
# 说明：curl 支持多次 -o 与 URL 配对，单进程可复用连接与 HTTP/2 多路复用
if command -v curl >/dev/null 2>&1; then
  curl -fsSL --retry 1 --connect-timeout 2 \
    -o "${TMPDIR}/cdh.fish"      "${RAW_BASE}/installers/fish/payload/cdh.fish" \
    -o "${TMPDIR}/cdh_log.fish"  "${RAW_BASE}/installers/fish/payload/cdh_log.fish"
elif command -v wget >/dev/null 2>&1; then
  # wget 不支持单进程多输出，只能退化为两次；一般系统都带 curl，极端兜底
  wget -qO "${TMPDIR}/cdh.fish"     "${RAW_BASE}/installers/fish/payload/cdh.fish"
  wget -qO "${TMPDIR}/cdh_log.fish" "${RAW_BASE}/installers/fish/payload/cdh_log.fish"
else
  echo "[cdh] 需要 curl 或 wget 用于下载远端文件。" >&2
  exit 127
fi

# ---- 包 marker + 原子覆盖落盘 ----
# 函数入口
{
  printf "%s\n" "${MARK_BEGIN}"
  cat "${TMPDIR}/cdh.fish"
  printf "%s\n" "${MARK_END}"
} > "${TMPDIR}/cdh.fish.final"
install -m 0644 "${TMPDIR}/cdh.fish.final" "${FISH_FUNC_DIR}/cdh.fish"

# PWD 日志钩子
{
  printf "%s\n" "${MARK_BEGIN}"
  cat "${TMPDIR}/cdh_log.fish"
  printf "%s\n" "${MARK_END}"
} > "${TMPDIR}/cdh_log.fish.final"
install -m 0644 "${TMPDIR}/cdh_log.fish.final" "${FISH_CONF_DIR}/cdh_log.fish"

printf "[cdh] 已安装：\n - %s\n - %s\n" \
  "${FISH_FUNC_DIR}/cdh.fish" "${FISH_CONF_DIR}/cdh_log.fish" >&2
printf "[cdh] 请执行：exec fish -l 以刷新当前会话。\n" >&2
