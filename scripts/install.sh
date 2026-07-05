#!/usr/bin/env bash
# scripts/install.sh
# 顶层入口：
# - install：解析“最新”版本 → 安装 ~/.local/bin/cdh → 自动识别/选择 shell → 执行子安装器（仅集成）
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
SCRIPT_SOURCE="${BASH_SOURCE[0]:-}"
SCRIPT_DIR=""
if [[ -n "${SCRIPT_SOURCE}" && -e "${SCRIPT_SOURCE}" ]]; then
  SCRIPT_DIR="$(cd -- "$(dirname -- "${SCRIPT_SOURCE}")" >/dev/null 2>&1 && pwd -P)"
fi
PACKAGE_ROOT="${CDH_PACKAGE_ROOT:-}"
if [[ -z "${PACKAGE_ROOT}" && -n "${SCRIPT_DIR}" && -x "${SCRIPT_DIR}/cdh" && -d "${SCRIPT_DIR}/scripts/installers" ]]; then
  PACKAGE_ROOT="${SCRIPT_DIR}"
fi

# ================= 参数解析（先解析，以便决定是否需要 TTY） =================
ACTION="install"
TARGET_SHELL=""
INTERACTIVE=0
QUIET=0
NO_PROGRESS=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --action)
      [[ $# -ge 2 ]] || {
        echo "[cdh] --action 需要参数：install 或 uninstall" >&2
        exit 12
      }
      ACTION="$2"
      shift 2
      ;;
    --shell)
      [[ $# -ge 2 ]] || {
        echo "[cdh] --shell 需要参数：bash、zsh、fish 或 current" >&2
        exit 12
      }
      TARGET_SHELL="$2"
      shift 2
      ;;
    --interactive)
      INTERACTIVE=1
      shift
      ;;
    --quiet)
      QUIET=1
      shift
      ;;
    --no-progress)
      NO_PROGRESS=1
      shift
      ;;
    *)
      echo "[cdh] 未知参数：$1" >&2
      exit 12
      ;;
  esac
done

