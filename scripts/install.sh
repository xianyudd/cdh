#!/usr/bin/env bash
# cdh 一键安装/卸载脚本（Linux/macOS；bash/zsh/fish）
# - TUI 渲染 stderr；stdout 仅输出最终选中目录
# - 优先 aria2c 并发下载，回退 curl（IPv4/HTTP1.1 + 断点续传 + 重试）
# - 支持 GH_TOKEN；可校验 .sha256；可卸载；修复 tmpdir 作用域与 fish 去抖初始化问题
set -Eeuo pipefail

REPO="xianyudd/cdh"
APP="cdh"

# ---------------- 参数 & 环境 ----------------
PREFIX_DEFAULT="${HOME}/.local"
BIN_DIR_DEFAULT="${PREFIX_DEFAULT}/bin"

CDH_VERSION="${CDH_VERSION:-}"              # vX.Y.Z；空则拉 latest
CDH_BIN_DIR="${CDH_BIN_DIR:-$BIN_DIR_DEFAULT}"
CDH_SHELL="${CDH_SHELL:-auto}"              # auto|bash|zsh|fish|all|none
CDH_INSTALL_LOGGER="${CDH_INSTALL_LOGGER:-auto}"  # auto|none
CDH_YES="${CDH_YES:-0}"                     # 1=不询问
CDH_FORCE="${CDH_FORCE:-0}"                 # 1=覆盖 fish 的同名函数
DO_UNINSTALL=0
VERIFY_SHA="${VERIFY_SHA:-auto}"            # auto|always|never

# ---------------- 实用函数 ----------------
color() { printf "\033[%sm%s\033[0m\n" "$1" "$2"; }
info()  { color "36" "==> $*"; }
ok()    { color "32" "✔ $*"; }
warn()  { color "33" "⚠ $*"; }
err()   { color "31" "✘ $*" >&2; }

need_cmd() { command -v "$1" >/dev/null 2>&1 || { err "缺少依赖：$1"; exit 1; }; }

confirm() {
  [[ "$CDH_YES" = "1" ]] && return 0
  read -r -p "$* [y/N]: " ans
  [[ "${ans:-N}" =~ ^[Yy]$ ]]
}

detect_shell() {
  local s
  s="$(ps -p $$ -o comm= | head -n1 | xargs basename 2>/dev/null || true)"
  [[ -z "$s" || "$s" = "sh" ]] && s="$(basename "${SHELL:-sh}")"
  echo "$s"
}

OS="$(uname -s)"
ARCH="$(uname -m)"
SHELL_BASENAME="$(detect_shell)"

# 全局 tmpdir（避免 set -u + EXIT trap 的作用域问题）
_CDH_TMPDIR=""
cleanup() {
  if [[ -n "${_CDH_TMPDIR:-}" && -d "${_CDH_TMPDIR:-}" ]]; then
    rm -rf "$_CDH_TMPDIR" || true
  fi
}
trap cleanup EXIT

# ---------------- 平台检测 ----------------
detect_target() {
  local os_tag arch_tag
  case "$OS" in
    Linux)  os_tag=linux ;;
    Darwin) os_tag=darwin ;;
    *) err "不支持的系统：$OS"; exit 1 ;;
  esac
  case "$ARCH" in
    x86_64|amd64) arch_tag=x86_64 ;;
    arm64|aarch64) arch_tag=aarch64 ;;
    *) err "不支持的架构：$ARCH"; exit 1 ;;
  esac
  case "${os_tag}-${arch_tag}" in
    linux-x86_64)   TARGET="x86_64-unknown-linux-gnu" ;;
    darwin-x86_64)  TARGET="x86_64-apple-darwin" ;;
    darwin-aarch64) TARGET="aarch64-apple-darwin" ;;
    linux-aarch64)
      err "当前未提供 Linux aarch64 预构建资产；请改用 x86_64 或从源码构建。"
      exit 1
      ;;
  esac
}

