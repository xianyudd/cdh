# cdh — Frecency 驱动的目录跳转（含 TUI）

`cdh` 基于“访问频次 × 时间衰减（半衰期）”对目录打分，提供一个终端 TUI，让你在历史目录里快速按分数排序选择并跳转。

> 当前已支持 **fish / bash / zsh** 的安装与卸载集成。

* 仓库地址：[https://github.com/xianyudd/cdh](https://github.com/xianyudd/cdh)
* 安装脚本：[https://raw.githubusercontent.com/xianyudd/cdh/main/scripts/install.sh](https://raw.githubusercontent.com/xianyudd/cdh/main/scripts/install.sh)

---

## 快速安装与卸载

### 一键安装（交互选择 Shell）

```bash
curl -fsSL https://raw.githubusercontent.com/xianyudd/cdh/main/scripts/install.sh \
  | bash --noprofile --norc
```

然后根据提示选择 shell（fish / bash / zsh），安装完成后执行对应命令重新加载：

```bash
# fish
exec fish -l

# bash
exec bash -l

# zsh
exec zsh -l
```

如果所在网络访问 GitHub latest 跳转不稳定，可以固定安装指定版本：

```bash
curl -fsSL https://raw.githubusercontent.com/xianyudd/cdh/main/scripts/install.sh \
  | CDH_VERSION=v0.2.6 bash --noprofile --norc
```

也可以直接使用 GitHub release 打包产物安装，适合 `raw.githubusercontent.com` 不稳定但 release asset 可下载的环境：

```bash
curl -fsSL https://github.com/xianyudd/cdh/releases/download/v0.2.7/cdh-v0.2.7-x86_64-unknown-linux-gnu.tar.gz \
  | tar -xz
cd cdh-v0.2.7-x86_64-unknown-linux-gnu
bash --noprofile --norc install.sh
```

也可以本地调试安装脚本（在仓库根目录）：

```bash
bash --noprofile --norc scripts/install.sh
```

### 一键卸载

远程卸载（自动清理 shell 集成 + 二进制 + 历史文件）：

```bash
curl -fsSL https://raw.githubusercontent.com/xianyudd/cdh/main/scripts/install.sh \
  | bash --noprofile --norc -s -- --action uninstall
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
* 按 Frecency 算法打分并排序；
* 启动一个 TUI 列表供你选择目录；
* 选择后，shell 包装函数会 `cd` 到该目录。

你也可以通过命令行参数控制行为（`cdh -h` 会打印完整帮助）：

```bash
cdh -h
```

核心参数：

* `-l, --limit <N>`：返回最大条数（默认取环境变量 `CDH_LIMIT` 或 20）；
* `--half-life <sec>`：半衰期（秒）（默认取环境变量 `CDH_HALF_LIFE` 或 7 天）；
* `--threshold <f64>`：评分阈值（低于阈值的条目被过滤，默认 0 不启用）；
* `--ignore-re <re>`：忽略路径正则（默认取 `CDH_IGNORE_RE`，比如忽略 `.git` 等）；
* `--no-check-dir`：不检查目录是否存在（跨机器共享历史时可以打开）。

退出码约定：

* `0`：成功选中目录并输出路径；
* `1`：用户取消（如按 `q` / Ctrl+C）或 TUI 渲染错误；
* `2`：没有可用候选（比如历史为空或全被过滤）。

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
  picker.rs                  # crossterm TUI（列表 + 搜索 + 键盘/鼠标）
  lib.rs                     # 模块导出
```

---

## 许可证

[MIT](./LICENSE)
