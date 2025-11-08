# cdh — Frecency 驱动的目录跳转（含 TUI）

`cdh` 基于“访问频次 × 时间衰减（半衰期）”对目录打分，提供一个终端 TUI 供你选中后快速跳转。

> 当前已实现 **fish** 的安装/卸载集成；**bash / zsh** 集成在路线图中。

---

## 快速安装与卸载

### 安装（交互选择 Shell）

```bash
curl -fsSL https://raw.githubusercontent.com/xianyudd/cdh/main/scripts/install.sh | bash --noprofile --norc
# 选择 1) fish；完成后：
exec fish -l
```

### 卸载（自动检测、无需交互）

```bash
curl -fsSL https://raw.githubusercontent.com/xianyudd/cdh/main/scripts/install.sh | bash --noprofile --norc -s -- --action uninstall
# 若卸载了 fish 集成，建议执行：
exec fish -l
```

**说明**

* 顶层安装器自动解析 **最新 Release**，将二进制安装到 `~/.local/bin/cdh`，再为所选 Shell 写入集成。
* 安装需要可交互 TTY（菜单通过 `/dev/tty`）；卸载不需要交互。
* 安装过程使用临时目录，退出自动清理，不在本机落盘日志。

---

## 用法

```bash
# 打开 TUI，选择后跳转
cdh

# 查看帮助（建议从这里了解当前可用选项）
cdh -h
```

> 首次无历史时，TUI 会提示如何快速生成一些历史（先多切换几个常用目录再执行 `cdh`）。

---

## 工作方式与数据

* **fish 侧只写原始历史**：`~/.cd_history_raw`（每行格式 `epoch\tpath`）。
* **2 秒去抖**：同一路径 2 秒内重复进入不追加，降低写盘抖动。
* 去重/聚合（如 `~/.cd_history`）将在后续版本由二进制生成；当前保持“只写 raw”的简单稳定策略。

---

## Shell 集成

### fish（已实现）

安装器写入：

* `~/.config/fish/functions/cdh.fish`
* `~/.config/fish/conf.d/cdh_log.fish`

行为约定（返回码）：

* 找不到外部二进制：提示安装并返回 **127**。
* 用户取消/未选择：返回 **1**。
* 未匹配到目录：返回 **2**。

> fish 可能存在同名 `cdh`，安装器会覆盖为自定义函数，但**始终调用外部二进制**。

### bash / zsh（规划中）

尚未提供安装/卸载脚本；见“路线图”。

---

## 故障排查（简要）

* **`cdh` 命令不存在**：确认 `~/.local/bin` 在 `PATH` 中；或重新执行安装命令。
* **TUI 显示“暂无历史”**：先切换几个目录产生日志（如：`cd ~/projects; cd ~; cd /etc; cd ~; cdh`）。
* **下载慢**：可设置 `http_proxy` / `https_proxy`；安装器会使用。
* **Locale 警告**：建议使用 `bash --noprofile --norc` 执行安装命令。

---

## 发行与二进制

* 由 GitHub Actions 构建发布（Linux / macOS，x86_64 与 arm64 变体）。
* 产物命名示例：`cdh-v0.1.2-x86_64-unknown-linux-gnu.tar.gz`。
* 默认解析 **最新 Release**；如需指定版本：

```bash
CDH_VERSION=v0.1.2 curl -fsSL https://raw.githubusercontent.com/xianyudd/cdh/main/scripts/install.sh | bash --noprofile --norc
```

---

## 开发者

目录结构（节选）：

```
scripts/
  install.sh                 # 顶层入口：安装/卸载；安装二进制；交互选 Shell；路由子脚本
  installers/
    fish/
      install.sh            # 写入 fish 集成（functions / conf.d），不下载二进制
      uninstall.sh          # 清理 fish 集成
      payload/
        cdh.fish
        cdh_log.fish
  tools/
    git-add-guard.sh        # 可选：git add 前 shfmt + 语法检查
```

本地调试：

```bash
# 安装（交互）
bash --noprofile --norc scripts/install.sh
# 卸载（自动检测）
bash --noprofile --norc scripts/install.sh --action uninstall
```

---

## 路线图：bash / zsh 集成

* **注入点**：

  * *bash*：`~/.bashrc`（登录/非登录兼容）。
  * *zsh*：`~/.zshrc`（考虑 oh-my-zsh 等加载顺序）。
* **冲突与回退**：检测既有 alias/函数；同名时以安装器标记包裹，确保可回滚。
* **自检**：与 fish 的返回码约定对齐；提供 `bash -lc` / `zsh -lic` 检验命令。
* **卸载**：按安装器标记精准删除注入片段，不误删用户自定义内容。
* **测试矩阵**：Ubuntu（bash/zsh）、macOS（zsh）、WSL（bash/zsh），登录/非登录、管道/本地两种安装方式。

---

## 许可证

MIT
