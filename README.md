# cdh — Frecency 驱动的目录跳转工具（含 TUI）

`cdh` 根据你访问目录的 **频次** 与 **时间衰减（半衰期）** 计算「相关性分数」，并提供一个 **终端交互界面（TUI）** 快速选择与跳转。适合在多个项目目录间频繁切换的开发者。

---

## 功能摘要

* **Frecency 推荐**：近期常用目录排名更靠前；半衰期可配置。
* **多源融合**：原始访问日志（raw）与去重列表（uniq）综合打分。
* **交互式 TUI**：键盘/鼠标、模糊搜索、页内数字直达。
* **过滤与校验**：关键词/正则过滤；可选跳过目录存在性检查。
* **轻量 & 跨平台**：Rust 实现，Linux / macOS / Windows（含 WSL）。

---

## 安装

### 方法一：下载预构建二进制（推荐）

到 GitHub Releases 页面，下载与你系统匹配的压缩包并解压，将二进制加入 `PATH`。

* Linux/macOS 通常放入 `/usr/local/bin` 或 `~/.local/bin`
* Windows 建议放入用户 PATH 目录，或与 Shell 启动脚本一同放置

### 方法二：源码构建

```bash
git clone https://github.com/<your-username>/cdh
cd cdh
cargo build --release
# 将 target/release/cdh 复制到 PATH
```

> Windows/WSL 提示：在 WSL 内编译得到的是 Linux ELF。如需 Windows 可执行文件，请在 Windows 环境构建，或在 GitHub Actions 上产出。

---

## 快速开始

```bash
# 直接运行，显示推荐列表（TUI）
cdh

# 只想打印前 N 条结果，用于脚本
cdh -l 10

# 指定半衰期（单位：秒），例如 3 天
cdh --half-life 259200

# 关键词过滤（空格分隔，大小写不敏感）
cdh project rust

# 跳过目录存在性校验（跨机器同步历史时有用）
cdh --no-check-dir
```

---

## 命令行参数

| 选项                    | 说明              | 默认          |
| --------------------- | --------------- | ----------- |
| `-l, --limit <N>`     | 返回最大条数          | 20          |
| `--half-life <sec>`   | Frecency 半衰期（秒） | 604800（7 天） |
| `--threshold <f64>`   | 最低分阈值           | 0（关闭）       |
| `--ignore-re <regex>` | 忽略路径的正则         | 无           |
| `--no-check-dir`      | 不检查目录存在性        | false       |
| `-h, --help`          | 帮助              | -           |

---

## 环境变量

| 变量               | 说明                     | 默认     |
| ---------------- | ---------------------- | ------ |
| `CDH_HALF_LIFE`  | Frecency 半衰期（秒）        | 604800 |
| `CDH_IGNORE_RE`  | 默认忽略正则                 | 无      |
| `CDH_W_FRECENCY` | frecency 权重            | 0.7    |
| `CDH_W_UNIQ`     | uniq 权重                | 0.3    |
| `CDH_UNIQ_DECAY` | uniq 衰减系数              | 0.85   |
| `CDH_COLOR`      | 彩色输出                   | true   |
| `CDH_MOUSE`      | 鼠标支持                   | true   |
| `CDH_INPUT_POS`  | 搜索输入位置（`top`/`bottom`） | bottom |

---

## TUI 操作

* **导航**：`↑/↓` 或 `j/k`；翻页：`←/→` 或 `p/n`
* **选择**：`Enter`；**退出**：`q`
* **搜索**：按 `i` 进入，输入关键字；`Backspace` 删除；`Esc` 退出搜索
* **数字跳转**：`0-9` 快速选中当前页的对应序号
* **鼠标**：单击移动，双击选中，滚轮滚动

---

## 数据文件

* `~/.cd_history_raw`：原始访问日志，行格式 `timestamp<TAB>path`
* `~/.cd_history`：去重后的最近访问目录（每行一个路径）

> 建议用 dotfiles 管理在多台机器共享。若路径不完全一致，建议配合 `--no-check-dir` 或忽略规则使用。

---

## Shell 集成（可选）

### Fish

```fish
function cd
    if test (count $argv) -eq 1
        cdh $argv[1] | read -l sel
        and test -n "$sel"
        and cd "$sel"
    else
        builtin cd $argv
    end
end
```

### Bash / Zsh

```bash
function cd() {
  if [ $# -eq 1 ]; then
    local sel
    sel="$(cdh "$1")"
    if [ -n "$sel" ]; then
      cd "$sel"
      return
    fi
  fi
  builtin cd "$@"
}
```

### Nushell

```nu
def cdh_cd [dir?: string] {
  if ($dir | is-empty) { cd } else {
    let sel = (cdh $dir | str trim)
    if ($sel != "") { cd $sel }
  }
}
```

---

## 设计要点

* **Frecency**：对每次访问按时间指数衰减；半衰期可调。
* **融合评分**：`final = 0.7 * frecency_norm + 0.3 * uniq_norm`（可通过环境变量调整）。
* **性能**：流式读取、哈希聚合、仅存必要状态。

---

## 故障排查

* **没有候选**：确认历史文件存在且非空 → `ls -la ~/.cd_history ~/.cd_history_raw`
* **结果不准**：缩短半衰期或提高 frecency 权重
* **WSL 路径不一致**：使用 `--no-check-dir` 或忽略规则

---

## 许可证

MIT

