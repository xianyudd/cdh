#!/usr/bin/env bash
# scripts/install.sh
# 顶层入口：
# - install：解析“最新”版本 → 安装 ~/.local/bin/cdh → 交互选择 shell → 执行子安装器（仅集成）
# - uninstall：自动检测 shell 集成 → 执行子卸载器（仅集成）→ 顶层移除二进制与历史
# - 资源均落到临时目录，退出自动清理；不落盘日志
set -Eeuo pipefail

# ---- 避免 locale 警告（调用时可再加：env -u LC_ALL -u LANG bash --noprofile --norc）----
unset LC_ALL || true
unset LANG || true

OWNER="xianyudd"
REPO="cdh"
BRANCH="main"
RAW_BASE="https://raw.githubusercontent.com/${OWNER}/${REPO}/${BRANCH}/scripts"

# ================= 参数解析（先解析，以便决定是否需要 TTY） =================
ACTION="install"
if [[ $# -ge 2 && "$1" == "--action" ]]; then
  ACTION="$2"
  shift 2
fi

# ---- 仅 install 需要交互式 TTY；uninstall 不需要 ----
_tty() {
  if [[ -w /dev/tty ]]; then printf "%s\n" "$*" > /dev/tty; else printf "%s\n" "$*"; fi
}
if [[ "${ACTION}" == "install" && ! -w /dev/tty ]]; then
  echo "[cdh] 需要可交互的 TTY 才能选择目标 shell。请在交互式终端运行此命令。" >&2
  exit 64
fi

# ---------- 阶段目录（退出时统一清理） ----------
STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/cdh-stage.XXXXXX")"
_cleanup() { rm -rf "${STAGE_DIR}" || true; }
trap _cleanup EXIT

# ---------- 下载器 ----------
_fetch() {
  local url="$1" out="$2"
  if command -v curl > /dev/null 2>&1; then
    curl -fsSL --retry 1 --connect-timeout 4 -o "$out" "$url"
  elif command -v wget > /dev/null 2>&1; then
    wget -q -O "$out" "$url"
  else
    _tty "[cdh] 需要 curl 或 wget 以下载：$url"
    return 127
  fi
}

# ---------- 解析“最新版本”（可被 CDH_VERSION 覆盖） ----------
_resolve_latest_version() {
  local eff
  eff="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/${OWNER}/${REPO}/releases/latest" 2> /dev/null || true)"
  case "${eff}" in
    */tag/*) printf "%s" "${eff##*/tag/}" ;;
    *) printf "" ;;
  esac
}

# ---------- 安装 cdh 二进制到 ~/.local/bin/cdh ----------
_install_binary_latest() {
  local version arch_triple os_triple tarball url tarpath unpack found
  local bindir="${HOME}/.local/bin"
  mkdir -p "${bindir}" "${STAGE_DIR}"

  # 已存在则跳过
  if command -v cdh > /dev/null 2>&1 || [[ -x "${bindir}/cdh" ]]; then
    _tty "[cdh] 检测到已存在的 cdh 二进制，跳过下载。"
    return 0
  fi

  version="${CDH_VERSION:-}"
  if [[ -z "${version}" ]]; then
    version="$(_resolve_latest_version)"
    if [[ -n "${version}" ]]; then
      _tty "[cdh] 使用最新版本：${version}"
    else
      version="v0.1.1"
      _tty "[cdh] 警告：解析最新版本失败，回落到 ${version}"
    fi
  else
    _tty "[cdh] 使用环境指定版本：${version}"
  fi

  case "$(uname -s || echo Linux)" in
    Linux) os_triple="unknown-linux-gnu" ;;
    Darwin) os_triple="apple-darwin" ;;
    *) os_triple="unknown-linux-gnu" ;;
  esac
  case "$(uname -m || echo x86_64)" in
    x86_64 | amd64) arch_triple="x86_64" ;;
    aarch64 | arm64) arch_triple="aarch64" ;;
    *) arch_triple="x86_64" ;;
  esac

  tarball="cdh-${version}-${arch_triple}-${os_triple}.tar.gz"
  url="https://github.com/${OWNER}/${REPO}/releases/download/${version}/${tarball}"
  tarpath="${STAGE_DIR}/${tarball}"

  _tty "[cdh] 获取二进制：${url}"
  _fetch "${url}" "${tarpath}"

  unpack="${STAGE_DIR}/unpack"
  mkdir -p "${unpack}"
  tar -xzf "${tarpath}" -C "${unpack}"

  # 智能定位可执行文件
  found=""
  [[ -x "${unpack}/cdh" ]] && found="${unpack}/cdh"
  [[ -z "${found}" ]] && found="$(find "${unpack}" -maxdepth 3 -type f -name 'cdh' -perm -u+x -print -quit 2> /dev/null || true)"
  [[ -z "${found}" ]] && found="$(find "${unpack}" -maxdepth 3 -type f -name 'cdh*' -perm -u+x -print -quit 2> /dev/null || true)"

  if [[ -z "${found}" ]]; then
    _tty "[cdh] 警告：未在压缩包中定位到可执行文件。以下为内容清单："
    tar -tzf "${tarpath}" | sed 's/^/[cdh]   /' >&2
    return 1
  fi

  install -m 0755 "${found}" "${bindir}/cdh"
  _tty "[cdh] 已安装二进制到：${bindir}/cdh"
}

