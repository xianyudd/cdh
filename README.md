# cdh - 智能目录切换工具

一个基于 Frecency 算法的智能目录切换工具，支持频次与时效衰减的路径推荐。

## 特性

- 🧠 **智能推荐**：基于访问频次和时间衰减的Frecency算法，推荐最相关的目录
- 🎯 **多维排序**：融合原始日志（frecency）和最近唯一记录（uniq），提供更精准的推荐
- 🖥️ **交互式TUI**：美观的终端界面，支持搜索、键盘导航和鼠标操作
- ⚡ **高性能**：流式读取、低内存占用、即时响应
- 🔍 **灵活过滤**：支持正则过滤、关键词过滤、目录存在性检查
- 🎮 **多种交互方式**：
  - 键盘导航（方向键、vim风格键位）
  - 数字快速跳转
  - 实时搜索过滤
  - 鼠标点击支持

## 核心算法

### Frecency 评分
Frecency = 频次 × 时效衰减

**衰减公式**：`score = Σ 0.5^((now - t_i)/half_life)`

- 半衰期（默认7天）：每经过一个半衰期，权重减半
- 支持在线增量更新和批量评分
- 自动处理乱序时间戳

### 融合推荐
结合两种历史数据源：
- **raw日志** (`~/.cd_history_raw`)：记录 `timestamp<TAB>path`，按Frecency算法计算分数
- **uniq列表** (`~/.cd_history`)：去重后的最近访问目录，按几何衰减计算分数

最终分数 = `0.7 × frecency_normalized + 0.3 × uniq_normalized`

## 安装

### 从源码构建

```bash
git clone <repository>
cd cdh
cargo build --release

# 复制到 PATH
cp target/release/cdh /usr/local/bin/
```

### 依赖
- Rust 2021+
- cargo
- 支持的操作系统：Linux, macOS, Windows (WSL)

## 使用方法

### 基本用法

```bash
# 无参数运行，使用默认配置
cdh

# 限制返回结果数量
cdh -l 10

# 设置半衰期（3天）
cdh --half-life 259200

# 关键词过滤
cdh project
cdh src rust

# 设置评分阈值
cdh --threshold 0.5

# 忽略匹配正则的路径
cdh --ignore-re "/(node_modules|\.git)/"

# 不检查目录存在性（跨机器日志时使用）
cdh --no-check-dir
```

### 命令行选项

| 选项 | 描述 | 默认值 |
|------|------|--------|
| `-l, --limit <N>` | 返回最大条数 | 20 |
| `--half-life <sec>` | Frecency半衰期（秒） | 7天 (604800) |
| `--threshold <f64>` | 评分阈值 | 0（不启用） |
| `--ignore-re <regex>` | 忽略路径正则 | 无 |
| `--no-check-dir` | 不检查目录存在性 | false |
| `-h, --help` | 显示帮助信息 | - |

### 环境变量

| 变量 | 描述 | 默认值 |
|------|------|--------|
| `CDH_HALF_LIFE` | Frecency半衰期 | 604800秒 |
| `CDH_IGNORE_RE` | 默认忽略正则 | 无 |
| `CDH_W_FRECENCY` | Frecency权重 | 0.7 |
| `CDH_W_UNIQ` | Uniq权重 | 0.3 |
| `CDH_UNIQ_DECAY` | Uniq衰减系数 | 0.85 |
| `CDH_COLOR` | 启用颜色 | true |
| `CDH_MOUSE` | 启用鼠标 | true |
| `CDH_INPUT_POS` | 搜索输入框位置 | bottom |

## 界面操作

### 主界面

| 按键 | 功能 |
|------|------|
| `↑/↓` 或 `k/j` | 上下移动光标（越界自动翻页） |
| `←/→` 或 `p/n` | 左右翻页 |
| `0-9` | 快速跳转到本页对应索引 |
| `Enter` | 选中并退出 |
| `q` | 退出程序 |
| `h` | 显示帮助 |
| `i` | 进入搜索模式 |

### 搜索模式

