# ~/.config/cdh/bash/cdh_log.bash
: "${__CDH_LAST_DIR:=}"
: "${__CDH_LAST_TS:=0}"

__cdh_log() {
  local raw="$HOME/.cd_history_raw" now cur
  now="$(date +%s 2>/dev/null)" || return 0
  cur="$PWD"; [ -z "$cur" ] && return 0
  # 2 秒去抖
  if [ "$cur" = "$__CDH_LAST_DIR" ] && [ $(( now - __CDH_LAST_TS )) -lt 2 ]; then
    return 0
  fi
  [ -e "$raw" ] || : > "$raw"
  printf '%s\t%s\n' "$now" "$cur" >> "$raw" 2>/dev/null || true
  __CDH_LAST_DIR="$cur"
  __CDH_LAST_TS="$now"
}

# 挂到 PROMPT_COMMAND（不导出）
if [ -n "${PROMPT_COMMAND:-}" ]; then
  case ";$PROMPT_COMMAND;" in
    *";__cdh_log;"*) : ;;
    *) PROMPT_COMMAND="__cdh_log; ${PROMPT_COMMAND}" ;;
  esac
else
  PROMPT_COMMAND="__cdh_log"
fi
