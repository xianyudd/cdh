set -g __CDH_LAST_DIR ""
set -g __CDH_LAST_TS 0

function __cdh_resolve_bin
    if test -n "$CDH_BIN" -a -x "$CDH_BIN"
        echo "$CDH_BIN"
        return 0
    end

    set -l bin (type -P cdh 2>/dev/null)
    if test -n "$bin" -a -x "$bin"
        echo "$bin"
        return 0
    end

    if test -x "$HOME/.local/bin/cdh"
        echo "$HOME/.local/bin/cdh"
        return 0
    end

    return 1
end

function __cdh_log --on-variable PWD
    set -l now (date +%s)
    if test -z "$PWD"
        return
    end
    if test "$PWD" = "$__CDH_LAST_DIR" -a (math "$now - $__CDH_LAST_TS") -lt 2
        return
    end

    set -l bin (__cdh_resolve_bin)
    or return

    $bin log --dir "$PWD" >/dev/null 2>/dev/null
    set -g __CDH_LAST_DIR "$PWD"
    set -g __CDH_LAST_TS "$now"
end
