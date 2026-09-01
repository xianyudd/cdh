# 工程改进清单

> **这个文件存的是计划**：待办、依赖图、验收标准，以及已经探明的结论。它不跟踪进度——运行时状态
> （pending / in_progress / completed，以及哪条分支在做哪一项）由 coordinator 在仓库之外的共享任务
> 列表里维护，不进 git。
>
> **业务 PR 不要改这个文件。** 理由是争用：若每完成一项都要回来打个勾，所有并行 PR 最后都在这一个
> 文件上相遇，而「总览」表的行是连续的，相邻两行同时改就是实打实的冲突。这个文件属于控制平面，
> 与业务改动分开走。结论有变化时（例如探针跑完发现问题不存在）改这里的正文并写明，不要直接删条目——
> 删掉就等于把判断过程一起丢了。
>
> **基线**：2026-09-01，v0.3.2（`cf7973d`）。文中行号引用以该提交为准，重构后可能漂移。

本清单是一次代码库体检的产出，覆盖 10 项工程债。排序依据是「尾巴是否可控」而非代码量：
有些任务改动只有几行，但它打开的是一扇之前关着的门，门后有多少东西事前无法预估。
每项因此给两个成本——**落地成本**（写这个改动本身）和**尾巴**（它可能暴露出的后续工作）。

## 总览

| # | 任务 | 优先级 | 落地成本 | 尾巴 | 已探明 |
| --- | --- | --- | --- | --- | --- |
| 1 | panic 后终端卡在 raw mode | P0 | 1 小时 | 小 | — |
| 2 | fish / zsh payload 无任何检查 | P0 | 20 分钟 | 中（fish 已探明干净，zsh 待验） | 2026-09-01 fish 半边已验证 |
| 3 | macOS 二进制未经测试就发布 | P0 | 10 分钟 | 不可控 | — |
| 4 | `picker.rs` 7931 行且几乎无测试 | P1 | 1-2 周 | 分阶段可控 | — |
| 5 | MSRV 1.74 缺 CI 固化 | P1 | 15 分钟 | 无（已探明） | 2026-09-01 已验证 1.74.1 通过 |
| 6 | 发布流程两个硬失败点 | P1 | 20 分钟 | 小 | — |
| 7 | 缺 ARM Linux / musl 产物 | P2 | 2-4 小时 | 中 | — |
| 8 | 无依赖审计与 dependabot | P2 | 1-2 小时 | 中 | — |
| 9 | `toml` 精确锁定原因失传 | P2 | 5 分钟 | 无 | — |
| 10 | 缺 CHANGELOG / SECURITY / 模板 / 英文 README | P2 | 见分项 | 无 | — |

## 执行批次

批次内的任务建议一个 PR 打包完成——2、3、5、6 都在改 CI 配置的同一片区域，分四次提会反复触发
全量 CI，也容易互相冲突。批次之间只有一条硬依赖：**4.1 必须在 4.2 之前**。

| 批次 | 内容 | 时间窗 | 说明 |
| --- | --- | --- | --- |
| 0 | 1 | 1 小时 | 纯收益、零风险，可立即做 |
| 1 | 9 + 10a + 10b | 1 小时 | 纯增量文件，不碰现有逻辑 |
| 2 | 2 + 3 + 5 + 6 | 1 小时写 + 预留半天收尾 | CI 加固；**不要和发版排在同一周** |
| 3 | 7 + 8 | 1 天 | 面向外部用户与供应链 |
| 4 | 4.1 → 4.2 → 4.3 → 4.4 | 1-2 周 | 0.4 主线，单独开分支 |

批次 2 的收尾预算原本估 1 天，2026-09-01 探掉 #2 的 fish 半边和 #5 之后砍到半天：四个未知量里
两个已确认无事，剩下的不确定性几乎全集中在 #3 的 macOS 那一项上。这个批次的作用恰恰是把之前
看不见的失败变成可见的红叉，而红叉数量在动手时仍然是未知的。

## PR 与依赖图

2026-09-01 决定：从现在起每项走独立分支 + PR 合并。仓库历史里 `#5`–`#9` 本来就是
`Merge pull request` 提交，最近 6 个提交才转成直接推 main，所以这是恢复旧习惯而非新增约束。

