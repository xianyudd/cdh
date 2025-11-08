set -g __CDH_LAST_DIR ""
set -g __CDH_LAST_TS 0

function __cdh_log --on-variable PWD
    set -l raw "$HOME/.cd_history_raw"
    set -l now (date +%s)
    if test -z "$PWD"
        return
    end
    if test "$PWD" = "$__CDH_LAST_DIR" -a (math "$now - $__CDH_LAST_TS") -lt 2
        return
    end
    test -e "$raw"; or touch "$raw"
    printf "%s\t%s\n" "$now" "$PWD" >> "$raw"
    set -g __CDH_LAST_DIR "$PWD"
    set -g __CDH_LAST_TS "$now"
end
