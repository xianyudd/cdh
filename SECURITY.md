# 安全策略

## 上报漏洞

**不要用公开 issue 上报安全问题。**请走 GitHub 的私有漏洞上报：

<https://github.com/xianyudd/cdh/security/advisories/new>

这是本项目唯一的上报渠道。这里不列安全邮箱——本项目没有专用邮箱和安全团队，写一个
出来只会让报告掉进无人看的地方。上面这个 advisory 草稿只有仓库维护者能看到，可以在
里面附复现步骤、日志和补丁，修好后再一并公开。

如果你确实打不开那个页面（例如没有 GitHub 账号），可以开一个**不含任何利用细节**的
普通 issue，只写「有安全问题需要私下沟通」，维护者会在里面约定后续渠道。

本项目由个人业余维护，没有响应时间承诺（no SLA）。能承诺的只有：确认属实的安全问题
优先于功能开发。

## 支持范围

只有最新 release 会收到修复，旧版本不做回溯补丁——升级到最新 tag 是唯一的修复路径。

| 版本 | 状态 |
| --- | --- |
| 最新 release（撰写时为 `v0.3.2`） | 支持 |
| 更早的 `0.x` | 不支持，请升级 |

`main` 上尚未发布的提交也欢迎上报，修复会随下一个 release 发出。

## 攻击面

`cdh` 是纯本地工具：不监听端口，运行时不发起任何网络请求（依赖里没有网络库，见
`Cargo.toml`），不上报遥测。但它有几处比普通 CLI 更值得注意的地方。

1. **安装会改写你的 shell 启动文件。** `scripts/install.sh` 与
   `scripts/installers/{bash,fish,zsh}/` 向 `~/.bashrc`、`~/.zshrc`、
   `~/.config/fish/` 写入标记块，payload 落在 `~/.config/cdh/`；`~/.bash_profile`
   存在且未 source `~/.bashrc` 时还会追加一行。README 推荐的一键安装是
   `curl … | bash`，即用管道执行远端脚本——这条路的信任边界就是 HTTPS 加 GitHub。
2. **卸载会删历史。** 卸载流程对 `${XDG_DATA_HOME:-~/.local/share}/cdh` 与
   `${XDG_STATE_HOME:-~/.local/state}/cdh` 直接 `rm -rf`，并删掉遗留的
   `~/.cd_history_raw`。其中的 `history_raw` / `history_uniq` 是你 cd 过的全部路径，
   本身就是隐私数据：**不要把原文贴进公开 issue**，需要时只贴脱敏片段。
3. **历史与配置是明文文件**，权限取决于创建时的 umask。共享主机上请自行确认。
4. **预览面板会在被预览的目录里执行 `git`。** `src/picker.rs` 的 `read_git_dirty`
   以该仓库为工作目录跑 `git status --porcelain`，也就是说光标停到某个目录上就够了，
   不需要你 `cd` 进去。git 会读取那个仓库自己的 `.git/config`，而该配置能指定要执行的
   外部程序（`core.fsmonitor` 之类），所以预览一个来源不明的仓库继承的是 git 自身的
   信任模型——cdh 没有额外沙箱。不信任的仓库目录建议先用 `Ctrl+D` 排除，或
   `CDH_IGNORE_RE` 过滤掉。

## 已知限制（不必重复上报）

* **安装器不校验下载产物的校验和。** 发布流程会随 tarball 一起发出
  `*.tar.gz.sha256`（`.github/workflows/release.yml`），但 `scripts/install.sh`
  下载后并不比对。想要更强保证：手工下载 tarball 与对应 `.sha256`，自行
  `sha256sum -c` 通过后再运行包内的 `install.sh`。
* **发布产物没有签名**（没有 GPG / cosign / sigstore）。

这两条是当前的已知现状，写在这里就是为了不用再单独上报；有可落地的改进方案，欢迎开
普通 issue 讨论。

## 不按安全问题处理

* TUI 在特殊终端下显示错乱、宽度算错、颜色异常
* 推荐排序不符合预期
* 由你自己设置的 `CDH_IGNORE_RE`、`CDH_SCAN_ROOTS` 等环境变量导致的行为
* 前提是攻击者已经能以你的身份执行命令的「漏洞」——那种情况下 shell rc 文件本来就任其
  改写，`cdh` 不是防线

以上都走公开 issue。