# ---------------- 路径/环境 ----------------
ensure_bindir() {
  mkdir -p "$CDH_BIN_DIR"
  case ":$PATH:" in *":$CDH_BIN_DIR:"*) ;; *)
    warn "PATH 中没有 $CDH_BIN_DIR，将尝试写入 rc"
    case "$SHELL_BASENAME" in
      fish)
        if command -v fish >/dev/null 2>&1; then
          fish -lc "set -Ux fish_user_paths $CDH_BIN_DIR \$fish_user_paths" || true
          ok "已为 fish 加 PATH：$CDH_BIN_DIR"
        fi
        ;;
      zsh)
        echo "export PATH=\"$CDH_BIN_DIR:\$PATH\"" >> "$HOME/.zshrc"
        ok "已写入 ~/.zshrc：PATH+=${CDH_BIN_DIR}"
        ;;
      bash|sh|*)
        echo "export PATH=\"$CDH_BIN_DIR:\$PATH\"" >> "$HOME/.bashrc"
        ok "已写入 ~/.bashrc：PATH+=${CDH_BIN_DIR}"
        ;;
    esac
  esac
}

# ---------------- Release 查找 ----------------
# 输出：VERSION ASSET_NAME ASSET_URL CHECKSUM_URL(可空)
get_asset_urls() {
  need_cmd curl
  local version="${CDH_VERSION:-}"
  local auth=()
  [[ -n "${GH_TOKEN:-}" ]] && auth=(-H "Authorization: Bearer $GH_TOKEN")

  if [[ -z "$version" ]]; then
    info "查询最新版本…"
    local j
    j="$(curl -fsSL "${auth[@]}" "https://api.github.com/repos/${REPO}/releases/latest")"
    version="$(printf '%s' "$j" | sed -n 's/ *"tag_name": *"\(v[^"]*\)".*/\1/p' | head -n1)"
    [[ -n "$version" ]] || { err "无法获取最新版本 tag"; exit 1; }
  fi
  VERSION="$version"
  ASSET_NAME="${APP}-${VERSION}-${TARGET}.tar.gz"

  info "读取 ${VERSION} 资产列表…"
  local j2
  j2="$(curl -fsSL "${auth[@]}" "https://api.github.com/repos/${REPO}/releases/tags/${VERSION}")" \
      || { err "获取 $VERSION 版本信息失败"; exit 1; }

  ASSET_URL="$(printf '%s' "$j2" | sed -n "s# *\"browser_download_url\": *\"\\(.*${ASSET_NAME}\\)\"#\\1#p" | head -n1)"
  [[ -n "${ASSET_URL:-}" ]] || { err "未找到资产：${ASSET_NAME}"; exit 1; }

  CHECKSUM_URL="$(printf '%s' "$j2" | sed -n "s# *\"browser_download_url\": *\"\\(.*${ASSET_NAME}\\.sha256\\)\"#\\1#p" | head -n1 || true)"
}

# ---------------- 下载（aria2c 优先，curl 回退） ----------------
download_asset() {
  local url="$1" out="$2"
  if command -v aria2c >/dev/null 2>&1; then
    info "使用 aria2c 并发下载"
    aria2c -x16 -s16 -k1M --console-log-level=warn \
           --file-allocation=none -o "$out" "$url" && return 0
    warn "aria2c 下载失败，回退 curl"
  fi
  info "使用 curl 下载（IPv4/HTTP1.1 + 断点续传 + 重试）"
  curl -fL --http1.1 -4 --continue-at - \
       --retry 5 --retry-delay 1 --retry-connrefused \
       -o "$out" "$url"
}

# ---------------- 校验（如果有 .sha256） ----------------
verify_checksum() {
  local file="$1" sum_url="$2"
  if [[ -z "${sum_url:-}" ]]; then
    [[ "$VERIFY_SHA" = "always" ]] && { err "未找到 .sha256；但 VERIFY_SHA=always"; exit 1; }
    warn "未找到校验文件，跳过校验（可设置 VERIFY_SHA=always 强制校验）"
    return 0
  fi
  need_cmd curl
  local sum_local="${_CDH_TMPDIR}/$(basename "$file").sha256"
  info "下载校验文件"
  curl -fL --http1.1 -4 -o "$sum_local" "$sum_url"

  if command -v sha256sum >/dev/null 2>&1; then
    ( cd "$(dirname "$file")" && sha256sum -c "$(basename "$sum_local")" )
  else
    need_cmd shasum
    local expect got
    expect="$(cut -d' ' -f1 "$sum_local")"
    got="$(shasum -a 256 "$file" | awk '{print $1}')"
    [[ "$expect" == "$got" ]] || { err "SHA256 不匹配：expect=$expect got=$got"; exit 1; }
  fi
  ok "SHA256 校验通过"
}

# ---------------- 安装二进制 ----------------
install_binary() {
  need_cmd tar
  detect_target
  get_asset_urls

  _CDH_TMPDIR="$(mktemp -d)"
  info "下载 ${ASSET_NAME}"
  download_asset "$ASSET_URL" "$_CDH_TMPDIR/$ASSET_NAME"
  verify_checksum "$_CDH_TMPDIR/$ASSET_NAME" "${CHECKSUM_URL:-}"

  info "解压到临时目录"
  tar -C "$_CDH_TMPDIR" -xzf "$_CDH_TMPDIR/$ASSET_NAME"

  local bin_path
  bin_path="$(find "$_CDH_TMPDIR" -type f -name "${APP}" -perm -111 | head -n1 || true)"
  [[ -n "$bin_path" ]] || { err "未在压缩包中找到可执行文件 ${APP}"; exit 1; }

  info "安装到 ${CDH_BIN_DIR}/${APP}"
  install -m 0755 "$bin_path" "${CDH_BIN_DIR}/${APP}"
  ok "二进制安装完成：$(command -v ${APP} || echo ${CDH_BIN_DIR}/${APP})"
}

# ---------------- Shell 集成 ----------------
install_fish_wrapper() {
  need_cmd fish
  local funcdir="${HOME}/.config/fish/functions"
  mkdir -p "$funcdir"

  # 冲突检测
  if fish -c "functions -q cdh" >/dev/null 2>&1; then
    warn "检测到 fish 已存在函数 cdh（可能与系统自带冲突）"
    if [[ "$CDH_FORCE" = "1" ]] || confirm "覆盖现有 cdh 函数以使用 Rust 版？"; then
      fish -c "functions --erase cdh" || true
    else
      warn "跳过覆盖 cdh 函数。你仍可用 'command cdh' 调用外部二进制。"
      return 0
    fi
  fi

  # 包装函数：stderr TUI，stdout 捕获后 cd（透传 --help/--version）
  cat > "${funcdir}/cdh.fish" <<'FISH'
# >>> cdh (installer marker)
function cdh -d "cd via Rust cdh (TUI)"
    set -l bin (command -v cdh)
    if not test -x "$bin"
        echo "cdh: not found" >&2
        return 127
    end
    if contains -- '--help' $argv; or contains -- '--version' $argv
        command cdh $argv
        return $status
    end
    set -l sel (command $bin $argv)
    set -l st $status
    if test $st -eq 0 -a -n "$sel"
        builtin cd -- "$sel"
    else
        return $st
    end
end
# <<< cdh (installer marker)
FISH
  ok "已安装 fish 函数：cdh（自动 cd）"
}

install_fish_logger() {
  [[ "$CDH_INSTALL_LOGGER" = "none" ]] && { warn "跳过目录日志（CDH_INSTALL_LOGGER=none）"; return 0; }
  need_cmd fish
  local funcdir="${HOME}/.config/fish/functions"
  mkdir -p "$funcdir"
  cat > "${funcdir}/cd.fish" <<'FISH'
# >>> cdh (installer marker)
functions --erase cd 2>/dev/null
function cd --wraps=cd -d "cd + log to ~/.cd_history(_raw)"
    builtin cd -- $argv; or return

    set -l now (date +%s)
    set -l raw ~/.cd_history_raw
    set -l uniq ~/.cd_history
    test -e $raw; or touch $raw
    test -e $uniq; or touch $uniq

    # 初始化默认值，避免首次使用时 math 报错
    set -l last_ts 0
    if set -q __CDH_LAST_TS
        set last_ts $__CDH_LAST_TS
    end
    set -l last_dir ""
    if set -q __CDH_LAST_DIR
        set last_dir $__CDH_LAST_DIR
    end

    # 2s 去抖：同目录且时间差 < 2 则不记录
    if test "$last_dir" = (pwd)
        and test (math "$now - $last_ts") -lt 2
        return
    end

    printf "%s\t%s\n" $now (pwd) >> $raw
    printf "%s\n" (pwd) >> $uniq

    set -g __CDH_LAST_DIR (pwd)
    set -g __CDH_LAST_TS $now
end
# <<< cdh (installer marker)
FISH
  ok "已安装 fish 日志包装（~/.cd_history_raw / ~/.cd_history）"
}

install_zsh_wrapper_and_logger() {
  local rc="$HOME/.zshrc"
  if ! grep -q "# >>> cdh (installer marker)" "$rc" 2>/dev/null; then
    cat >> "$rc" <<'ZSH'

# >>> cdh (installer marker)
__cdh_run() {
  local bin sel
  bin="$(command -v cdh)" || { echo "cdh: not found" >&2; return 127; }
  sel="$("$bin" "$@" 2>/dev/tty)"
  [ -n "$sel" ] && builtin cd -- "$sel"
}
alias cdh="__cdh_run"

__cdh_log() {
  local now raw uniq
  now="$(date +%s)"
  raw="$HOME/.cd_history_raw"
  uniq="$HOME/.cd_history"
  touch "$raw" "$uniq"
  if [ "$PWD" = "${__CDH_LAST_DIR:-}" ] && [ $(( now - ${__CDH_LAST_TS:-0} )) -lt 2 ]; then
    return
  fi
  printf "%s\t%s\n" "$now" "$PWD" >> "$raw"
  printf "%s\n" "$PWD" >> "$uniq"
  __CDH_LAST_DIR="$PWD"; __CDH_LAST_TS="$now"
}
typeset -ga chpwd_functions
chpwd_functions+=(__cdh_log)
# <<< cdh (installer marker)
ZSH
    ok "已写入 ~/.zshrc：cdh 包装 + chpwd 日志"
  else
    warn "~/.zshrc 已包含 cdh 标记，跳过重复写入"
  fi
}

install_bash_wrapper_and_logger() {
  local rc="$HOME/.bashrc"
  if ! grep -q "# >>> cdh (installer marker)" "$rc" 2>/dev/null; then
    cat >> "$rc" <<'BASH'

# >>> cdh (installer marker)
__cdh_run() {
  local bin sel
  bin="$(command -v cdh)" || { echo "cdh: not found" >&2; return 127; }
  sel="$("$bin" "$@" 2>/dev/tty)"
  [ -n "$sel" ] && builtin cd -- "$sel"
}
alias cdh="__cdh_run"

__cdh_log() {
  local now raw uniq
  now="$(date +%s)"
  raw="$HOME/.cd_history_raw"
  uniq="$HOME/.cd_history"
  mkdir -p "$(dirname "$raw")" "$(dirname "$uniq")"
  touch "$raw" "$uniq"
  if [ "$PWD" = "${__CDH_LAST_DIR:-}" ] && [ $(( now - ${__CDH_LAST_TS:-0} )) -lt 2 ]; then
    return
  fi
  printf "%s\t%s\n" "$now" "$PWD" >> "$raw"
  printf "%s\n" "$PWD" >> "$uniq"
  __CDH_LAST_DIR="$PWD"; __CDH_LAST_TS="$now"
}
if [[ -n "${PROMPT_COMMAND:-}" ]]; then
  PROMPT_COMMAND="__cdh_log;$PROMPT_COMMAND"
else
  PROMPT_COMMAND="__cdh_log"
fi
# <<< cdh (installer marker)
BASH
    ok "已写入 ~/.bashrc：cdh 包装 + 目录日志"
  else
    warn "~/.bashrc 已包含 cdh 标记，跳过重复写入"
  fi
}

install_shell_integration() {
  case "$CDH_SHELL" in
    none) warn "跳过 Shell 集成（CDH_SHELL=none）"; return 0 ;;
    all)  install_bash_wrapper_and_logger; install_zsh_wrapper_and_logger; install_fish_wrapper; install_fish_logger ;;
    auto)
      case "$SHELL_BASENAME" in
        fish) install_fish_wrapper; install_fish_logger ;;
        zsh)  install_zsh_wrapper_and_logger ;;
        bash|sh|*) install_bash_wrapper_and_logger ;;
      esac
      ;;
    fish) install_fish_wrapper; install_fish_logger ;;
    zsh)  install_zsh_wrapper_and_logger ;;
    bash) install_bash_wrapper_and_logger ;;
    *) warn "未知 CDH_SHELL=$CDH_SHELL，按 auto 处理"; CDH_SHELL=auto; install_shell_integration ;;
  esac
}

