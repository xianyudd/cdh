#!/usr/bin/env bash
# scripts/installers/fish/install.sh
# fish 安装器：仅使用「远端」来源；先下载到本地 STAGE_DIR，再从本地写入目标；不保存日志
set -Eeuo pipefail

unset LC_ALL || true
unset LANG   || true

OWNER="xianyudd"
REPO="cdh"
# 发布版本（可用环境变量覆盖：CDH_VERSION=v0.1.1）
VERSION="${CDH_VERSION:-v0.1.1}"
BRANCH="main"
RAW_BASE="https://raw.githubusercontent.com/${OWNER}/${REPO}/${BRANCH}/scripts"
REL_BASE="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}"

# -------- 阶段目录（若父脚本未提供则自建并自行清理） --------
if [[ -n "${STAGE_DIR:-}" ]]; then
  CLEAN_STAGE=0
else
  STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/cdh-stage.XXXXXX")"
  CLEAN_STAGE=1
fi
_cleanup() { [[ "${CLEAN_STAGE}" -eq 1 ]] && rm -rf "${STAGE_DIR}" || true; }
trap _cleanup EXIT

# -------- 下载器 --------
_fetch() {
  local url="$1" out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 1 --connect-timeout 2 -o "$out" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$out" "$url"
  else
    echo "[cdh] 需要 curl 或 wget 用于下载：$url" >&2
    return 127
  fi
}

# -------- 目标位置与标记 --------
FISH_FUNC_DIR="${HOME}/.config/fish/functions"
FISH_CONF_DIR="${HOME}/.config/fish/conf.d"
BIN_DIR="${HOME}/.local/bin"
MARK_BEGIN="# >>> cdh installer >>>"
MARK_END="# <<< cdh installer <<<"

mkdir -p "${FISH_FUNC_DIR}" "${FISH_CONF_DIR}" "${BIN_DIR}" "${STAGE_DIR}"

# -------- 1) 下载 payload 到本地阶段目录 --------
PAYLOAD_DIR="${STAGE_DIR}/payload"
mkdir -p "${PAYLOAD_DIR}"

echo "[cdh] 获取 payload ..."
if command -v curl >/dev/null 2>&1; then
  curl -fsSL --retry 1 --connect-timeout 2 \
    -o "${PAYLOAD_DIR}/cdh.fish"     "${RAW_BASE}/installers/fish/payload/cdh.fish" \
    -o "${PAYLOAD_DIR}/cdh_log.fish" "${RAW_BASE}/installers/fish/payload/cdh_log.fish"
else
  _fetch "${RAW_BASE}/installers/fish/payload/cdh.fish"     "${PAYLOAD_DIR}/cdh.fish"
  _fetch "${RAW_BASE}/installers/fish/payload/cdh_log.fish" "${PAYLOAD_DIR}/cdh_log.fish"
fi

# -------- 2) 若缺少二进制则从 Release 下载并安装 --------
need_bin=1
if command -v cdh >/dev/null 2>&1 || [[ -x "${BIN_DIR}/cdh" ]]; then
  need_bin=0
fi
if [[ "${CDH_SKIP_BIN:-0}" == "1" ]]; then
  need_bin=0
fi

if (( need_bin )); then
  uname_s="$(uname -s || echo Linux)"
  uname_m="$(uname -m || echo x86_64)"
  case "${uname_s}" in
    Linux)  os_triple="unknown-linux-gnu" ;;
    Darwin) os_triple="apple-darwin" ;;  # 预留，将来支持 macOS
    *)      os_triple="unknown-linux-gnu" ;;
  esac
  case "${uname_m}" in
    x86_64|amd64)   arch_triple="x86_64" ;;
    aarch64|arm64)  arch_triple="aarch64" ;;
    *)              arch_triple="x86_64" ;;
  esac

  TARBALL="cdh-${VERSION}-${arch_triple}-${os_triple}.tar.gz"
  TAR_URL="${REL_BASE}/${TARBALL}"
  TAR_PATH="${STAGE_DIR}/${TARBALL}"

  echo "[cdh] 获取二进制：${TAR_URL}"
  _fetch "${TAR_URL}" "${TAR_PATH}"

  UNPACK_DIR="${STAGE_DIR}/unpack"
  mkdir -p "${UNPACK_DIR}"
  tar -xzf "${TAR_PATH}" -C "${UNPACK_DIR}"

  if [[ -x "${UNPACK_DIR}/cdh" ]]; then
    install -m 0755 "${UNPACK_DIR}/cdh" "${BIN_DIR}/cdh"
    echo "[cdh] 已安装二进制到：${BIN_DIR}/cdh"
  else
    echo "[cdh] 警告：未在压缩包中找到可执行文件 cdh，请手动检查：${TAR_PATH}" >&2
  fi
else
  echo "[cdh] 已检测到 cdh 二进制，跳过下载。"
fi

# -------- 3) 从本地 payload 写入目标（带 marker，原子写入）--------
{
  printf "%s\n" "${MARK_BEGIN}"
  cat "${PAYLOAD_DIR}/cdh.fish"
  printf "%s\n" "${MARK_END}"
} > "${STAGE_DIR}/cdh.fish.final"
install -m 0644 "${STAGE_DIR}/cdh.fish.final" "${FISH_FUNC_DIR}/cdh.fish"

{
  printf "%s\n" "${MARK_BEGIN}"
  cat "${PAYLOAD_DIR}/cdh_log.fish"
  printf "%s\n" "${MARK_END}"
} > "${STAGE_DIR}/cdh_log.fish.final"
install -m 0644 "${STAGE_DIR}/cdh_log.fish.final" "${FISH_CONF_DIR}/cdh_log.fish"

echo "[cdh] 已写入："
echo " - ${FISH_FUNC_DIR}/cdh.fish"
echo " - ${FISH_CONF_DIR}/cdh_log.fish"
echo "[cdh] 执行：exec fish -l"