# ---- 仅 install 需要交互式 TTY；uninstall 不需要 ----
_has_tty() {
  [[ -r /dev/tty && -w /dev/tty ]] && { : > /dev/tty; } 2> /dev/null
}
_tty() {
  [[ "${QUIET}" -eq 1 ]] && return 0
  if _has_tty; then printf "%s\n" "$*" > /dev/tty; else printf "%s\n" "$*"; fi
}
_note() {
  [[ "${QUIET}" -eq 1 ]] && return 0
  if _has_tty; then printf "%s\n" "$*" > /dev/tty; else printf "%s\n" "$*" >&2; fi
}
_err() {
  if _has_tty; then printf "%s\n" "$*" > /dev/tty; else printf "%s\n" "$*" >&2; fi
}
_design_enabled() {
  [[ "${QUIET}" -eq 0 && "${TERM:-}" != "dumb" ]] && _has_tty
}
_color_enabled() {
  _design_enabled && [[ -z "${NO_COLOR:-}" ]]
}
_ui_init() {
  C_RESET=""
  C_DIM=""
  C_BOLD=""
  C_GREEN=""
  C_RED=""
  C_BLUE=""
  C_GOLD=""
  C_CYAN=""
  if _color_enabled; then
    C_RESET="$(printf '\033[0m')"
    C_DIM="$(printf '\033[2m')"
    C_BOLD="$(printf '\033[1m')"
    C_GREEN="$(printf '\033[32m')"
    C_RED="$(printf '\033[31m')"
    C_BLUE="$(printf '\033[34m')"
    C_GOLD="$(printf '\033[33m')"
    C_CYAN="$(printf '\033[36m')"
  fi
}
_ui_line() {
  [[ "${QUIET}" -eq 1 ]] && return 0
  if _has_tty; then printf "%b\n" "$*" > /dev/tty; else printf "%b\n" "$*"; fi
}
_ui_banner() {
  _design_enabled || return 0
  _ui_line ""
  _ui_line "${C_BOLD}${C_CYAN}╭─ cdh installer ─────────────────────────╮${C_RESET}"
  _ui_line "${C_CYAN}│${C_RESET}  smart directory jumping, wired cleanly  ${C_CYAN}│${C_RESET}"
  _ui_line "${C_CYAN}╰─────────────────────────────────────────╯${C_RESET}"
  _ui_line ""
}
_ui_step() {
  local label="$1" value="${2:-}"
  if _design_enabled; then
    printf "%b" "${C_BLUE}→${C_RESET} ${label}" > /dev/tty
    [[ -n "${value}" ]] && printf "%b" "  ${C_DIM}${value}${C_RESET}" > /dev/tty
    printf "\n" > /dev/tty
  fi
}
_ui_ok() {
  local label="$1" value="${2:-}"
  if _design_enabled; then
    printf "%b" "${C_GREEN}✓${C_RESET} ${label}" > /dev/tty
    [[ -n "${value}" ]] && printf "%b" "  ${C_DIM}${value}${C_RESET}" > /dev/tty
    printf "\n" > /dev/tty
  fi
}
_ui_warn() {
  local label="$1" value="${2:-}"
  if _design_enabled; then
    printf "%b" "${C_GOLD}!${C_RESET} ${label}" > /dev/tty
    [[ -n "${value}" ]] && printf "%b" "  ${C_DIM}${value}${C_RESET}" > /dev/tty
    printf "\n" > /dev/tty
  fi
}
_ui_fail() {
  local label="$1" value="${2:-}"
  if _design_enabled; then
    printf "%b" "${C_RED}✗${C_RESET} ${label}" > /dev/tty
    [[ -n "${value}" ]] && printf "%b" "  ${C_DIM}${value}${C_RESET}" > /dev/tty
    printf "\n" > /dev/tty
  else
    _err "[cdh] ${label}${value:+：${value}}"
  fi
}
_human_size() {
  local bytes="$1"
  if command -v awk > /dev/null 2>&1; then
    awk -v b="${bytes}" 'BEGIN {
      split("B KiB MiB GiB", u, " ");
      i = 1;
      while (b >= 1024 && i < 4) { b /= 1024; i++ }
      if (i == 1) printf "%d %s", b, u[i]; else printf "%.1f %s", b, u[i]
    }'
  else
    printf "%s B" "${bytes}"
  fi
}
_file_size() {
  if command -v stat > /dev/null 2>&1; then
    stat -c '%s' "$1" 2> /dev/null || stat -f '%z' "$1" 2> /dev/null || wc -c < "$1"
  else
    wc -c < "$1"
  fi
}
_ui_install_summary() {
  local shell_name="$1" bin="$2"
  if _design_enabled; then
    _ui_line ""
    _ui_line "${C_BOLD}${C_GREEN}╭─ cdh installed ─────────────────────────╮${C_RESET}"
    _ui_line "${C_GREEN}│${C_RESET}  binary   ${bin}"
    _ui_line "${C_GREEN}│${C_RESET}  shell    ${shell_name}"
    _ui_line "${C_GREEN}│${C_RESET}  next     exec ${shell_name} -l"
    _ui_line "${C_GREEN}╰─────────────────────────────────────────╯${C_RESET}"
    _ui_line ""
    _ui_line "${C_DIM}Try:${C_RESET} cdh --help"
  else
    _tty "[cdh] 安装完成。"
    _tty " - 如为 fish：执行  exec fish -l"
    _tty " - 如为 bash：执行  exec bash -l"
    _tty " - 如为 zsh：执行  exec zsh -l"
  fi
}
_ui_uninstall_summary() {
  if _design_enabled; then
    _ui_line ""
    _ui_line "${C_BOLD}${C_GREEN}╭─ cdh uninstalled ───────────────────────╮${C_RESET}"
    _ui_line "${C_GREEN}│${C_RESET}  integrations removed where detected"
    _ui_line "${C_GREEN}│${C_RESET}  binary and history cleaned"
    _ui_line "${C_GREEN}╰─────────────────────────────────────────╯${C_RESET}"
  else
    _tty "[cdh] 卸载完成。"
    _tty " - 如为 fish：执行  exec fish -l"
    _tty " - 如为 bash：执行  exec bash -l"
    _tty " - 如为 zsh：执行  exec zsh -l"
  fi
}
_ui_init
if [[ "${ACTION}" == "install" && "${INTERACTIVE}" -eq 1 ]] && ! _has_tty; then
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
    if _design_enabled && [[ "${NO_PROGRESS}" -eq 0 ]]; then
      curl -fL --progress-bar --retry 5 --retry-all-errors --connect-timeout 30 --max-time 600 -o "$out" "$url"
    else
      curl -fsSL --retry 5 --retry-all-errors --connect-timeout 30 --max-time 600 -o "$out" "$url"
    fi
  elif command -v wget > /dev/null 2>&1; then
    if _design_enabled && [[ "${NO_PROGRESS}" -eq 0 ]]; then
      wget --progress=bar:force:noscroll --timeout=600 --tries=5 -O "$out" "$url"
    else
      wget -q --timeout=600 --tries=5 -O "$out" "$url"
    fi
  else
    _err "[cdh] 需要 curl 或 wget 以下载：$url"
    return 127
  fi
}