| 按键 | 功能 |
|------|------|
| 任意字符 | 加入搜索查询（大小写不敏感） |
| `↑/↓` 或 `Ctrl+N/P` | 上下移动 |
| `←/→` | 左右翻页 |
| `Enter/Tab` | 选中（单结果直接选中） |
| `Esc` | 返回主界面 |
| `Backspace` | 删除搜索字符 |

### 鼠标操作

| 操作 | 功能 |
|------|------|
| 单击 | 移动光标到点击位置 |
| 双击 | 选中并退出 |
| 滚轮上/下 | 上下滚动 |

## 文件结构

### 状态文件

- **~/.cd_history_raw**：原始访问日志，格式为 `timestamp<TAB>path`
- **~/.cd_history**：去重后的最近访问目录列表（每行一个路径）

### 数据格式示例

```bash
# ~/.cd_history_raw
1700000000	/home/user/projects
1700000100	/home/user/documents
1700000200	/home/user/projects

# ~/.cd_history
/home/user/projects
/home/user/documents
/home/user/downloads
```

## 集成

### Shell 集成

#### Fish Shell
```fish
# 在 ~/.config/fish/config.fish 中添加
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

#### Bash/Zsh
```bash
# 在 ~/.bashrc 或 ~/.zshrc 中添加
function cd() {
    if [ $# -eq 1 ]; then
        local sel=$(cdh "$1")
        if [ -n "$sel" ]; then
            cd "$sel"
            return
        fi
    fi
    builtin cd "$@"
}
```

#### Nu Shell
```nu
# 在 ~/.config/nushell/config.nu 中添加
def cdh_cd [dir?: string] {
    if ($dir | is-empty) {
        cd
    } else {
        let sel = (cdh $dir | str trim)
        if ($sel != "") {
            cd $sel
        }
    }
}
```

## 配置示例

### 最小化系统日志
```bash
# 在 .bashrc 中
export CDH_HALF_LIFE=259200  # 3天
export CDH_IGNORE_RE="/(node_modules|\.git|\.cache)/"
export CDH_LIMIT=30
```

### WSL/跨机器环境
```bash
export CDH_NO_CHECK_DIR=1  # 跳过目录存在性检查
export CDH_W_FRECENCY=0.8
export CDH_W_UNIQ=0.2
```

### 开发者配置
```bash
export CDH_HALF_LIFE=432000  # 5天
export CDH_IGNORE_RE="/(target|node_modules|\.git)/"
export CDH_INPUT_POS=top     # 搜索框在顶部
```

## 实现细节

### 模块架构

- **frecency.rs**：Frecency算法实现
  - Frecency：衰减模型
  - FrecencyState：在线增量状态
  - FrecencyIndex：多目录聚合索引

- **recommend.rs**：智能推荐系统
  - 融合RAW+UNIQ数据源
  - 支持正则/关键词过滤
  - 归一化与权重调整

- **picker.rs**：TUI交互界面
  - 跨平台终端UI（crossterm）
  - 支持颜色、鼠标、搜索
  - 贴底浮动面板设计

- **controller.rs**：控制器
  - CLI参数解析
  - 推荐流程编排
  - 错误处理与退出码

### 性能优化

- 流式读取：避免加载整个历史文件
- 哈希聚合：O(1)时间复杂度的访问记录
- 增量更新：Frecency状态常数时间更新
- 内存友好：仅存储必要的路径和分数

### 退出码

- `0`：成功选中目录
- `1`：用户取消或错误
- `2`：无可用候选目录

## 故障排除

### Q: 没有推荐任何目录？
A: 检查历史文件是否存在且包含有效数据：
```bash
ls -la ~/.cd_history ~/.cd_history_raw
```

### Q: 推荐结果不准确？
A: 调整半衰期和权重参数：
```bash
# 更关注近期访问
export CDH_HALF_LIFE=259200  # 3天

# 提高频次权重
export CDH_W_FRECENCY=0.8
export CDH_W_UNIQ=0.2
```

### Q: WSL环境路径不存在？
A: 启用`--no-check-dir`或设置环境变量：
```bash
export CDH_CHECK_DIR=0
```

## 许可证

MIT

## 贡献

欢迎提交 Issue 和 Pull Request！

---

**cdh** - 让目录切换更智能 🚀
