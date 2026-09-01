# 更新日志

本文件把 `scripts/release_notes/` 下历代 release notes 倒序汇总到一处，作为查看历史变更的入口。
格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循
[语义化版本](https://semver.org/lang/zh-CN/)。

各版本条目沿用当时 release notes 的原文与分节，未重写、未翻译，因此小节标题不套用
Keep a Changelog 的 Added / Fixed 固定分类。只省略了每篇末尾两节：「🧰 构建支持」13 篇
逐字相同（Linux `x86_64-unknown-linux-gnu`、macOS `x86_64-apple-darwin` /
`aarch64-apple-darwin`），「💡 安装方式」只是把当时的安装命令按 tag 复写一遍，当前安装
方式看 README 即可。原始单篇 notes 在 `scripts/release_notes/<tag>.md`，它同时被
`.github/workflows/release.yml` 用作 GitHub Release 正文，所以发布新版本时先写那份文件，
再把其中的变更部分追加到本文件顶部。

记录从 `v0.1.1` 起：那是首个公开发布版本，`scripts/release_notes/` 里没有更早的条目。

## [v0.3.2] - 2026-08-31

修复版，外加一个降噪开关。两条修复都是 `v0.3.1` 里就存在的、能在正常使用中撞到的
问题——其中大写搜索会让 TUI 直接崩掉，建议装了 `v0.3.1` 的都升一下。

### 🐛 修复

* 💥 **含大写字母的搜索会崩掉 TUI。** 查询长度 ≥2 且含大写字母时（`VS`、`Jd`、
  `Do` 这类），只要撞上特定候选路径就 panic 退出——是否触发取决于当前列表里有哪些路径，
  所以表现为「偶发」。高亮走的 `Matcher::fuzzy_indices` 要求 needle 预先小写化，
  而未小写化的大写 needle 能通过 prefilter，随后触发 nucleo 内部
  `should have been caught by prefilter` 断言，在渲染中途 abort。
  全小写查询、以及只有单个字符的查询不受影响。
  改为复用过滤器同一个 `Pattern::parse`，顺带让高亮的大小写与模糊语法和过滤结果对齐。
* 🔤 **Emoji ZWJ 序列会被光标切断。** 搜索框里的 `👩‍💻`、`👨‍👩‍👧‍👦` 这类由零宽连接符
  拼成的字形，此前按 `char` 移动光标和截断，会从中间断开并留下孤立的 U+200D，
  于是显示宽度和终端实际渲染不一致，输入区跟着溢出。现在按字形簇（grapheme cluster）
  走。组合符（`é`）和区域指示符（`🇨🇳`）此前是碰巧安全的，ZWJ 序列不是。

### ✨ 新增

* 🙈 **`Ctrl+H` / `F5` 临时过滤隐藏目录。** 一键隐藏路径里含隐藏目录段的候选
  （`~/.cache/pip`、`~/.git` 这类），再按一次全部回来。只作用于本次会话，不写任何文件——
  长期降噪仍然用 `Ctrl+D` 排除清单。判定按路径分段做，只看 `/` 分出的每一段是否以 `.`
  开头，所以 `~/workspace/dot.config` 不会被误判；WSL 上 `/mnt` 打头的路径同样准确。
  部分终端把 `Ctrl+H` 编码成 ASCII BS，这一路（`Ctrl+Backspace`）也一并接受。

## [v0.3.1] - 2026-07-27

修复版。`v0.3.0` 里新加的排除清单有一条路径是坏的，这一版专门修它。

### 🐛 修复

* ↩️ **同一会话内取消排除拿不回目录。** `Ctrl+D` 排除一个子树后，在 `F4` 面板里
  马上撤销，该子树不会回到候选池——补扫重新发出的每条路径都被自己的陈旧指纹去重掉了。
  跨会话撤销（重启后再撤）不受影响，坏掉的恰恰是「手滑排错了、马上撤」这条最常走的路。
* 🔇 **`CDH_DISCOVER=0` 时的误导文案。** 发现层关闭时没有补扫可跑，取消排除不再宣称
  「正在找回该目录」，改为说明重启后可见。
* 📐 **排除面板在极小终端下页脚压住条目。** 高度不足时优先保留条目行，丢掉按键提示。

### 🧹 内部

* 移除 `App::excludes_path` 死字段。
* 已知边界补记：补扫的候选上限是每次扫描独立的，反复取消排除可以让候选池超过 50,000。

## [v0.3.0] - 2026-07-27

自 `v0.2.8` 起累积了 34 个提交、15 个特性都没有发过版 —— 期间 TUI 基本重写了一遍。
这一版把它们一次性放出来，版本号也从补丁位提到次版本位，以匹配实际变化幅度。

### ✨ 候选池：从「去过的目录」变成「历史 ∪ 目录树」

* 🌲 **目录树发现层：** 后台流式扫描目录树（历史目录的兄弟、`$HOME` 全树、`$HOME` 外锚点），
  **没 cd 过的目录现在也能被模糊搜索命中**。分层 BFS + 剪枝 + 时间/数量双预算，
  实测本机候选从 47,048 涨到 50,074。总开关 `CDH_DISCOVER=0`。
* 🎯 **排序不被稀释：** 发现层候选排在同模糊分的历史候选之后，不给它们伪造 frecency。
* 🌱 **空历史也能用：** 没有任何历史时照常打开界面，先从 `$PWD` 的祖先链自举。

### 🧹 降噪：`Ctrl+D` 现在是「排除」

* 🚫 **子树排除：** `Ctrl+D` 排除该目录**及其全部子目录**，写入 `DATA/cdh/excludes`（行式路径，可手工编辑）。
  排除的子树连 `read_dir` 都不会发生，省的是 I/O 而不只是显示。
* ↩️ **`F4` 排除清单面板：** 上下选择、`Ctrl+D` 取消排除，取消后该子树**当场**补扫回来，不用重启。
* 🔍 **`CDH_IGNORE_RE` 现在也作用于发现层**（此前它只过滤历史候选）。

### 🖼️ 界面

* 👁️ **目录预览面板**（`Tab` 切换），异步加载，不阻塞按键。
* ⚙️ **`F2` 设置面板：** 语言、启动预览、颜色、鼠标，持久化到 `$XDG_CONFIG_HOME/cdh/tui.toml`。
* 🌏 **中英双语**，跟随 locale，可用 `CDH_LANG` 覆盖。
* 🎨 **主题与扁平化重构**，`F3` / `Ctrl+T` 循环切换。
* 🧊 **右下角环境 3D 线框立方体**（仅彩色模式，`CDH_CORNER_3D=0` 关闭）。
* 🔀 **git 状态**：预览里显示分支与干净/已修改。
* 🖱️ 鼠标单击选中、双击跳转、滚轮滚动；分页、Home/End、Unicode 光标编辑等一并补齐。

### 🧮 排序与历史

* 📊 **多信号融合排序**（frecency / uniq / recency / context），解决 `$HOME` 长期霸榜。
* 🕐 **时间戳归一化**：毫秒 / 微秒 / 纳秒历史记录都能正确解析。
* 🧽 TUI 内清理失效路径；`--ignore-re`、评分明细等 CLI 能力。

### 🐛 值得一提的修复

* 挂载根自身（如 `/mnt`）此前不被判为慢挂载，会让 9p 的 `read_dir` 进到本地专用的防饥饿阶段。
* 立方体曾覆盖列表行、破坏选中条。
* git 状态探测超时后不再留下僵尸进程。
* 帮助浮层截断、安装器管道执行、子安装器路径等。

## [v0.2.8] - 2026-06-07

### 🧩 更新内容

* 🐚 **修复：** bash / fish 子安装器现在正确继承发布包内的本地 `RAW_BASE`，不会在包内安装时再访问 `raw.githubusercontent.com`。
* 🧪 **验证：** release tarball 安装路径可用于 raw GitHub 不稳定的 VM 环境。

## [v0.2.7] - 2026-06-07

### 🧩 更新内容

* 📦 **改进：** GitHub release tarball 现在包含完整 shell installer payload。
* 🧰 **新增：** 支持从 release 包内运行 `install.sh`，无需再访问 `raw.githubusercontent.com` 下载子安装器。
* 📝 **更新：** README 增加 release tarball 安装方式，适合 raw GitHub 不稳定的 VM 环境。

## [v0.2.6] - 2026-06-07

### 🧩 更新内容

* 🌐 **改进：** bash / zsh / fish 子安装器下载 payload 时使用统一的慢网重试与超时策略。
* 🧪 **验证：** 防止二进制已安装但 shell 集成 payload 下载失败导致半安装。

## [v0.2.5] - 2026-06-07

### 🧩 更新内容

* 🌐 **改进：** README 一键安装的下载重试与超时更适合慢速 VM 网络，避免 release asset 下载中途超时。
* 🧪 **验证：** 下载失败仍会中止安装，避免写入半成品 shell 集成。

## [v0.2.4] - 2026-06-07

### 🧩 更新内容

* 🧹 **修复：** README 一键安装在无法解析 GitHub latest release 时会中止并提示 `CDH_VERSION`，不再隐式回落到旧版本。
* 🧪 **验证：** 避免慢网络或 IPv6 异常时误安装过期 release。

## [v0.2.3] - 2026-06-07

### 🧩 更新内容

* 🧹 **修复：** README 一键安装在二进制下载失败时会立即中止，不再继续写入 shell 集成。
* 🌐 **改进：** GitHub latest release 解析和资产下载使用更宽松超时与重试，降低慢网络下误回落旧版本的概率。
* 🧪 **验证：** 继续使用 GitHub release 资产作为真实安装来源。

## [v0.2.2] - 2026-06-07

### 🧩 更新内容

* 🧹 **修复：** 远程卸载命令在无 TTY 环境下可正常输出并完成清理。
* 🧪 **验证：** 保持 README 一键安装入口，安装流程继续解析 GitHub 最新 release 并下载对应二进制。

## [v0.2.1] - 2026-06-07

### 🧩 更新内容

* 🐚 **修复：** fish / zsh wrapper 在 `CDH_BIN` 指向失效路径时会回退到 PATH 中的外部 `cdh`。
* 🐚 **修复：** zsh fallback 只查找外部可执行文件，避免命中当前 `cdh()` 函数导致递归。
* 🧪 **修复：** `--half-life 0` 或负数现在返回明确错误，不再触发进程 abort。
* 🧹 **修复：** bash wrapper 对 `cdh --version` 这类成功但不输出目录的命令返回 0。
* 🧹 **修复：** bash uninstall 能正确移除 `.bashrc` 中的 installer 标记块。
* 📝 **新增：** 添加仓库协作指南 `AGENTS.md`。

## [v0.2.0] - 2026-03-10

### 🧩 更新内容

* ✨ **新增：** 引入统一的历史记录子系统，`cdh log` 现在会同时维护 `history_raw` 与 `history_uniq`。
* 🧭 **改进：** 目录访问路径会在写入历史前规范化为绝对路径，减少重复记录和推荐偏差。
* 🔒 **增强：** 加强历史写入时的锁处理、短重试与过期锁清理，降低并发写入失败概率。
* 🐚 **调整：** bash / fish / zsh 的 shell hook 统一通过 `cdh log --dir "$PWD"` 记录历史，并对齐 XDG 历史目录。
* 📝 **完善：** 更新 README、卸载逻辑与提交信息规范，补充本地 commit-msg 校验脚本。

## [v0.1.1] - 2025-11-07

### 🧩 更新内容

* ✨ **新增：** `install.sh` 安装脚本，支持 Linux / macOS / fish / zsh / bash 的一键安装与 shell 集成。
* 🧭 **改进：** TUI（基于 stderr 输出界面，stdout 仅输出目录路径），更好兼容 shell 自动 cd。
* ⚙️ **优化：** Frecency 算法调优，使常用目录排序更智能；增强 Controller 模块稳定性。
* 📦 **完善：** 新增 LICENSE、README、CI workflow、打包与自动发布逻辑。

[v0.3.2]: https://github.com/xianyudd/cdh/compare/v0.3.1...v0.3.2
[v0.3.1]: https://github.com/xianyudd/cdh/compare/v0.3.0...v0.3.1
[v0.3.0]: https://github.com/xianyudd/cdh/compare/v0.2.8...v0.3.0
[v0.2.8]: https://github.com/xianyudd/cdh/compare/v0.2.7...v0.2.8
[v0.2.7]: https://github.com/xianyudd/cdh/compare/v0.2.6...v0.2.7
[v0.2.6]: https://github.com/xianyudd/cdh/compare/v0.2.5...v0.2.6
[v0.2.5]: https://github.com/xianyudd/cdh/compare/v0.2.4...v0.2.5
[v0.2.4]: https://github.com/xianyudd/cdh/compare/v0.2.3...v0.2.4
[v0.2.3]: https://github.com/xianyudd/cdh/compare/v0.2.2...v0.2.3
[v0.2.2]: https://github.com/xianyudd/cdh/compare/v0.2.1...v0.2.2
[v0.2.1]: https://github.com/xianyudd/cdh/compare/v0.2.0...v0.2.1
[v0.2.0]: https://github.com/xianyudd/cdh/compare/v0.1.1...v0.2.0
[v0.1.1]: https://github.com/xianyudd/cdh/releases/tag/v0.1.1