# ---------- 解析“最新版本”（可被 CDH_VERSION 覆盖） ----------
_resolve_latest_version() {
  local eff
  if command -v curl > /dev/null 2>&1; then
    eff="$(curl -fsSLI --retry 2 --connect-timeout 15 --max-time 60 -o /dev/null -w '%{url_effective}' "https://github.com/${OWNER}/${REPO}/releases/latest" 2> /dev/null || true)"
  elif command -v wget > /dev/null 2>&1; then
    eff="$(wget -q --timeout=60 --tries=2 --max-redirect=5 --spider --server-response "https://github.com/${OWNER}/${REPO}/releases/latest" 2>&1 | awk '/^  Location: / {print $2}' | tail -n 1 | tr -d '\r' || true)"
  else
    eff=""
  fi
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
    _ui_ok "Binary already installed" "${bindir}/cdh"
    _tty "[cdh] 检测到已存在的 cdh 二进制，跳过下载。"
    return 0
  fi

  if [[ -n "${PACKAGE_ROOT}" && -x "${PACKAGE_ROOT}/cdh" ]]; then
    _ui_step "Install binary" "${bindir}/cdh"
    install -m 0755 "${PACKAGE_ROOT}/cdh" "${bindir}/cdh"
    _ui_ok "Install binary" "${bindir}/cdh"
    _tty "[cdh] 已从发布包安装二进制到：${bindir}/cdh"
    return 0
  fi

  version="${CDH_VERSION:-}"
  if [[ -z "${version}" ]]; then
    _ui_step "Resolve version" "GitHub latest"
    version="$(_resolve_latest_version)"
    if [[ -n "${version}" ]]; then
      _ui_ok "Resolve version" "${version}"
      _tty "[cdh] 使用最新版本：${version}"
    else
      _ui_fail "Resolve version" "failed"
      _tty "[cdh] 错误：解析最新版本失败。请检查网络，或使用 CDH_VERSION=vX.Y.Z 指定版本。"
      return 1
    fi
  else
    _ui_ok "Resolve version" "${version}"
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

  _ui_step "Download binary" "${tarball}"
  _tty "[cdh] 获取二进制：${url}"
  if ! _fetch "${url}" "${tarpath}"; then
    _ui_fail "Download binary" "failed"
    _tty "[cdh] 错误：二进制下载失败：${url}"
    return 1
  fi
  if [[ ! -s "${tarpath}" ]]; then
    _ui_fail "Download binary" "empty file"
    _tty "[cdh] 错误：下载文件为空：${url}"
    return 1
  fi
  _ui_ok "Download binary" "$(_human_size "$(_file_size "${tarpath}")")"

  unpack="${STAGE_DIR}/unpack"
  mkdir -p "${unpack}"
  _ui_step "Verify archive" "${tarball}"
  if ! tar -xzf "${tarpath}" -C "${unpack}"; then
    _ui_fail "Verify archive" "tar failed"
    _tty "[cdh] 错误：无法解压二进制包：${tarpath}"
    return 1
  fi
  _ui_ok "Verify archive" "ok"

  # 智能定位可执行文件
  found=""
  [[ -x "${unpack}/cdh" ]] && found="${unpack}/cdh"
  [[ -z "${found}" ]] && found="$(find "${unpack}" -maxdepth 3 -type f -name 'cdh' -perm -u+x -print -quit 2> /dev/null || true)"
  [[ -z "${found}" ]] && found="$(find "${unpack}" -maxdepth 3 -type f -name 'cdh*' -perm -u+x -print -quit 2> /dev/null || true)"

  if [[ -z "${found}" ]]; then
    _ui_fail "Locate binary" "not found"
    _tty "[cdh] 警告：未在压缩包中定位到可执行文件。以下为内容清单："
    tar -tzf "${tarpath}" | sed 's/^/[cdh]   /' >&2
    return 1
  fi

  _ui_step "Install binary" "${bindir}/cdh"
  install -m 0755 "${found}" "${bindir}/cdh"
  _ui_ok "Install binary" "${bindir}/cdh"
  _tty "[cdh] 已安装二进制到：${bindir}/cdh"
}

