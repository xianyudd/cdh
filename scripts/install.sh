#!/usr/bin/env bash
# scripts/install.sh
# 统一入口：检测可用 shell，交互选择；将 installers/<shell>/<action>.sh 下载到本地临时目录并“本地执行”
set -Eeuo pipefail

# 避免 locale 警告
unset LC_ALL || true
unset LANG   || true

OWNER="xianyudd"
REPO="cdh"
BRANCH="main"
RAW_BASE="https://raw.githubusercontent.com/${OWNER}/${REPO}/${BRANCH}/scripts"

# ---------- 阶段目录（本脚本退出时自动清理） ----------
STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/cdh-stage.XXXXXX")"
_cleanup() { rm -rf "${STAGE_DIR}" || true; }
trap _cleanup EXIT

# ---------- 下载器 ----------
_fetch() {
  local url="$1" out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 1 --connect-timeout 2 -o "$out" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$out" "$url"
  else
    echo "[cdh] 需要 curl 或 wget 以下载远端脚本。" >&2
    return 127
  fi
}

_usage() {
  cat <<USAGE
Usage:
  $(basename "$0") [--action install|uninstall|selfcheck]

无参数：交互选择要安装到的 shell（当前仅实现 fish）。
USAGE
}

_list_installed_shells() {
  local out=()
  command -v fish >/dev/null 2>&1 && out+=("fish")
  command -v zsh  >/dev/null 2>&1 && out+=("zsh")
  command -v bash >/dev/null 2>&1 && out+=("bash")
  printf "%s\n" "${out[@]}"
}

_detect_current_shell() {
  case "${SHELL##*/}" in
    fish) echo fish ;;
    zsh)  echo zsh  ;;
    bash) echo bash ;;
    *)    echo ""   ;;
  esac
}

_read_line() {
  local __var="$1"; shift || true
  local __prompt="${*:-}"
  [[ -n "$__prompt" ]] && printf "%s" "$__prompt"
  local ans=""
  if [ -t 0 ]; then
    read -r ans || true
  elif [ -r /dev/tty ]; then
    # shellcheck disable=SC2162
    read -r ans < /dev/tty || true
  fi
  printf -v "${__var}" '%s' "${ans}"
}

_choose_shell() {
  local installed; installed="$(_list_installed_shells | tr '\n' ' ')"
  local def; def="$(_detect_current_shell)"; [[ -z "$def" ]] && def="fish"

  echo "[cdh] 检测到 shell: ${installed:-<none>}"
  echo "[cdh] 请选择要安装到的 shell（已实现：fish）："
  local idx=1
  for s in ${installed}; do
    if [[ "$s" == "fish" ]]; then
      printf "  %d) %s\n" "$idx" "$s"
    else
      printf "  %d) %s  （尚未实现安装器）\n" "$idx" "$s"
    fi
    idx=$((idx+1))
  done
  echo "  q) 退出"
  _read_line ans "[cdh] 输入序号或名称（回车默认 ${def}）："

  case "${ans:-}" in
    q|Q) echo ""; return 0 ;;
    "")  echo "${def}"; return 0 ;;
    1)   echo ${installed} | awk '{print $1}'; return 0 ;;
    2)   echo ${installed} | awk '{print $2}'; return 0 ;;
    3)   echo ${installed} | awk '{print $3}'; return 0 ;;
    fish|zsh|bash) echo "${ans}"; return 0 ;;
    *)   echo "${def}"; return 0 ;;
  esac
}

# 将远端 installers/<shell>/<action>.sh 下载到本地并执行（不使用 exec，返回后统一清理 STAGE_DIR）
_run_remote_staged() {
  # $1=shell $2=action
  local sh="$1" act="$2" dst="${STAGE_DIR}/${sh}-${act}.sh"
  local url="${RAW_BASE}/installers/${sh}/${act}.sh"
  echo "[cdh] 下载 ${sh}/${act}.sh ..."
  _fetch "$url" "$dst"
  chmod +x "$dst"
  echo "[cdh] 执行 ${dst} ..."
  # 透传 STAGE_DIR 给子脚本使用（子脚本若检测到该变量存在，则不再清理它）
  STAGE_DIR="${STAGE_DIR}" env -u LC_ALL -u LANG bash "$dst"
}

_selfcheck_fish() {
  command -v fish >/dev/null 2>&1 || { echo "[cdh] 未安装 fish" >&2; return 1; }
  fish -lc 'type -q cdh'           || { echo "[cdh] cdh 函数未生效；请先安装并执行：exec fish -l" >&2; return 2; }
  if fish -lc 'cd ~; cd /; cd ~; test -s ~/.cd_history_raw'; then
    echo "[cdh] 历史文件 OK：~/.cd_history_raw"
  else
    echo "[cdh] 历史为空；多切换几个目录后再试。"
  fi
  if fish -lc 'command -sq cdh; or test -x ~/.local/bin/cdh'; then
    echo "[cdh] 检测到外部二进制（PATH 或 ~/.local/bin/cdh）"
  else
    echo "[cdh] 未检测到外部二进制；运行时会提示安装（正常）。"
  fi
}

# -------- 主流程 --------
ACTION=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --action) ACTION="${2:-}"; shift 2 ;;
    -h|--help) _usage; exit 0 ;;
    *) break ;;
  esac
done
[[ -z "${ACTION}" ]] && ACTION="install"

SEL_SHELL="$(_choose_shell)"
[[ -z "${SEL_SHELL}" ]] && { echo "[cdh] 已取消。"; exit 0; }

if [[ "${SEL_SHELL}" != "fish" ]]; then
  case "${ACTION}" in
    install|uninstall) echo "[cdh] ${SEL_SHELL} 的安装/卸载脚本尚未实现；当前仅支持 fish。" >&2; exit 10 ;;
    selfcheck)         echo "[cdh] 仅 fish 支持自检。" >&2; exit 11 ;;
  esac
fi

case "${ACTION}" in
  install|uninstall) _run_remote_staged "fish" "${ACTION}" ;;
  selfcheck)         _selfcheck_fish ;;
  *) echo "[cdh] 未知动作：${ACTION}" >&2; exit 12 ;;
esac

echo "[cdh] 完成。"