一旦每项都是独立 PR，决定顺序的就不再是逻辑先后，而是**文件争用**——三个 PR 同改 `ci.yml`
就要处理三次 rebase 冲突。于是有一条硬规则：**并行进行的任务之间禁止共享文件**，一个文件在同一
时刻只能有一个 PR 在改；两项任务要碰同一个文件，就只能排成串行链，而不是同时开工。争用情况：

| 文件 | 争用它的任务 |
| --- | --- |
| `src/picker.rs` | #1、#4.1、#4.2、#4.3、#4.4 |
| `.github/workflows/ci.yml` | #2、#3、#5、#8 |
| `.github/workflows/release.yml` | #3、#6、#7 |
| `Justfile` | #2 |
| 只新增文件 | #9、#10a、#10b、#10c |

这张表能当规则用的前提是它没有漏项，而它原本漏了一个最大的：`docs/roadmap.md` 自己。旧用法要求
每完成一项就回来打勾并填状态列，那样每一项都在争用这个文件，表里却看不到它，而且争的还是「总览」
表相邻的几行。取消打勾要求、把运行时状态挪到仓库之外，正是为了让这张表说的是真话。

于是形成两条互不相交的串行链加一个自由节点。两条链碰的文件完全不重叠，可以真并行：

| PR | 分支 | 内容 | 改哪些文件 | 前置 |
| --- | --- | --- | --- | --- |
| PR-1 | `tui-panic-hook` | #1 | `src/picker.rs` | 无 |
| PR-2 | `docs-chores` | #9 + #10a + #10b | `Cargo.toml` + 新文件 | 无 |
| PR-3 | `ci-hardening` | #2 + #3 + #5 + #6 | `ci.yml`、`release.yml`、`Justfile` | 无 |
| PR-4 | `ci-supply-chain` | #7 + #8 | `release.yml`、`ci.yml` + 新文件 | PR-3 |
| PR-5 | `tui-render-snapshots` | #4.1 | `src/picker.rs` | PR-1 |
| PR-6 | `tui-extract-cube` | #4.2 | `src/picker.rs` + 新模块 | PR-5 |
| PR-7 | 待定 | #4.3 | `src/picker.rs` + 新模块 | PR-6 |
| PR-8 | 待定 | #4.4 | `src/picker.rs` | PR-7 |
| PR-9 | 待定 | #10c | 新文件 | 无（可选） |

**前置一列混了两种依赖**，得分开看：PR-5 等 PR-1 是文件争用（两者同改 `src/picker.rs`，逻辑上毫无
关系），而 PR-6 等 PR-5 是真逻辑依赖（4.1 的快照是证明 4.2 搬动没有改变渲染的唯一手段）。这个区别
决定了将来哪里还能拆——文件争用那种，只要能把改动错开到不同文件就能并行；真逻辑依赖拆不掉。

这套划分与上一节的批次一一对应：批次 0 → PR-1，批次 1 → PR-2，批次 2 → PR-3，
批次 3 → PR-4，批次 4 → PR-5 至 PR-8。只有 #10c 两边都没排进去，单独作 PR-9。

**关键路径**：PR-1 → PR-3 → PR-5 → PR-6 → PR-7 → PR-8。PR-2 随时插空，PR-4 等 PR-3 合完。

**一条软依赖**：PR-3 要在 4.1 之前落地。它们不碰同一个文件，所以不是冲突问题——理由是
4.1 到 4.4 是 1-2 周的连续改动，全压在全仓风险最高的文件上。先把 macOS job 和 1.74 job
装好，这一路重构从第一天起就在三个平台上被验证；反过来做，两周后才发现某处改动在 macOS 上
不成立，回溯成本高得多。

**分支命名**沿用仓库惯例的裸 slug（`tui-unexclude-panel`、`fix-unexclude-same-session`），
不加斜杠前缀——`feat/install-ui-progress` 是历史上唯一的例外。合并沿用 merge commit，不 squash。

**提交信息要过 `scripts/tools/commit-msg-check.sh` 的硬校验**，这里有个坑：scope 是白名单
强制（`install|bash|fish|zsh|history|paths|recommend|controller|readme|release|ci|tui`），
不在表里的一律拒。提示语写的是「推荐的 scope」，实际行为是强制，所以 `docs(roadmap):` 会被拒，
无 scope 的 `docs:` 才通过。另外标题上限 72 字符、结尾不能有句号或感叹号。

## P0：会直接伤到用户

### 1. panic 后终端卡在 raw mode

**是什么** —— 已确认，不是推测。三条事实叠加：

