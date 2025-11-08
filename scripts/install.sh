#!/usr/bin/env bash
# scripts/install.sh
# 统一入口：检测可用 shell，交互选择；路由到具体 installers/<shell>/{install,uninstall}.sh
set -Eeuo pipefail

# 不主动设置 locale，避免 setlocale 警告（若通过管道执行，建议调用方用 `env -u LC_ALL -u LANG`）
unset LC_ALL || true
unset LANG   || true

OWNER="xianyudd"
REPO="cdh"
BRANCH="main"
RAW_BASE="https://raw.githubusercontent.com/${OWNER}/${REPO}/${BRANCH}/scripts"

# 下载器（curl 优先，wget 兜底）
_fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 --connect-timeout 5 "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- --tries=3 --timeout=5 "$1"
  else
    echo "[cdh] 需要 curl 或 wget 以下载远端脚本。" >&2
    exit 127
  fi
}

_usage() {
  cat <<USAGE
Usage:
  $(basename "$0") [--action install|uninstall|selfcheck]

无参数：交互选择要安装到的 shell（当前已实现：fish）。
USAGE
}

# 列出系统安装的常见 shell（按优先级排序展示）
_list_installed_shells() {
  local out=()
  command -v fish >/dev/null 2>&1 && out+=("fish")
  command -v zsh  >/dev/null 2>&1 && out+=("zsh")
  command -v bash >/dev/null 2>&1 && out+=("bash")
  printf "%s\n" "${out[@]}"
}

# 猜测当前 shell（用于交互默认值）
_detect_current_shell() {
  case "${SHELL##*/}" in
    fish) echo "fish" ;;
    zsh)  echo "zsh"  ;;
    bash) echo "bash" ;;
    *)    echo "" ;;
  esac
}

# 读取一行（支持被管道调用时从 /dev/tty 读）
_read_line() {
  local __var="$1"; shift || true
  local __prompt="${*:-}"
  if [[ -n "${__prompt}" ]]; then printf "%s" "${__prompt}"; fi
  local ans=""
  if [ -t 0 ]; then
    read -r ans || true
  elif [ -r /dev/tty ]; then
    # shellcheck disable=SC2162
    read -r ans < /dev/tty || true
  else
    ans=""
  fi
  printf -v "${__var}" '%s' "${ans}"
}

# 交互：列出已安装的 shell，提示用户选择
_choose_shell() {
  local installed; installed="$(_list_installed_shells | tr '\n' ' ')"
  local default;   default="$(_detect_current_shell)"
  [[ -z "${default}" ]] && default="fish"

  echo "[cdh] 检测到已安装的 shell: ${installed:-<none>}"
  echo "[cdh] 请选择要安装到的 shell（仅已实现：fish）："
  local idx=1
  for s in ${installed}; do
    if [[ "${s}" == "fish" ]]; then
      printf "  %d) %s\n" "${idx}" "${s}"
    else
      printf "  %d) %s  （未实现安装器）\n" "${idx}" "${s}"
    fi
    idx=$((idx+1))
  done
  echo "  q) 退出"
  _read_line ans "[cdh] 输入序号或名称（回车默认 ${default}）："

  case "${ans:-}" in
    q|Q) echo ""; return 0 ;;
    "" )
      echo "${default}"; return 0 ;;
    1)  # 序号 1 对应 installed 列表第一个
        echo "${installed}" | awk '{print $1}'; return 0 ;;
    2)
        echo "${installed}" | awk '{print $2}'; return 0 ;;
    3)
        echo "${installed}" | awk '{print $3}'; return 0 ;;
    fish|zsh|bash)
        echo "${ans}"; return 0 ;;
    *)
        # 非法输入，回落默认
        echo "${default}"; return 0 ;;
  esac
}

# 路由到 installers/<shell>/<action>.sh（本地优先，远端兜底）
_run_local_or_remote() {
  # $1=shell $2=action
  local sh="$1" act="$2"
  local local_path="scripts/installers/${sh}/${act}.sh"
  local remote_url="${RAW_BASE}/installers/${sh}/${act}.sh"

  if [[ -f "${local_path}" ]]; then
    echo "[cdh] 使用本地脚本：${local_path}"
    exec bash "${local_path}"
  else
    echo "[cdh] 从仓库获取：${remote_url}"
    _fetch "${remote_url}" | env -u LC_ALL -u LANG bash
  fi
}

# 自检（fish 已实现；其它给出说明）
_selfcheck() {
  local sh="$1"
  case "${sh}" in
    fish)
      if ! command -v fish >/dev/null 2>&1; then
        echo "[cdh][selfcheck] 未检测到 fish。请先安装 fish。" >&2
        return 1
      fi
      if ! fish -lc 'type -q cdh'; then
        echo "[cdh][selfcheck] cdh 函数未生效；请先安装并执行：exec fish -l" >&2
        return 2
      fi
      if fish -lc 'cd ~; cd /; cd ~; test -s ~/.cd_history_raw'; then
        echo "[cdh][selfcheck] 历史文件 OK：~/.cd_history_raw"
      else
        echo "[cdh][selfcheck] 历史为空；先多切换几个目录再试。" >&2
      fi
      if fish -lc 'command -sq cdh; or test -x ~/.local/bin/cdh'; then
        echo "[cdh][selfcheck] 外部二进制已检测到（PATH 或 ~/.local/bin/cdh）"
      else
        echo "[cdh][selfcheck] 未检测到外部二进制；运行时会提示安装（正常）。"
      fi
      ;;
    zsh|bash)
      echo "[cdh][selfcheck] ${sh} 的安装器尚未实现；当前仅支持 fish。" >&2
      return 4
      ;;
    *)
      echo "[cdh][selfcheck] 不支持的 shell: ${sh}" >&2
      return 5
      ;;
  esac
}

# ---------------- 主流程 ----------------
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
[[ -z "${SEL_SHELL}" ]] && exit 0   # 用户选择 q 退出

# 对未实现的 shell 给出清晰说明
if [[ "${SEL_SHELL}" != "fish" ]]; then
  case "${ACTION}" in
    install|uninstall)
      echo "[cdh] ${SEL_SHELL} 的安装/卸载脚本尚未实现；当前仅支持 fish。" >&2
      exit 10
      ;;
    selfcheck)
      _selfcheck "${SEL_SHELL}" || exit $?
      exit 0
      ;;
    *)
      echo "[cdh] 未知动作：${ACTION}" >&2
      exit 11
      ;;
  esac
fi

# fish 路由
case "${ACTION}" in
  install|uninstall) _run_local_or_remote "fish" "${ACTION}" ;;
  selfcheck)         _selfcheck "fish" || exit $? ;;
  *)                 echo "[cdh] 未知动作：${ACTION}" >&2; exit 12 ;;
esac