# ---------- 卸载二进制与历史 ----------
_uninstall_binary_and_data() {
  local bindir="${HOME}/.local/bin"
  local bin="${bindir}/cdh"
  local legacy_hist="${HOME}/.cd_history_raw"
  local data_base="${XDG_DATA_HOME:-${HOME}/.local/share}"
  local state_base="${XDG_STATE_HOME:-${HOME}/.local/state}"
  local data_dir="${data_base}/cdh"
  local state_dir="${state_base}/cdh"

  _ui_step "Clean binary" "${bin}"
  rm -f "${bin}" "${legacy_hist}" 2> /dev/null || true
  _ui_ok "Clean binary" "${bin}"
  _ui_step "Clean history" "${data_dir}"
  rm -rf "${data_dir}" "${state_dir}" 2> /dev/null || true
  _ui_ok "Clean history" "removed if present"

  _tty "[cdh] 已卸载二进制与历史（若存在则已删除）："
  _tty " - ${bin}"
  _tty " - ${legacy_hist}"
  _tty " - ${data_dir}"
  _tty " - ${state_dir}"
}

# ---------- 检测/枚举 shell ----------
_has_fish_integration() {
  [[ -e "${HOME}/.config/fish/functions/cdh.fish" || -e "${HOME}/.config/fish/conf.d/cdh_log.fish" ]]
}
_has_bash_integration() {
  [[ -e "${HOME}/.config/cdh/bash/cdh.bash" ]] || grep -q '^# >>> cdh installer >>>$' "${HOME}/.bashrc" 2> /dev/null
}
_has_zsh_integration() {
  [[ -e "${HOME}/.config/cdh/zsh/cdh.zsh" ]] || grep -q '^# >>> cdh zsh integration >>>$' "${HOME}/.zshrc" 2> /dev/null
}

declare -a SHELLS=()
_add_if() { command -v "$1" > /dev/null 2>&1 && SHELLS+=("$1"); }
_is_supported_shell() {
  case "$1" in
    fish | bash | zsh) command -v "$1" > /dev/null 2>&1 ;;
    *) return 1 ;;
  esac
}
_detect_current_shell() {
  local shell_name
  shell_name="$(basename "${SHELL:-}")"
  if _is_supported_shell "${shell_name}"; then
    printf "%s" "${shell_name}"
    return 0
  fi
  return 1
}
_resolve_target_shell() {
  local selected=""
  case "${TARGET_SHELL}" in
    "")
      if [[ "${INTERACTIVE}" -eq 0 ]]; then
        selected="$(_detect_current_shell || true)"
        if [[ -n "${selected}" ]]; then
          _note "[cdh] 自动识别当前 shell：${selected}"
          printf "%s" "${selected}"
          return 0
        fi
      fi
      ;;
    current)
      selected="$(_detect_current_shell || true)"
      if [[ -n "${selected}" ]]; then
        _note "[cdh] 自动识别当前 shell：${selected}"
        printf "%s" "${selected}"
        return 0
      fi
      _note "[cdh] 无法从 SHELL=${SHELL:-<empty>} 识别支持的 shell。"
      return 1
      ;;
    fish | bash | zsh)
      if _is_supported_shell "${TARGET_SHELL}"; then
        _note "[cdh] 使用指定 shell：${TARGET_SHELL}"
        printf "%s" "${TARGET_SHELL}"
        return 0
      fi
      _note "[cdh] 指定的 shell 未安装或不可用：${TARGET_SHELL}"
      return 1
      ;;
    *)
      _note "[cdh] 不支持的 shell：${TARGET_SHELL}（支持：fish / zsh / bash）"
      return 1
      ;;
  esac

  if ! _has_tty; then
    _note "[cdh] 无法自动识别支持的 shell，且当前没有可交互 TTY。"
    _note "[cdh] 请设置 SHELL，或使用 --shell bash|zsh|fish 指定。"
    return 1
  fi
  _choose_shell_interactive
}