- `Cargo.toml:34` 设了 `panic = "abort"`，release 构建不做栈展开，**`Drop` 不执行**。
- 终端恢复完全挂在 `Drop` 上：`src/picker.rs:742` 的 `impl Drop for TermGuard`，748-749 行做 `Show` + `LeaveAlternateScreen` + `disable_raw_mode`。
- 全仓没有任何 `std::panic::set_hook`（`grep -rn 'set_hook' src/` 零匹配）。

结论：release 版进入 alternate screen 之后一旦 panic，`TermGuard::drop` 永远不会被调用，
终端就留在 raw mode + alternate screen 里。

**为什么本地测不出来** —— debug 构建默认 `panic = "unwind"`，`TermGuard::drop` 正常执行，终端能恢复。
这个故障只在 release 版出现，也就是只在用户那边出现。

**触发条件** —— 进入 alternate screen 之后的任意 panic。`picker.rs` 有 121 处 `unwrap()`/`expect()`，
全仓最多。真实先例是刚修掉的两条：`3f3a385`（大写多字符查询 panic）、`d9839d5`（光标按字节切多字节路径）。
这类 bug 复现时用户看到的不是报错信息，而是终端彻底失灵——无回显、无换行，只能盲敲 `reset`。

**怎么修** —— 在进入 alternate screen 之前装 `std::panic::set_hook`：先恢复终端
（`disable_raw_mode` + `LeaveAlternateScreen` + `Show`），再调用事先 `take_hook()` 拿到的默认 hook
打印原始 panic 信息。`abort` 之前 hook 是会执行的，所以这条路有效。保留现有 `TermGuard`，
两者不冲突：hook 管 panic 路径，Drop 管正常退出和 `?` 早退路径。

**验收标准** —— 在 picker 主循环里临时插一句 `panic!("test")`，`cargo build --release` 后运行，要求
panic 信息可读地打印在正常屏幕上、且 panic 后终端仍能正常回显和换行。验证完删掉临时代码。

**成本** —— 落地 1 小时；尾巴小。全表里收益/成本比最高的一条，因为它保护的是用户能直接感知的最严重故障。

### 2. fish / zsh payload 无任何检查

**是什么** —— `Justfile` 的 `shell-lint` 只做 `find scripts -type f -name '*.sh'` 加上 `docs/install.sh`，
然后 `bash -n`。以下四个文件因此从未被任何工具看过：

- `scripts/installers/fish/payload/cdh.fish`
- `scripts/installers/fish/payload/cdh_log.fish`
- `scripts/installers/zsh/payload/cdh.zsh`
- `scripts/installers/zsh/payload/cdh_log.zsh`

**探针结论（2026-09-01）** —— fish 那两个文件已在本地验过，`fish --no-execute` 全部通过，干净。
zsh 半边仍未验证（本地没装 zsh），留给 CI。

同时验明了这个检查的能力边界——一个只会说 PASS 的检查等于没有检查：

| 用例 | `fish --no-execute` |
| --- | --- |
| `if` 缺 `end` | 报错并指出行号 |
| 命令名拼错 `echoo hello` | 放过 |
| 引用未定义变量 | 放过 |

所以它抓的是**结构性语法错误**：块不闭合、引号不配对、`function` / `switch` 缺 `end`。
这修正了本条最初的判断——原话说「改 payload 时打错一个字，CI 全绿」，实际上**加了这个检查之后，
打错命令名照样全绿**。防命令拼错要靠真装一遍再执行的集成测试，那是另一个量级的工程，不在本条范围内。

`zsh -n` 的价值可能比 fish 这半边更高：zsh payload 通过 `chpwd_functions` 挂载，语法比 fish 复杂，
出结构性错误的空间更大。这也是本条保持 P0 的理由——zsh payload 至今没被任何工具看过，
如果它里面有块不闭合，那现在就是坏的。

**触发条件** —— 改 fish 或 zsh payload 时打错一个字。CI 全绿、release 正常产出，用户装完后 shell
启动报错，或者 `cdh` 函数根本不存在。这些 payload 是写进用户 rc 文件的，出问题影响他们每次开终端。

**怎么修** —— `shell-lint` 里加两段：fish 文件跑 `fish --no-execute`，zsh 文件跑 `zsh -n`。
CI 已经在 ubuntu 上，加一句 `apt-get install -y fish zsh` 即可。