# ---------- 卸载二进制与历史 ----------
_uninstall_binary_and_data() {
  local bindir="${HOME}/.local/bin"
  local bin="${bindir}/cdh"
  local hist="${HOME}/.cd_history_raw"
  rm -f "${bin}" "${hist}" 2> /dev/null || true
  _tty "[cdh] 已卸载二进制与历史（若存在则已删除）："
  _tty " - ${bin}"
  _tty " - ${hist}"
}

# ---------- 检测/枚举 shell ----------
_has_fish_integration() {
  [[ -e "${HOME}/.config/fish/functions/cdh.fish" || -e "${HOME}/.config/fish/conf.d/cdh_log.fish" ]]
}
_has_bash_integration() {
  [[ -e "${HOME}/.config/cdh/bash/cdh.bash" ]] || grep -q '^# >>> cdh installer >>>$' "${HOME}/.bashrc" 2> /dev/null
}
declare -a SHELLS=()
_add_if() { command -v "$1" > /dev/null 2>&1 && SHELLS+=("$1"); }

# ---------- 必须选择 shell（仅安装时使用） ----------
_choose_shell_interactive() {
  SHELLS=()
  _add_if fish
  _add_if zsh
  _add_if bash
  if ((${#SHELLS[@]} == 0)); then
    _tty "[cdh] 未检测到可用 shell。"
    exit 65
  fi
  local ans
  while :; do
    _tty "[cdh] 请选择要安装到的 shell："
    local i
    for ((i = 0; i < ${#SHELLS[@]}; i++)); do
      case "${SHELLS[i]}" in
        fish | bash) _tty "  $((i + 1))) ${SHELLS[i]}" ;;
        *) _tty "  $((i + 1))) ${SHELLS[i]}  （未实现安装器）" ;;
      esac
    done
    _tty "  q) 退出"
    printf "[cdh] 请输入序号或名称： " > /dev/tty
    # shellcheck disable=SC2162
    read -r ans < /dev/tty || true
    case "${ans}" in
      q | Q)
        echo ""
        return 0
        ;;
      '') _tty "[cdh] 不能为空，请重新输入。" ;;
      *)
        if [[ "${ans}" =~ ^[0-9]+$ ]]; then
          local idx=$((ans - 1))
          if ((idx >= 0 && idx < ${#SHELLS[@]})); then
            echo "${SHELLS[idx]}"
            return 0
          else
            _tty "[cdh] 无效序号：${ans}"
          fi
        else
          local s
          for s in "${SHELLS[@]}"; do
            [[ "${ans}" == "${s}" ]] && {
              echo "${s}"
              return 0
            }
          done
          _tty "[cdh] 非法名称：${ans}"
        fi
        ;;
    esac
  done
}

# ---------- 运行子安装/卸载器（子脚本只做集成，不动二进制） ----------
_run_child_staged() {
  local sel="$1" kind="$2" # kind: install|uninstall
  local dst="${STAGE_DIR}/${sel}-${kind}.sh"
  local url="${RAW_BASE}/installers/${sel}/${kind}.sh"
  _tty "[cdh] 下载 ${sel} ${kind} 脚本 ..."
  _fetch "$url" "$dst"
  chmod +x "$dst"
  _tty "[cdh] 执行 ${kind} ..."
  STAGE_DIR="${STAGE_DIR}" env -u LC_ALL -u LANG bash "$dst"
}

# ================= 主流程 =================
case "${ACTION}" in
  install)
    _install_binary_latest || _tty "[cdh] 二进制安装可能未完成，请检查上述提示。"
    SEL_SHELL="$(_choose_shell_interactive)"
    [[ -z "${SEL_SHELL}" ]] && {
      _tty "[cdh] 已取消。"
      exit 0
    }
    case "${SEL_SHELL}" in
      fish) _run_child_staged "fish" "install" ;;
      bash) _run_child_staged "bash" "install" ;;
      zsh)
        _tty "[cdh] zsh 的安装器暂未实现。"
        exit 10
        ;;
      *)
        _tty "[cdh] 未识别的 shell：${SEL_SHELL}"
        exit 11
        ;;
    esac
    _tty "[cdh] 安装完成。"
    _tty " - 如为 fish：执行  exec fish -l"
    _tty " - 如为 bash：执行  exec bash -l"
    ;;
  uninstall)
    # —— 自动检测，无需交互 ——
    if command -v fish > /dev/null 2>&1 && _has_fish_integration; then
      _run_child_staged "fish" "uninstall"
    else
      _tty "[cdh] 未发现 fish 集成（跳过子卸载）。"
    fi
    if command -v bash > /dev/null 2>&1 && _has_bash_integration; then
      _run_child_staged "bash" "uninstall"
    else
      _tty "[cdh] 未发现 bash 集成（跳过子卸载）。"
    fi
    _uninstall_binary_and_data
    _tty "[cdh] 卸载完成。"
    _tty " - 如为 fish：执行  exec fish -l"
    _tty " - 如为 bash：执行  exec bash -l"
    ;;
  *)
    _tty "[cdh] 未知动作：${ACTION}"
    exit 12
    ;;
esac
