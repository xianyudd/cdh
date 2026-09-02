#!/usr/bin/env bash
set -euo pipefail

msg_file="${1:-}"
if [[ -z "$msg_file" || ! -f "$msg_file" ]]; then
  echo "commit-msg-check: missing commit message file" >&2
  exit 1
fi

subject="$(sed -n '/^[[:space:]]*#/d;/^[[:space:]]*$/d;p;q' "$msg_file" | tr -d '\r')"
if [[ -z "$subject" ]]; then
  echo "提交信息不能为空。" >&2
  exit 1
fi

type_re='feat|fix|docs|refactor|test|chore|ci|perf'
scope_re='install|bash|fish|zsh|history|paths|recommend|controller|readme|release|ci|tui'
pattern="^(${type_re})(\\((${scope_re})\\))?: .+"

if ! printf '%s' "$subject" | grep -Eq "$pattern"; then
  cat >&2 << 'EOF'
提交信息格式不符合约定。

期望格式：
  <type>(<scope>): <summary>
  或
  <type>: <summary>

允许的 type（白名单，不在表里一律拒）：
  feat, fix, docs, refactor, test, chore, ci, perf

允许的 scope（白名单，不在表里一律拒；scope 可以整段省略，`<type>: <summary>` 是合法的）：
  install, bash, fish, zsh, history, paths, recommend, controller, readme, release, ci, tui

注意：改 docs/roadmap.md 这类文件时 `docs(roadmap):` 会被拒（roadmap 不在表里），
写无 scope 的 `docs:` 才通过。

示例：
  feat(history): maintain a uniq visit history on directory access
  fix(install): record shell history through cdh log consistently
  chore(release): prepare v0.2.0
EOF
  exit 1
fi

# 标题长度用**显示宽度**卡，而且刻意不依赖调用者的 locale。
#
# 原来写的是 `${#subject}`：它在 UTF-8 locale 下数字符、在 LC_ALL=C 下数字节，
# 同一条中文标题在两种环境里结论不同。钩子跑在别人的机器上，locale 不由我们决定，
# 这种不确定性必须消掉。
#
# 三种度量里选显示宽度，因为 72 这个数字的来历就是「80 列终端里 git log 不折行」：
#   - 数字节：一个汉字算 3，72 字节只剩 24 个汉字，比 72 列严了一半，还会追认拒掉
#     历史上 4 条并无问题的标题；
#   - 数字符：72 个汉字 ≈ 144 显示列，对以中文标题为主的本仓等于把长度检查关掉 ——
#     只会说 PASS 的检查等于没有检查；
#   - 数显示宽度：ASCII 1 列、中日韩 2 列，正是 72 的原意，也与本仓 TUI 那边把
#     Unicode 显示宽度当一等公民的做法一致。
#
# 显示宽度没法便携地直接测（GNU 的 `wc -L` 在 macOS 的 BSD wc 上不存在），用 UTF-8
# 的字节数与字符数估算：把多字节字符一律当成 3 字节 / 2 列，于是
#   width = chars + (bytes - chars) / 2 = (bytes + chars) / 2
# 对「ASCII + 中日韩」这种实际情形是精确的；对 2 字节字符（西里尔等）和 3 字节的
# 窄符号会**高估**，也就是偏严 —— 偏严可以接受，偏松才是这次要修的方向。
#
# 两个计数都在 LC_ALL=C 下做，与外部 locale 无关：`wc -c` 本来就数字节；UTF-8 的
# 字符数等于非续接字节的个数，所以先用 tr 删掉续接字节（0x80-0xBF）再数。
subject_bytes="$(printf '%s' "$subject" | LC_ALL=C wc -c | LC_ALL=C tr -d '[:space:]')"
subject_chars="$(printf '%s' "$subject" | LC_ALL=C tr -d '\200-\277' | LC_ALL=C wc -c | LC_ALL=C tr -d '[:space:]')"
subject_width=$(((subject_bytes + subject_chars) / 2))

if [[ ${subject_width} -gt 72 ]]; then
  echo "提交标题过长（约 ${subject_width} 显示列，上限 72 列）。" >&2
  echo "计量方式：ASCII 每字符算 1 列，中日韩字符每字符算 2 列，所以 72 列约等于 36 个汉字。" >&2
  exit 1
fi

# Subject 只允许可打印 ASCII（英文）。bytes 与 chars 都在 LC_ALL=C 下取得：纯 ASCII
# 时两者相等；一旦出现多字节字符（中文、emoji、全角标点等），bytes 必然大于 chars。
# 这个判等与 locale 无关，也不会把控制字符误放进来——多字节判等只拦非 ASCII，
# 控制字符等奇怪输入本来就无法通过上面的格式正则。
# 2026-09-03 用户裁定：subject 一律英文；此前历史中的中文标题保留不改写。
if ((subject_bytes != subject_chars)); then
  echo "提交标题（subject）必须是可打印 ASCII（英文），当前含有非 ASCII 字符。" >&2
  echo "说明：type/scope 本就是英文，主体混入中文会让 git log --grep 与 GitHub 检索失焦；" >&2
  echo "正文（body）语言不限，中文详述 encouraged。2026-09-03 前的中文标题历史保留不改。" >&2
  exit 1
fi

case "$subject" in
  *'.' | *'。' | *'!' | *'！' | *'?' | *'？')
    echo "提交标题末尾不要添加句号或感叹号等标点。" >&2
    exit 1
    ;;
esac

exit 0