**范围提醒** —— 别顺手把 `bash -n` 升级成 `shellcheck`。`bash -n` 只查语法，未加引号的变量展开、
`set -e` 失效场景一概放过，确实该升级；但 shellcheck 对存量脚本一般能报出几十条 warning，
需要逐条决定修还是 `# shellcheck disable`。**单独立项**，不要混在这条里。

**验收标准** —— 故意在 `cdh.fish` 里写一处语法错误，`just shell-lint` 必须失败；改回后必须通过。zsh 同理。

**成本** —— 落地 20 分钟；尾巴不可控——这四个文件第一次被检查，冒出真实错误的概率不低，
那之后就是逐个修 shell 脚本。

### 3. macOS 二进制未经测试就发布

**是什么** —— `.github/workflows/release.yml` 的 `test` job 只有 `runs-on: ubuntu-latest`，
但 `build-and-release` 矩阵产出三个目标，其中 `x86_64-apple-darwin` 和 `aarch64-apple-darwin`
从没跑过测试。`ci.yml` 同样只有 ubuntu。

**触发条件** —— 任何平台差异：XDG 路径解析（macOS 默认没有 `XDG_DATA_HOME`）、终端 key code 差异、
`strip` 参数差异。Mac 用户装上后行为不对，而本地和 CI 都看不到。

**怎么修** —— `ci.yml` 的 check job 改成 `strategy.matrix.os: [ubuntu-latest, macos-latest]`；
`release.yml` 的 `test` job 同样加 macos。构建矩阵本来就有 macOS runner，测试补上几乎不增加维护面。

**验收标准** —— 两个 workflow 都在 macos-latest 上跑完 `cargo test --locked --all` 且为绿。

**成本** —— 落地 10 分钟；尾巴不可控，留半天到一天缓冲。好消息是**这些失败本来就是已经存在的
Mac 用户 bug，只是现在才看见**，所以修它们不是额外工作，是把已经欠下的债显性化。

## P1：迟早会咬人

### 4. `picker.rs` 7931 行且几乎无测试

**是什么** —— 单文件占全仓 14097 行的 56%，里面同时装着渲染、键盘输入、分页、预览面板、
三个浮层（help / settings / excludes）、3D 立方体、模糊匹配高亮。与此同时 `tests/` 只有 385 行
（`install_script.rs` 208 + `tui_settings_non_tty.rs` 177），`AGENTS.md` 自己写着覆盖率集中在
frecency / history / recommend——**缺陷最密集的文件恰好是没有护栏的那个**。其余模块行数都正常
（`recommend.rs` 1349、`discover.rs` 1171、`tui_settings.rs` 886），说明这不是整体风格问题，
只是 picker 一直在往上堆。

**触发条件** —— 每次改 TUI。最近两条 TUI 修复（`3f3a385`、`d9839d5`）都落在这个文件里，
就是这个结构的直接产物：改分页可能撞坏高亮，改光标可能撞坏浮层，而没有测试会告诉你。

**顺序不能反**：先铺测试网，再拆文件。4.1 结束后随时可以停，不会留下半拆状态。

- **4.1 铺测试网**（2-3 天）—— 用 ratatui 的 `TestBackend` 对固定候选集做渲染快照断言：
  选中行、高亮位置、分页边界、三个浮层各一组。不改任何生产代码，风险为零，交付后立刻有价值
  （此后每个 picker 修复都有回归保护）。
  验收：`cargo test` 里存在覆盖上述五类场景的渲染断言，且故意改坏渲染逻辑时它们会红。
- **4.2 摘装饰性模块**（半天）—— 3D 立方体及其光照/次像素渲染与主状态几乎无耦合，
  最容易切干净，适合当拆分的第一刀练手。
  验收：立方体代码移出 `picker.rs`，`CDH_CORNER_3D=0/1` 行为不变，4.1 的快照全绿。
- **4.3 浮层各自独立**（3-5 天）—— help / settings / excludes 三个浮层逐个搬出去，
  一个浮层一个提交，每步都有 4.1 的快照兜底。
  验收：三个浮层各自成模块，F1 / F2 / F4 行为不变。
- **4.4 渲染与状态分离**（暂不估）—— 最深的一刀。做完前三步再重新评估，
  那时对实际耦合情况会清楚得多。

**成本** —— 1-2 周；分阶段可控。单独开分支，按阶段合。