# ---------- 交互选择 shell（仅安装时作为兜底使用） ----------
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
        fish | bash | zsh) _tty "  $((i + 1))) ${SHELLS[i]}" ;;
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
  if [[ -n "${PACKAGE_ROOT}" && -f "${PACKAGE_ROOT}/scripts/installers/${sel}/${kind}.sh" ]]; then
    _ui_step "Stage ${sel} ${kind}" "release package"
    _tty "[cdh] 使用发布包内 ${sel} ${kind} 脚本 ..."
    cp "${PACKAGE_ROOT}/scripts/installers/${sel}/${kind}.sh" "$dst"
    if [[ -d "${PACKAGE_ROOT}/scripts/installers/${sel}/payload" ]]; then
      mkdir -p "${STAGE_DIR}/payload"
      cp -R "${PACKAGE_ROOT}/scripts/installers/${sel}/payload/." "${STAGE_DIR}/payload/"
    fi
    RAW_BASE="${PACKAGE_ROOT}/scripts"
  else
    _ui_step "Download ${sel} ${kind}" "${url}"
    _tty "[cdh] 下载 ${sel} ${kind} 脚本 ..."
    if ! _fetch "$url" "$dst"; then
      _ui_fail "Download ${sel} ${kind}" "failed"
      return 1
    fi
  fi
  chmod +x "$dst"
  _ui_step "Configure ${sel}" "${kind}"
  _tty "[cdh] 执行 ${kind} ..."
  env -u LC_ALL -u LANG STAGE_DIR="${STAGE_DIR}" RAW_BASE="${RAW_BASE}" bash "$dst"
  _ui_ok "Configure ${sel}" "${kind}"
}

# ================= 主流程 =================
case "${ACTION}" in
  install)
    _ui_banner
    _ui_step "Detect shell" "${TARGET_SHELL:-current}"
    SEL_SHELL="$(_resolve_target_shell)" || exit 11
    [[ -z "${SEL_SHELL}" ]] && {
      _tty "[cdh] 已取消。"
      exit 0
    }
    _ui_ok "Detect shell" "${SEL_SHELL}"
    if ! _install_binary_latest; then
      _ui_fail "Install cdh" "binary install failed"
      _tty "[cdh] 安装中止：二进制安装失败。"
      exit 20
    fi
    case "${SEL_SHELL}" in
      fish) _run_child_staged "fish" "install" ;;
      bash) _run_child_staged "bash" "install" ;;
      zsh)  _run_child_staged "zsh"  "install" ;;
      *)
        _tty "[cdh] 未识别的 shell：${SEL_SHELL}"
        exit 11
        ;;
    esac
    _ui_install_summary "${SEL_SHELL}" "${HOME}/.local/bin/cdh"
    ;;
  uninstall)
    _ui_banner
    # —— 自动检测，无需交互 ——
    if command -v fish > /dev/null 2>&1 && _has_fish_integration; then
      _run_child_staged "fish" "uninstall"
    else
      _ui_warn "Skip fish" "integration not found"
      _tty "[cdh] 未发现 fish 集成（跳过子卸载）。"
    fi

    if command -v bash > /dev/null 2>&1 && _has_bash_integration; then
      _run_child_staged "bash" "uninstall"
    else
      _ui_warn "Skip bash" "integration not found"
      _tty "[cdh] 未发现 bash 集成（跳过子卸载）。"
    fi

    if command -v zsh > /dev/null 2>&1 && _has_zsh_integration; then
      _run_child_staged "zsh" "uninstall"
    else
      _ui_warn "Skip zsh" "integration not found"
      _tty "[cdh] 未发现 zsh 集成（跳过子卸载）。"
    fi

    _uninstall_binary_and_data
    _ui_uninstall_summary
    ;;
  *)
    _tty "[cdh] 未知动作：${ACTION}"
    exit 12
    ;;
esac