# ---------------- 卸载 ----------------
uninstall_all() {
  info "卸载 cdh 二进制与 Shell 集成"
  rm -f "${CDH_BIN_DIR}/cdh" || true

  # fish
  if [[ -f "$HOME/.config/fish/functions/cdh.fish" ]] && grep -q ">>> cdh (installer marker)" "$HOME/.config/fish/functions/cdh.fish" 2>/dev/null; then
    rm -f "$HOME/.config/fish/functions/cdh.fish" || true
    ok "已删除 fish 函数 cdh"
  fi
  if [[ -f "$HOME/.config/fish/functions/cd.fish" ]] && grep -q "cd + log to ~/.cd_history" "$HOME/.config/fish/functions/cd.fish" 2>/dev/null; then
    rm -f "$HOME/.config/fish/functions/cd.fish" || true
    ok "已删除 fish 日志包装 cd"
  fi

  # zsh/bash：按标记删除
  for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
    [[ -f "$rc" ]] || continue
    if grep -q "# >>> cdh (installer marker)" "$rc"; then
      awk '
        BEGIN{skip=0}
        /# >>> cdh \(installer marker\)/{skip=1; next}
        /# <<< cdh \(installer marker\)/{skip=0; next}
        skip==0{print}
      ' "$rc" > "$rc.tmp" && mv "$rc.tmp" "$rc"
      ok "已清理 $rc 中的 cdh 段落"
    fi
  done

  ok "卸载完成（历史数据 ~/.cd_history* 未动）"
}