### 5. MSRV 1.74 缺 CI 固化

**是什么** —— `Cargo.toml:5` 写着 `rust-version = "1.74"`（注释说受 ratatui 0.29 约束），
CI 用的是 `dtolnay/rust-toolchain@stable`。这个数字自写下那天起没被任何构建检验过。

**探针结论（2026-09-01）** —— 已在本地用 1.74.1（2023-12-04）实测，三层全绿：

| 检查 | 结果 |
| --- | --- |
| `cargo +1.74 build --locked` | 通过，16.5s，含 ratatui 0.29 在内的依赖全部编译成功 |
| `cargo +1.74 test --no-run --locked` | 通过，测试代码也能编译 |
| `cargo +1.74 test --locked` | 216 个测试全过（210 + 4 + 2），零失败 |

**声明是准确的**，本条因此从「未验证的风险」降级成「补 CI 固化现状」。原文说「有相当概率直接
编译失败」不成立，「不要预设要迁就旧版本」那条提醒也就用不上了——这个项目依赖少、没用新语法，
跨版本兼容性比预估的好。

**顺带发现一条隐藏约束** —— `Cargo.lock` 目前是 `version = 3`。lock 文件 version 4 需要
Cargo 1.78+，所以哪天有人跑 `cargo update` 把 lock 升成 version 4，**1.74 会在读 lock 文件时就
失败**，跟代码本身能不能编译毫无关系；而报错信息指向的是 lock 文件不是代码，很难一眼看懂。
这条耦合是 1.74 job 最有价值的地方：它会在升级发生的那一刻当场抓到。

**触发条件** —— 用户在 Debian stable 之类偏旧的工具链上 `cargo build`。目前实测能过，所以真正的
风险不是「现在坏了」，而是「以后被无声改坏」——无论是代码用了新 API，还是 lock 格式被升级。

**怎么修** —— CI 加一个 `dtolnay/rust-toolchain@1.74` 的 job。既然实测 `cargo test` 能过，
这个 job 就直接跑 `cargo test --locked --all`，比只 `cargo build` 多守住 lock 格式那条线。
**仍然不要在这个 job 里跑 clippy**——旧版 clippy 的 lint 集不一样，只会制造噪音。

**验收标准** —— CI 上 1.74 的 job 为绿，且 `rust-version` 的值与 job 里的版本号一致。

**成本** —— 落地 15 分钟；无尾巴（已探明）。

### 6. 发布流程两个硬失败点

**是什么** —— 两处都在 `.github/workflows/release.yml`：

1. `body_path: scripts/release_notes/${{ github.ref_name }}.md` 是硬依赖，文件不存在整个 release step 失败。
2. 全流程没有一处校验 `Cargo.toml` 的 `version` 与 tag 名一致。

**触发条件** —— 第一种：忘写 release notes 就打 tag，三个平台的二进制全部编译完（十几分钟）
才在最后一步炸掉。第二种更隐蔽：`v0.3.3` 的 tag 打在 `version = "0.3.2"` 的提交上会**一路成功**，
产出的 tarball 目录名是 v0.3.3、里面的二进制 `--version` 报 0.3.2，且没有任何告警。

**怎么修** —— 在 `test` job 最前面加一个前置检查 step：确认
`scripts/release_notes/${GITHUB_REF_NAME}.md` 存在，且 `cargo metadata` 取出的 version 等于
去掉 `v` 前缀的 tag 名，任一不符就 `exit 1`。几行 shell，在编译前就拦住。

**验收标准** —— 用 `workflow_dispatch` 空跑一次，确认检查逻辑本身不报假错。真正的验证要等下一次
打 tag，所以这条在下次发版前只能算「已写未验」。

**成本** —— 落地 20 分钟；尾巴小。

## P2：补齐面向外部的部分

### 7. 缺 ARM Linux / musl 产物

**是什么** —— release 矩阵的 Linux 侧只有 `x86_64-unknown-linux-gnu`。

**触发条件** —— 树莓派、ARM 云主机（现在相当常见）的用户找不到预编译产物；
glibc 偏旧的发行版装上也跑不起来。

**怎么修** —— 矩阵各加一行 `aarch64-unknown-linux-gnu` 和 `x86_64-unknown-linux-musl`。
前者现在可以直接用 GitHub 的 ARM runner，比过去的 cross 编译省事很多；后者要装 musl 工具链，
本项目依赖很干净（没有 openssl 这类麻烦货），大概率顺利，顺便解决静态链接。

