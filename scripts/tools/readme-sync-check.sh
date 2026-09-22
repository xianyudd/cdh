#!/usr/bin/env bash
set -euo pipefail

# README 中英同步（touch-parity）校验。守的是 roadmap.md:577 记下的那笔债：
# README.md（中文）与 README.en.md（英文）近乎全译，但中英不同步「没有任何检查会
# 报出来」，漂移是沉默的。这个脚本把该漂移变「响」。
#
# 刻意只做 touch-parity —— 只问「两个文件是不是都在本次 PR 的改动里」，不做语义
# 等价。原因：语义是否真的同步（译文有没有跟上原文的意思）无法自动判定，硬做只会
# 得到一个不断误报、最终被无视的检查；而「改了一个忘了另一个」是最常见、也最容易
# 被自动抓住的那类漏译，抓住它已经能拦下绝大多数沉默漂移。语义等价不是本次范围。

# 逃生舱：确有「本次改动只适用于单一语言」的正当情形（比如只修中文原文里的一个错别
# 字、或补一段只对中文读者有意义的说明）。这种时候在 PR 描述里写一行 readme-sync: skip
# 明确声明「我知道只改了一个，是有意的」，检查就放行。用不区分大小写的匹配，容忍
# 大小写与冒号后空格的写法差异。
if printf '%s' "${PR_BODY:-}" | grep -Eiq 'readme-sync:[[:space:]]*skip'; then
  echo "检测到 readme-sync: skip 标记，跳过 README 同步校验"
  exit 0
fi

# 用三点 diff（base...head）而不是两点：三点会自动以 base 与 head 的 merge-base 为
# 基准，只算 head 分支自己引入的改动。两点（base..head 或直接比较两个 sha）会把 base
# 之后 main 上推进的、与本 PR 无关的提交也算进来 —— 那些无关改动可能恰好动过某个
# README，从而让本检查得出错误结论（误判成「改了」或「没改」）。
changed="$(git diff --name-only "${BASE_SHA}...${HEAD_SHA}")"

# 用 grep -qx 做整行精确匹配，不用子串匹配：README.md 是 README.en.md 的子串，
# 子串匹配会把只改了英文版误判成两个都改了，正好放过要抓的那类漏译。
zh_changed=false
en_changed=false
if printf '%s\n' "$changed" | grep -qx 'README.md'; then
  zh_changed=true
fi
if printf '%s\n' "$changed" | grep -qx 'README.en.md'; then
  en_changed=true
fi

# 只在「异或」（恰好改了一个）时报错。两个都改＝同步意图已在场，放行；两个都没改＝
# 本次 PR 与 README 无关，也放行。检查只关心「改一个漏一个」这一种失衡状态。
if [[ "$zh_changed" == true && "$en_changed" == false ]]; then
  echo "::error::README.md（中文）改了，但 README.en.md（英文）没改。这两份是近乎全译的双语文档，通常要一起更新。若本次改动确实只适用于单一语言，在 PR 描述里加一行 readme-sync: skip"
  exit 1
fi
if [[ "$zh_changed" == false && "$en_changed" == true ]]; then
  echo "::error::README.en.md（英文）改了，但 README.md（中文）没改。这两份是近乎全译的双语文档，通常要一起更新。若本次改动确实只适用于单一语言，在 PR 描述里加一行 readme-sync: skip"
  exit 1
fi

echo "README 中英同步校验通过"
exit 0