post_message() {
  cat <<'TXT'
----------------------------------------
安装完成

• 新开一个终端（或手动 source rc）后可用：
    cdh             # 打开 TUI；回车选中即切换目录
    command cdh …   # 避开同名函数（若你手动保留了它）

• 历史文件：
    ~/.cd_history_raw   # timestamp<TAB>path
    ~/.cd_history       # 最近唯一目录（每行一个）
  cdh 基于这些数据进行推荐（Frecency，半衰期默认 7 天）。

• 验证：
    cdh --version
    cdh --help
----------------------------------------
TXT
}

# ---------------- 参数解析 ----------------
while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) CDH_VERSION="$2"; shift 2 ;;
    --bin-dir) CDH_BIN_DIR="$2"; shift 2 ;;
    --shell)   CDH_SHELL="$2"; shift 2 ;;
    --logger)  CDH_INSTALL_LOGGER="$2"; shift 2 ;;
    --verify)  VERIFY_SHA="$2"; shift 2 ;;   # auto|always|never
    -y|--yes)  CDH_YES=1; shift ;;
    --force)   CDH_FORCE=1; shift ;;
    --uninstall) DO_UNINSTALL=1; shift ;;
    *) err "未知参数：$1"; exit 2 ;;
  esac
done

# ---------------- 主流程 ----------------
main() {
  if [[ "$DO_UNINSTALL" = "1" ]]; then
    uninstall_all
    exit 0
  fi
  ensure_bindir
  install_binary
  install_shell_integration
  post_message
}

main "$@"