**验收标准** —— 两个新产物出现在 release assets 里，且各自在目标机器上实际跑通一次——
不能只看编译通过。

**成本** —— 落地 2-4 小时；尾巴中。

### 8. 无依赖审计与 dependabot

**是什么** —— 没有 `deny.toml`，CI 没有 `cargo-audit` 或 `cargo-deny` 步骤，`.github/` 下只有
`workflows/`，没有 `dependabot.yml`。对一个通过 `curl | bash` 装进别人 shell 的工具，
这块空白比普通 CLI 更需要补。

**怎么修** —— 加 `cargo-deny` 的 CI job，加 `.github/dependabot.yml`。

**验收标准** —— CI 上 `cargo-deny check` 为绿，且 `deny.toml` 里每条豁免都有注释说明理由。

**成本** —— 落地 1-2 小时；尾巴中。`cargo-deny` 首次运行几乎必然报一批 license 和 advisory 条目，
逐条判断并写 allowlist 占了大半时间。dependabot 配置本身十分钟，但它此后会持续开 PR，
属于长期维护面而不是一次性成本。

### 9. `toml` 精确锁定原因失传

**是什么** —— `Cargo.toml:19` 是 `toml = "=0.8.23"`，精确锁定，且没有注释说明为什么。
同一个文件里其他关键行都有注释（`rust-version` 甚至写明「受 ratatui 0.29 约束」），
这一行的沉默很可能意味着当初是临时规避某个问题。

**触发条件** —— 半年后没人记得原因，它会一直挡着依赖升级，也没人敢动。

**怎么修** —— 两条路：翻 git log 找到当初钉住它的提交，补一行注释写明约束来源；
或者试着放宽成 `^0.8`，`just check` 能过就说明约束已经失效，直接放宽。

**验收标准** —— 这一行要么带上解释性注释，要么已经放宽。

**成本** —— 5 分钟；无尾巴。

### 10. 缺失的门面文件

- **10a `CHANGELOG.md`**（30 分钟）—— 13 个版本的 notes 散在 `scripts/release_notes/`，
  没有聚合入口。几乎免费：倒序拼一次，此后每次发布追加一段。
- **10b `SECURITY.md` + issue / PR 模板**（30 分钟）—— 一个改写用户 rc 文件、
  且卸载会删历史的工具，没有漏洞上报渠道。issue 模板要收 shell 类型、终端、`cdh --version`、OS——
  现在这些信息全靠来回问。
- **10c 英文 README**（半天）—— crate 元数据（`description`、`keywords`）和 TUI 都是双语的，
  README 只有中文。现有 README 近 14 KB，含大量表格和环境变量说明，是实打实的翻译量，
  而且此后每次改文档要维护两份。**建议只译一个精简版**，细节指向中文原文。

## 待决问题

**crates.io 要不要发？** `Cargo.toml` 的元数据完全齐备（`description`、`readme`、`repository`、
`homepage`、`license`、`keywords`、`categories`），一副准备发布的样子，但流程里没有任何
`cargo publish`，安装路径只有 install.sh 和 release tarball。如果这是有意的
（`cargo install` 装不了 shell 集成，确实说得通），那没问题，但值得在 README 里写一句说明；
如果本来打算发，这套元数据现在是白填的。

## 未纳入清单的观察

- `feat/install-ui-progress`（`de43348`）和 `tui-cube-polish`（`b045851`）是合并后遗留的分支指针：
  两个提交都已是 main 的祖先，`git log main..<branch>` 为空，diff 也为空，没有任何未合并的工作。
  **因此 4.2 没有前置阻塞**——本清单初版误判为与 `tui-cube-polish` 冲突，2026-09-01 已更正。
  陈旧指针本身是有害的（它正是那次误判的来源），建议连远端一并删除。
- 远端有个 `backup-main-20251109` 标签指向 `3d4f7f7`，是历史上的一次 main 备份，与当前发布线无关。
- 版本序列从 `v0.1.1` 开始，远端没有 `v0.1.0`。
- `v0.1.1`–`v0.2.8` 是轻量标签，`v0.3.0` 起改为附注标签，看起来是有意的切换。
- 本地已装 1.74.1 工具链（2026-09-01 探 #5 时装的），写 CI job 时可以复验；不需要时
  `rustup toolchain uninstall 1.74` 可移除。
