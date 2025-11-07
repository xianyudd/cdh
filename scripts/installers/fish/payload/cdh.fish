function cdh -d "智能 cd 历史（Rust 版 TUI）"
    set -l bin ""
    if test -n "$CDH_BIN"
        set bin "$CDH_BIN"
    else if command -sq cdh
        set bin (command -v cdh)
    else if test -x "$HOME/.local/bin/cdh"
        set bin "$HOME/.local/bin/cdh"
    end

    if test -z "$bin"
        echo "cdh: 找不到外部二进制。请先安装到 PATH，或设置 CDH_BIN。" >&2
        echo "示例：curl -fsSL https://raw.githubusercontent.com/xianyudd/cdh/main/scripts/install.sh | bash" >&2
        return 127
    end

    set -l raw "$HOME/.cd_history_raw"
    if not test -s "$raw"
        echo "cdh: 暂无历史可供推荐。" >&2
        echo "提示：先切换几个目录再试，例如：" >&2
        echo "  cd ~/projects; cd ~; cd /etc; cd ~; 然后执行：cdh" >&2
        return 0
    end

    set -l sel ( $bin $argv )
    set -l st $status

    switch $st
        case 0
            if test -n "$sel"
                builtin cd -- "$sel"
            end
        case 1
            return 0
        case 2
            echo "cdh: 未匹配到目录（可尝试输入关键字）" >&2
            return 2
        case "*"
            echo "cdh: 执行错误（退出码 $st）" >&2
            return $st
    end
end
