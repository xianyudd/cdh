# cdh — Frecency 驱动的目录跳转（含 TUI）

**中文** | [English](./README.en.md)

`cdh` 融合“访问频次 × 时间衰减 + 最近性 + 当前目录上下文”对历史目录多信号打分，提供一个终端 TUI，让你按分数排序快速选择并跳转。

> 当前已支持 **fish / bash / zsh** 的安装与卸载集成。

* 仓库地址：[https://github.com/xianyudd/cdh](https://github.com/xianyudd/cdh)
* 安装脚本：[https://xianyudd.github.io/cdh/install.sh](https://xianyudd.github.io/cdh/install.sh)

---

## 快速安装与卸载

### 一键安装（自动识别当前 Shell）

```bash
curl -fsSL https://xianyudd.github.io/cdh/install.sh | bash
```

> 使用短链接前，需要在 GitHub Pages 中启用 `main` 分支的 `/docs` 目录。未启用时可临时使用 `https://raw.githubusercontent.com/xianyudd/cdh/main/scripts/install.sh`。

安装器会根据 `$SHELL` 自动安装到 fish / bash / zsh。安装完成后执行对应命令重新加载：

```bash
# fish
exec fish -l

# bash
exec bash -l

# zsh
exec zsh -l
```

也可以手动指定 shell，或强制进入交互选择：

```bash
curl -fsSL https://xianyudd.github.io/cdh/install.sh | bash -s -- --shell zsh
curl -fsSL https://xianyudd.github.io/cdh/install.sh | bash -s -- --interactive
```

如果所在网络访问 GitHub latest 跳转不稳定，可以固定安装指定版本：

```bash
curl -fsSL https://xianyudd.github.io/cdh/install.sh \
  | CDH_VERSION=v0.2.8 bash
```

也可以直接使用 GitHub release 打包产物安装，适合 `raw.githubusercontent.com` 不稳定但 release asset 可下载的环境：

```bash
curl -fsSL https://github.com/xianyudd/cdh/releases/download/v0.2.8/cdh-v0.2.8-x86_64-unknown-linux-gnu.tar.gz \
  | tar -xz
cd cdh-v0.2.8-x86_64-unknown-linux-gnu
bash --noprofile --norc install.sh
```

也可以本地调试安装脚本（在仓库根目录）：

```bash
bash --noprofile --norc scripts/install.sh
```

### 一键卸载

远程卸载（自动清理 shell 集成 + 二进制 + 历史文件）：

```bash
curl -fsSL https://xianyudd.github.io/cdh/install.sh | bash -s -- --action uninstall
```

本地卸载：

```bash
bash --noprofile --norc scripts/install.sh --action uninstall
```

---

## 使用说明

### 日志采集

安装完成后，各 shell 通过轻量级 hook 调用：

```bash
cdh log --dir "$PWD"
```

由 `cdh` 二进制统一把目录访问写入 XDG 历史目录：

* `DATA/history/history_raw`
* `DATA/history/history_uniq`

其中：

* `DATA = ${XDG_DATA_HOME:-$HOME/.local/share}/cdh`
* `STATE = ${XDG_STATE_HOME:-$HOME/.local/state}/cdh`

各 shell 的挂载方式：

* fish：`cdh_log.fish` 通过 fish 的 hook 挂载；
* bash：`cdh_log.bash` 通过 `PROMPT_COMMAND` 挂载；
* zsh：`cdh_log.zsh` 通过 `chpwd_functions` 挂载。

`history_raw` 每一行形如：

```text
<TIMESTAMP>\t<ABS_PATH>
```

例如：

```text
1763319252	/tmp
1763319270	/home/tester/cdh
```

### 基本用法

在 shell 里直接敲：

```bash
cdh
```

默认行为：

* 从 XDG 历史目录中的 `history_raw` 与 `history_uniq` 读取历史；
* 按融合分（频次 + 最近性 + 上下文 + 最近唯一）打分并排序；
* 启动一个 TUI 列表供你选择目录；
* 选择后，shell 包装函数会 `cd` 到该目录。

### TUI 操作

打开后是一个紧凑、键盘优先的目录选择器：首行显示当前结果范围和页码，第二行直接输入
fzf 风格的模糊搜索，只渲染当前页。每行显示完整路径，Home 目录缩写为 `~`；路径前缀弱化，
末级目录加粗。失效历史在中文界面标记为 `失效`、英文界面标记为 `missing`，可在确认后删除。
搜索命中只改变文字前景和字重，不会打断选中行背景。TUI 会按 `LC_ALL`、`LC_MESSAGES`、
`LANG` 自动选择中文或英文，也可用 `CDH_LANG` 强制指定。

键位：

| 按键 | 作用 |
| --- | --- |
| 任意字符 | 在搜索光标位置插入并重新过滤 |
| `↑` / `↓`、`Ctrl+P` / `Ctrl+N` | 上下移动，到页面边缘自动进入相邻页 |
| `Ctrl+↑` / `Ctrl+↓`、`PageUp` / `PageDown` | 上一页 / 下一页（每页数量随终端高度变化） |
| `Home` / `End` | 跳到首 / 末项 |
| `←` / `→` | 移动搜索输入光标 |
| `Backspace` / `Delete` | 删除光标左侧 / 所在字符 |
| `Enter` | 跳转到选中目录；失效目录会显示删除提示 |
| `Tab` | 仅在本次 TUI 会话中打开 / 关闭预览面板，不写入配置 |
| `Ctrl+U` | 清空搜索 |
| `Ctrl+H` / `F5` | 过滤 / 显示隐藏目录（本次会话内，不写入配置） |
| `Ctrl+D` | 排除当前目录及其子目录（再次按确认） |
| `F1` | 打开帮助浮层（`?` 和全角 `？` 也可用） |
| `F2` | 打开设置浮层 |
| `F4` | 打开排除清单（上下选择，`Ctrl+D` 取消排除） |
| `Esc` | 依次关闭预览、清空查询、退出 |
| `Ctrl+C` / `Ctrl+G` | 退出 |
| 鼠标 | 单击选中、双击跳转、滚轮滚动 |

候选池是「历史 ∪ 目录树」：除了 cd 过的历史目录，后台会流式扫描目录树（历史目录的兄弟、`$HOME` 全树等），让没去过的目录也能被模糊搜索命中。发现层候选排在同模糊分的历史候选之后。历史为空时仍会打开界面并从 `$PWD` 自举。

`Ctrl+D` 是**排除**，不是单纯删除：确认后该目录**及其全部子目录**被写入排除清单，此后既不出现在历史候选里，也不会被目录树扫描产出——扫描线程拿它当剪枝集合，整棵子树连 `read_dir` 都不会发生。候选池有五万条量级，噪音的单位是子树而不是单个目录（`~/miniforge3` 一条抵六千多条），所以按子树排除。历史行会额外把记录从历史文件里删掉；发现行没有历史记录，只写清单。

按 `F4` 打开排除清单，上下选择、`Ctrl+D` 取消排除。取消后该子树会**当场**被补扫回来，不用重启——这是取消排除唯一的入口，因为被排除的目录根本不在候选列表里，没有行可以按。

排除清单是 `$XDG_DATA_HOME/cdh/excludes`（默认 `~/.local/share/cdh/excludes`），每行一个绝对路径，`#` 开头为注释，也可以手工编辑；文件不存在等于空清单。它和 `CDH_IGNORE_RE` 的分工是：正则面向「在脚本或配置里预先声明」，清单面向「用的时候顺手清掉眼前的噪音」，两者都同时作用于历史候选和发现候选。

`Ctrl+H`（或 `F5`）是**临时视图过滤**，和排除清单是两回事：它只在本次会话内隐藏路径中含隐藏目录段的候选（`~/.cache/pip` 这类），不写任何文件，再按一次就全部回来。适合「我知道这些点目录迟早还要用，但现在挡视线」的场合；真要长期清掉才用 `Ctrl+D`。判定是按路径分段做的，只看 `/` 分隔出的每一段是否以 `.` 开头，所以 `~/workspace/dot.config` 不会被误判成隐藏目录。

行为开关（环境变量）：

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `CDH_COLOR` | `1` | 设为 `0` 关闭配色（选中行退化为反显） |
| `CDH_MOUSE` | `1` | 设为 `0` 关闭鼠标捕获 |
| `CDH_PREVIEW` | `0` | 设为 `1` 启动时默认打开预览；也可按 `Tab` 切换 |
| `CDH_LANG` | 自动 | TUI 语言；支持 `zh-CN` / `zh` 和 `en` / `en-US`；无法识别的值会被忽略 |
| `CDH_THEME` | `graphite` | TUI 配色主题；可选 `graphite` / `nord` / `daylight` / `mono` / `dracula` / `amber` / `forest`；无法识别的值会被忽略 |
| `CDH_CORNER_3D` | `1` | 设为 `0` 关闭主界面右下角环境 3D 线框立方体（仅彩色模式） |
| `CDH_DISCOVER` | `1` | 目录树发现层总开关；设为 `0` 只用历史候选，不扫描目录树 |
| `CDH_SCAN_ROOTS` | 自动 | 冒号分隔，覆盖扫描根列表（默认：历史锚点 → `$HOME` 全树 → `$HOME` 外锚点级联） |
| `CDH_SCAN_DEPTH` | 不限 | 可选硬深度上限；不设时历史锚点下探 1 层、`$HOME` 全树不限深、`$HOME` 外锚点下探 4 层 |
| `CDH_ANIM` | 任意 | 保留兼容；TUI 已改为事件驱动，不再使用固定刷新动画 |

按 `F2` 可持久化五项 TUI 设置：语言、配色主题、启动时预览、颜色和鼠标捕获。配置文件位于
`$XDG_CONFIG_HOME/cdh/tui.toml`（未设置 `XDG_CONFIG_HOME` 时使用 cdh 已解析的用户配置目录），
格式如下：

```toml
language = "auto" # auto、zh-CN 或 en
theme = "graphite" # graphite、nord、daylight、mono、dracula、amber 或 forest
preview = false
color = true
mouse = true
```

有效值按“环境变量 > 配置文件 > 内置默认值”的优先级解析。`CDH_PREVIEW`、
`CDH_COLOR` 或 `CDH_MOUSE` 存在时，对应设置行会标记为环境控制/只读；`CDH_LANG`
和 `CDH_THEME` 只有在值能识别时才会锁定对应行，无法识别的值会被忽略。设置浮层中的修改会写入文件；
`Tab` 切换预览只影响当前 TUI 会话，不会改变 `preview`。持久化通过同目录临时文件和
原子重命名完成，保证范围为 Unix 及 WSL；不承诺原生 Windows 文件系统语义。

你也可以通过命令行参数控制行为（`cdh -h` 会打印完整帮助）：

```bash
cdh -h
```

核心参数：

* `-v, --version`：显示版本并退出；
* `-l, --limit <N>`：限制最大候选数（默认不截断，让 TUI 搜索覆盖完整历史候选；也可用 `CDH_LIMIT` 设置）；
* `--half-life <sec>`：半衰期（秒）（默认取环境变量 `CDH_HALF_LIFE` 或 7 天）；
* `--threshold <f64>`：评分阈值（低于阈值的条目被过滤，默认 0 不启用）；
* `--ignore-re <re>`：忽略路径正则（默认取 `CDH_IGNORE_RE`，比如忽略 `.git` 等）；
* `--no-check-dir`：不检查目录是否存在（跨机器共享历史时可以打开）。

退出码约定：

* `0`：成功选中目录并输出路径；
* `1`：用户取消（如按 `Esc` / `Ctrl+C`）或 TUI 渲染错误；
* `2`：没有可用候选（比如历史为空或全被过滤）。

### 排序算法

推荐分由四个归一化到 `[0,1]` 的信号线性融合（默认权重见下表）：

| 信号 | 说明 | 默认权重 | 权重变量 |
| --- | --- | --- | --- |
| 频次 frecency | raw 日志的时间衰减访问分，经 `ln(1+s)` 对数压缩，避免 `$HOME` 这类高频目录压扁其余排序 | 0.40 | `CDH_W_FRECENCY` |
| 最近性 recency | 独立的短半衰期信号，让“刚去过”的目录浮上来 | 0.30 | `CDH_W_RECENCY` |
| 上下文 context | 从当前 `pwd` 出发的历史一阶转移 + 直接子目录小加成 | 0.20 | `CDH_W_CONTEXT` |
| 最近唯一 uniq | uniq 文件的几何衰减名次 | 0.10 | `CDH_W_UNIQ` |

另有两项关键处理：

* **防抖**：同一目录在 `CDH_DEBOUNCE_SECS`（默认 600 秒）窗口内的重复访问只计一次频次，
  抵消“每开一个 shell / 标签就记一条 `$HOME`”造成的分数虚高（设为 `0` 可关闭）；
* **排除当前目录**：`pwd` 本身不会出现在候选里（跳到自己所在目录没有意义）。

相关调节变量：

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `CDH_HALF_LIFE` | `604800`（7 天） | 频次半衰期（秒） |
| `CDH_RECENCY_HALF_LIFE` | `86400`（24 小时） | 最近性半衰期（秒） |
| `CDH_DEBOUNCE_SECS` | `600` | 频次防抖窗口（秒），`0` 关闭 |
| `CDH_UNIQ_DECAY` | `0.85` | uniq 几何衰减系数 |

### 示例

只看前 80 条推荐：

```bash
cdh -l 80
```

过滤掉包含 `.git` 的路径：

```bash
CDH_IGNORE_RE='\.git($|/)' cdh
```

---

## 开发者说明

开发期自动化命令（推荐先安装 [`just`](https://github.com/casey/just)）：

```bash
cargo install just
```

常用命令：

```bash
just fmt          # 格式化 Rust + shell 脚本
just fmt-check    # 检查格式是否符合要求
just shell-lint   # bash -n 检查脚本语法
just lint         # cargo clippy（warning 不算失败）
just lint-strict  # cargo clippy -- -D warnings
just test         # cargo test --locked --all
just check        # fmt-check + shell-lint + lint + test
just build-release
just release-dry-run
just hooks-install # 安装 pre-commit / commit-msg hooks
```

Git hooks：

- `pre-commit`：提交前执行格式/脚本语法/测试检查
- `commit-msg`：检查提交信息是否符合约定
- 依赖：`pre-commit` 优先使用 `just`，若未安装则自动回退到底层 `cargo fmt --check` / `shfmt` / `bash -n` / `cargo test`

安装：

```bash
just hooks-install
```

验证：

```bash
git commit --allow-empty -m "bad message"   # 应被 commit-msg 拦下
git commit --allow-empty -m "fix(ci): validate hooks"  # 应通过 hooks 检查
```

已知问题记录：

* [Shell Path Migration Issue](./docs/issues/2026-03-20-shell-path-migration.md)

### 目录结构（节选）

```text
scripts/
  install.sh                 # 顶层入口：安装/卸载；安装二进制；交互选 Shell；路由子脚本
  installers/
    fish/
      install.sh             # 写入 fish 集成（functions / conf.d），不下载二进制
      uninstall.sh           # 清理 fish 集成
      payload/
        cdh.fish             # fish wrapper：调用二进制并 cd
        cdh_log.fish         # fish 日志采集 hook
    bash/
      install.sh             # bash 集成安装（修改 ~/.bashrc，引用 payload）
      uninstall.sh           # bash 集成卸载
      payload/
        cdh.bash             # bash wrapper
        cdh_log.bash         # bash 日志 hook
    zsh/
      install.sh             # zsh 集成安装（修改 ~/.zshrc，引用 payload）
      uninstall.sh           # zsh 集成卸载
      payload/
        cdh.zsh              # zsh wrapper
        cdh_log.zsh          # zsh 日志 hook
  tools/
    git-add-guard.sh         # 可选：git add 前 shfmt + 语法检查

src/
  main.rs                    # 入口：调用 controller::run()
  controller.rs              # CLI + env 解析、推荐 + TUI glue 逻辑
  frecency.rs                # Frecency 算法与打分
  recommend.rs               # 从 raw/uniq 历史生成推荐路径
  picker.rs                  # ratatui TUI（分页搜索 + 异步预览 + 键盘/鼠标）
  lib.rs                     # 模块导出
```

---

## 许可证

[MIT](./LICENSE)
