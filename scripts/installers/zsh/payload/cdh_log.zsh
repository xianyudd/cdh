# cdh_log.zsh - zsh hook for logging directory changes
# 每次 cd 时往 ~/.cd_history_raw 追加一行：<epoch>\t<path>

: "${CDH_HISTORY_FILE:=$HOME/.cd_history_raw}"

typeset -g CDH_LOG_LAST_TS=0

_cdh_log_dir_change() {
  [[ -z "$CDH_HISTORY_FILE" ]] && return 0

  local now
  now=$(date +%s 2>/dev/null || printf '%s\n' "$EPOCHSECONDS")

  # 2 秒去抖
  if (( CDH_LOG_LAST_TS > 0 && now - CDH_LOG_LAST_TS < 2 )); then
    return 0
  fi

  CDH_LOG_LAST_TS=$now

  {
    # 与 README 中 fish 行为保持一致：epoch\tpath
    printf '%s\t%s\n' "$now" "$PWD"
  } >>"$CDH_HISTORY_FILE" 2>/dev/null || true
}

# 用 chpwd_functions 注册 hook，避免覆写用户自己的 chpwd
typeset -ga chpwd_functions
if [[ "${chpwd_functions[(Ie)_cdh_log_dir_change]}" -eq 0 ]]; then
  chpwd_functions+=(_cdh_log_dir_change)
fi

# 启用时先记录一次当前目录
_cdh_log_dir_change

