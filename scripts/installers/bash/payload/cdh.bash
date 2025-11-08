# ~/.config/cdh/bash/cdh.bash
cdh() {
  local bin="" sel st raw="$HOME/.cd_history_raw"

  if [ -n "${CDH_BIN:-}" ] && [ -x "$CDH_BIN" ]; then
    bin="$CDH_BIN"
  else
    # 只找外部可执行，避免命中函数/alias 递归
    bin="$(type -P cdh 2>/dev/null || true)"
    [ -z "$bin" ] && [ -x "$HOME/.local/bin/cdh" ] && bin="$HOME/.local/bin/cdh"
  fi

  if [ -z "$bin" ]; then
    echo "cdh: 找不到外部二进制。请先安装到 PATH，或设置 CDH_BIN。" >&2
    echo "示例（统一入口）：" >&2
    echo "  curl -fsSL https://raw.githubusercontent.com/xianyudd/cdh/main/scripts/install.sh | bash --noprofile --norc" >&2
    return 127
  fi

  if [ ! -s "$raw" ]; then
    echo "cdh: 暂无历史可供推荐。" >&2
    echo "提示：先切换几个目录后再试，例如：" >&2
    echo "  cd ~/projects; cd ~; cd /etc; cd ~; cdh" >&2
    return 0
  fi

  sel="$("$bin" "$@")"; st=$?
  case "$st" in
    0) [ -n "$sel" ] && builtin cd -- "$sel" ;;
    1) return 0 ;;
    2) echo "cdh: 未匹配到目录（可尝试输入关键字）" >&2; return 2 ;;
    *) echo "cdh: 执行错误（退出码 $st）" >&2; return "$st" ;;
  esac
}
