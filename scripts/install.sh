#!/usr/bin/env bash
set -Eeuo pipefail

OWNER="xianyudd"
REPO="cdh"
BRANCH="main"
RAW_BASE="https://raw.githubusercontent.com/${OWNER}/${REPO}/${BRANCH}/scripts"

usage() {
  echo "Usage: $(basename "$0") [--shell fish|zsh|bash] [--action install|uninstall|selfcheck]"
  echo "No args -> 交互式选择 shell（默认使用当前 shell）并执行 install。"
}

detect_shell() {
  case "${SHELL:-}" in
    *fish*) echo fish ;;
    *zsh*)  echo zsh  ;;
    *bash*) echo bash ;;
    *)      echo fish ;;
  esac
}

choose_shell() {
  local def; def="$(detect_shell)"
  echo "[cdh] 检测到当前 shell: ${def}"
  echo "[cdh] 请选择要安装的 shell："
  echo "  1) fish"
  echo "  2) zsh  （暂未提供安装脚本）"
  echo "  3) bash （暂未提供安装脚本）"
  echo "  q) 退出"
  printf "[cdh] 输入选择（回车默认 %s）: " "${def}"
  # 优先从 /dev/tty 读，避免被管道占用
  if [ -t 0 ]; then read -r ans; else read -r ans < /dev/tty || true; fi
  case "${ans:-}" in
    1|fish) echo fish ;;
    2|zsh)  echo zsh  ;;
    3|bash) echo bash ;;
    q|Q)    echo ""; exit 0 ;;
    "")     echo "${def}" ;;
    *)      echo "${def}" ;;
  esac
}

run_local_or_remote() {
  # $1=shell  $2=action
  local sh="$1" act="$2"
  local local_path="scripts/installers/${sh}/${act}.sh"
  local remote_url="${RAW_BASE}/installers/${sh}/${act}.sh"

  if [ -f "${local_path}" ]; then
    echo "[cdh] 使用本地脚本：${local_path}"
    exec bash "${local_path}"
  else
    echo "[cdh] 从仓库拉取：${remote_url}"
    curl -fsSL "${remote_url}" | bash
  fi
}

# ---- 参数解析（可选，默认交互式 install）----
SEL_SHELL=""
ACTION=""
while [ $# -gt 0 ]; do
  case "$1" in
    --shell)  SEL_SHELL="${2:-}"; shift 2 ;;
    --action) ACTION="${2:-}";     shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "[cdh] 未知参数: $1"; usage; exit 1 ;;
  esac
done

if [ -z "${SEL_SHELL}" ]; then
  SEL_SHELL="$(choose_shell)"
fi
[ -n "${SEL_SHELL}" ] || exit 0

if [ -z "${ACTION}" ]; then
  ACTION="install"
fi

# 目前仅 fish 可用；其他给出提示
case "${SEL_SHELL}" in
  fish)
    case "${ACTION}" in
      install|uninstall)
        run_local_or_remote "${SEL_SHELL}" "${ACTION}"
        ;;
      selfcheck)
        # 轻量自检：函数/历史/二进制提示（无需子脚本）
        if ! command -v fish >/dev/null 2>&1; then
          echo "[cdh][selfcheck] FAIL: 未安装 fish" >&2; exit 1
        fi
        if ! fish -lc 'type -q cdh'; then
          echo "[cdh][selfcheck] WARN: cdh 函数未生效；请先安装并执行：exec fish -l" >&2
          exit 2
        fi
        if fish -lc 'cd ~; cd /; cd ~; test -s ~/.cd_history_raw'; then
          echo "[cdh][selfcheck] OK: 历史文件存在且非空"
        else
          echo "[cdh][selfcheck] WARN: ~/.cd_history_raw 不存在或为空；多切换几个目录后再试" >&2
        fi
        if fish -lc 'command -sq cdh; or test -x ~/.local/bin/cdh'; then
          echo "[cdh][selfcheck] INFO: 检测到 cdh 可执行文件"
        else
          echo "[cdh][selfcheck] INFO: 未检测到外部二进制；运行时会提示安装" >&2
        fi
        ;;
      *) echo "[cdh] 未知动作: ${ACTION}"; exit 3 ;;
    esac
    ;;
  zsh|bash)
    echo "[cdh] ${SEL_SHELL} 的安装脚本尚未提供；请选择 fish 或等待后续版本。" >&2
    exit 4
    ;;
  *)
    echo "[cdh] 不支持的 shell: ${SEL_SHELL}" >&2
    exit 5
    ;;
esac
