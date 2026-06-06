# Shell Path Migration Issue

## Summary

在本地项目目录重组之后，`cdh` 的 shell 集成可能因为旧的 `CDH_BIN` 和旧历史路径而失效。

这个问题在 fish 上已经实际出现过：

- `CDH_BIN` 仍然指向旧路径 `/home/jason/cdh/target/debug/cdh`
- 仓库真实路径已经迁移到 `/home/jason/workspace/repos/github.com/xianyudd/cdh`
- `cdh` 历史里还保留了大量旧目录，例如：
  - `/home/jason/cdh`
  - `/home/jason/aha`
  - `/home/jason/rust-demo`
  - `/home/jason/Pumpkins`
  - `/home/jason/workspace/go`
  - `/home/jason/workspace/python`
  - `/home/jason/workspace/wordlist-pipeline`

## Symptoms

- 在 fish 中执行 `cdh` 时，shell wrapper 优先读取 `CDH_BIN`
- 如果 `CDH_BIN` 指向的二进制已经不存在，`cdh` 无法正常工作
- 即使二进制还能运行，推荐历史里仍会混入已经搬迁或删除的旧路径

## Root Cause

问题分成两层：

1. shell wrapper / log hook 对 `CDH_BIN` 采用了“只要非空就直接使用”的策略，没有检查它是否仍然可执行
2. `history_raw` / `history_uniq` 没有提供“目录迁移后批量修复”的机制，导致工作区重构后历史长期残留失效路径

## Reproduction

一个稳定复现方式：

1. 在 fish 中设置 `CDH_BIN` 指向某个仓库内的 debug 二进制
2. 搬迁仓库目录
3. 不更新 `CDH_BIN`
4. 重新打开 shell，执行 `cdh`

此时 wrapper 会优先尝试旧二进制路径，而不是回退到 PATH 中可用的 `cdh`

## Expected Behavior

- 如果 `CDH_BIN` 不存在或不可执行，shell wrapper 应自动回退到：
  - `command -v cdh`
  - 或 `~/.local/bin/cdh`
- 历史文件应至少提供一种可维护方式，避免目录迁移后长期残留失效路径

## Proposed Fixes

### Short Term

1. 更新 fish / bash / zsh 的 wrapper 和 log hook：
   - 只有在 `CDH_BIN` 可执行时才使用
   - 否则自动回退到 PATH 中的 `cdh`
2. 安装脚本或升级脚本在检测到旧 `CDH_BIN` 时，提示用户修复

### Medium Term

1. 增加一个历史修复命令，例如：
   - `cdh doctor`
   - 或 `cdh history rewrite`
2. 支持：
   - 清理不存在的路径
   - 批量替换旧前缀到新前缀
   - 输出修复前后的统计信息

### Optional Improvement

如果 `CDH_BIN` 存在但不可执行，wrapper 可以输出一次性警告，帮助用户理解为什么发生了回退

## Suggested Tests

- shell payload 行为测试：
  - `CDH_BIN` 为有效路径时正常工作
  - `CDH_BIN` 为不存在路径时会回退到 PATH
  - `CDH_BIN` 为空时会走 PATH
- 历史迁移测试：
  - 给定一批旧路径，能正确按映射表改写
  - 不存在目录能被过滤或标记

## Notes

这次问题是在本机目录整理过程中暴露出来的，本地已经通过手工修正 shell 配置和历史文件规避，但仓库本身仍然值得补一个正式修复。
