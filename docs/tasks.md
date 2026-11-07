# 任务登记

## SEARCH-DISCOVERY-RANKING-001 — 搜索交接、发现范围与结果排序修复

- 状态：**实现与自验完成，待独立复核/待提交**。两处收口闭环：完整安全历史储备让 `CDH_DISCOVER=0`、扫描剪枝与目录符号链接历史在清空查询后可检索；`search_50k` 新增 `query_compilations` 编译次数断言。此前独立 ready=false 结论保留为历史证据，最终验收仍需独立 verifier 通过；已获用户授权：独立复核通过并主会话逐文件讲解后自动 commit → push → PR → CI → merge main（详见下文“主会话实测门禁与提交授权”）。未暂存/未提交。
- 工作分支：`fix/search-discovery-ranking`。
- 计划：[search-discovery-ranking.plan.md](plans/search-discovery-ranking.plan.md)。
- RED/GREEN、兼容取舍及缺口：[search-discovery-ranking.tdd.md](testing/search-discovery-ranking.tdd.md)。

| 子目标 ID | 子目标 | 角色负责人 | 状态 | 验收 |
| --- | --- | --- | --- | --- |
| SEARCH-DISCOVERY-RANKING-001-S1 | 发现来源与查询交接契约 | designer | 设计交接完成 | 完整安全 anchors、CLI OR/argv 边界、路径意图、通用强度与结构排序 |
| SEARCH-DISCOVERY-RANKING-001-S2 | 独立可失败验收矩阵 | acceptance-reader | 验收交接完成 | 实际 CLI→PTY、HOME 外未访问目标、quoted spaces、路径/排序、非 TTY、安全和变异 |
| SEARCH-DISCOVERY-RANKING-001-S3 | 最小实现与承重回归 | implementer | 实现与自验完成（两处收口），待独立复核 | 无发现/扫描剪枝/目录符号链接的完整安全历史在清空查询后可检索；保留分数、时间、身份、删除、安全与初始裁剪；50k 编译次数断言 `query_compilations` 与逐候选变异 RED |
| SEARCH-DISCOVERY-RANKING-001-S4 | 当前源码独立复核 | verifier | 再次复核未通过（ready=false，历史结论；针对上一版源码，本轮修复待复核） | 308 项测试、15 项核心变异、release PTY 文件 13 项通过；P2/P3 为对上一版源码的阻塞结论，本轮已收口自验、仍待独立复核 |

### 范围与边界

发现 roots 独立于 query/limit/threshold，保留存在性、ignore/excludes、symlink 与预算。CLI 以字面 OR 交接并保持参数边界，编辑仍 OR、清空后恢复 nucleo。裸绝对/tilde/相对路径只接受字面精确/前缀；basename 胜祖先匹配，等强度历史优先、发现项通用结构决胜。非 TTY 仍为历史最佳项无换行或无结果 exit 2。

没有特定工具路径或目录用途权重；独立复核通过并经主会话逐文件讲解前不暂存、提交、推送或开 PR，产物留工作区交独立 verifier。已获用户授权：独立复核通过并逐文件讲解后自动 commit → push → PR → CI → merge main；已确认 main 分支保护要求 6 个 status check 全绿、无需人工批准，仓库未启用 auto-merge，将走普通 PR 合并。Git 操作由主会话另行安排，本 implementer 不自行执行。仍不外部发布额外产物、不更新安装二进制、不改真实历史、认证、agent 配置/权限。仅 implementer 写生产/测试和本任务文档。

### 此前独立复核阻塞与本轮自验

- **P2 历史回流不完整**：`src/picker.rs:2006,2014` 候选仍只由裁剪后的 items 构建；完整历史登记身份后必须等扫描器发射。隔离 release PTY 中，历史含 HOME/node_modules 与 HOME/seed、无 ignore/excludes；`cdh node_modules` 正对照可选，`cdh seed` → Ctrl+U → `node_modules` 却 Page 0/0。符号链接目录、`CDH_DISCOVER=0` 同样失败；原 HEAD 也复现，属于旧缺陷未修完整而非新回归。证据：`scratch/verifier-release-probe.log`、`scratch/verifier-head-history-probe.log`。
- **P3 50k 编译次数无承重断言**：`src/picker_tests.rs:4906` 只断言匹配数量和首项。把 `src/picker.rs:772` 的查询构造移进循环、执行 50,000 次后，`search_50k` 仍 exit 0。保留匹配行为测试价值，但不得用它证明单次编译；需可确定的次数断言。证据：`scratch/verifier-compile-per-candidate.log`。

以上为此前 verifier 的失败证据，源码位置是当时快照。本轮仅修 P2/P3：P2 新库回归实际 RED（0 个匹配而非 2），三个真实 CLI→PTY 回归各在直接启动正对照后 RED；完整历史储备与分数交接修复后 GREEN。P3 用 Filter 实例级测试计数，50k 的 fuzzy/CLI OR 每次 refilter 精确编译 1 次、空查询 0 次；隔离逐候选编译变异明确 RED（50000 != 1），非墙钟阈值。首帧仍由原 query/limit/threshold 控制，首次编辑/清空后完整安全历史入池，扫描回流同样保留完整历史分数。证据 `scratch/search-final-gaps-*.log` 与 TDD 最后节。尚需独立 verifier，未解除最终验收阻塞。

### 两处收口确认

- **P2 收口（历史候选储备）**：完整安全历史储备修复后，`CDH_DISCOVER=0`、扫描剪枝目录名与目录符号链接三类历史在“`cdh seed` → Ctrl+U → 目标”清空查询路径下均可检索，不再依赖扫描批次发射；恢复项按 History 前缀追加，不改变原推荐顺序与首帧 limit/threshold 语义。RED/GREEN 与四场景细节见 TDD 最后节，证据 `scratch/search-final-gaps-pty-{red,green}.log`。
- **P3 收口（编译次数）**：`search_50k` 新增 `query_compilations` 确定次数断言（`cfg(test)` 且按 Filter 实例隔离，不引入热路径成本）；fuzzy 与 CLI OR 每次 refilter 精确编译 1 次、空查询 0 次，隔离逐候选编译变异明确 RED（50000 != 1），无墙钟阈值。证据 `scratch/search-final-gaps-compile-{control,red,green}.log` 与 TDD 最后节。

### P3 补充收口（verifier 复核新发现，已修复并补回归）

独立 verifier 在本分支源码上发现 P3：`controller.rs` 关键字 token 处理改为 `if !a.is_empty() { opt.tokens.push(a); }` 后，纯空白 argv（如 `cdh " "`）被当作字面 token，非 TTY 从“空查询 → 输出最佳历史 exit 0”退化为“无候选 exit 2”。本例事实：修复为 `if !a.trim().is_empty()` ——只丢弃纯空白参数，仍 push 原始 `a` 保留 argv 边界（`"alpha beta"` 词内空格不变），空白参数等同空查询、不会传给 TUI 作为初始查询。回归测试（`tests/tui_settings_non_tty.rs`）：`blank_argv_matches_best_history_like_empty_query`（有历史时 `cdh " "` 与 `cdh ""` 均 exit 0 且 stdout 恰为最佳历史路径、无换行）、`blank_argv_with_empty_history_still_exits_two`（无历史时 `cdh " "` 仍 exit 2）；变异验证：暂时恢复旧条件后该测试明确 RED（`cdh " "` exit 2）。门禁：`cargo test --locked --test tui_settings_non_tty` 4 项通过；完整 `cargo test --locked --all`、`cargo fmt --check`、严格 clippy、`cargo +1.88 test --locked --all`、`cargo build --locked --release`、`git diff --check` 全绿（含本项后完整套件 289 库 + 25 集成 = 314 通过、0 失败）。

### 检查与环境缺口

首轮与定向修复的 RED/GREEN、8 项及 5 项变异原始记录保留在 TDD。再次 verifier 运行 `cargo test`、`cargo test --locked --all`、`cargo +1.88 test --locked --all`，各 **308 通过、0 失败**；`cargo fmt --check`、严格 clippy、`git diff --check` 通过，日志 `scratch/verifier-required-checks.log`。独立正对照及 15 项核心变异均被断言捕获、无编译错误；逐候选编译变异存活单独记为 P3，不计入已捕获项。

当前源码 release 构建与 PTY 文件 **13 项通过**；另外独立探针验证外部未访问兄弟目标在空启动/query/limit/threshold 下均可选、路径隔离、空历史交互发现/非 TTY exit 2、Enter/Ctrl+C 完整 termios 恢复与隔离历史字节不变。日志见 `scratch/verifier-release-pty.log`、`scratch/verifier-release-probe.log`。这些通过项不能消除上述阻塞。本次文档收尾再次运行 `just fmt-check`、`just shell-lint`、`just test`，均 exit 0；shell 仍明确 SKIP 两个 zsh payload。

Rust 1.74 实跑 exit 101，被既有 lockfile v4 阻塞；Cargo.toml 与 HEAD 已声明 MSRV 1.88，不是本轮回归。无 zsh，两个 payload 未验证。独立覆盖率尝试的 288 项仪器化库测试通过，但 LLVM profile v10/工具要求 v9 导致 merge 失败，没有可信百分比，不能证明 >=80%；日志 `scratch/verifier-coverage.log`。未安装工具，未新增 abort panic 旅程。

verifier 临时源码/构建副本及 HOME/XDG 夹具已清理；本轮文档收尾未创建或删除仓库 scratch 产物，检查无 `/tmp/*cdh*` 残留。scratch 仍保留 verifier 脚本/日志、首轮变异源码副本与 evidence，以及 repair evidence 目录；不是本轮所有的产物，不擅自删除，详情见 TDD。

本轮检查：`just fmt-check`、`just shell-lint`、`just test`、`cargo fmt --check`、严格 clippy、`cargo +1.88 test --locked --all` 全 exit 0；完整套件各 **289 库 + 23 集成 = 312 通过、0 失败**。release 构建、PTY 文件 **16 项**、release 50k 次数回归通过。已有 15 项变异和全仓性能盘点不重做；zsh 两个 payload SKIP 与 LLVM 覆盖率工具缺口仍保留。仅本轮隔离源码副本和脚本清理，保留小日志，未删除其他 scratch 产物；检查无本轮 PTY 夹具与 `/tmp/*cdh*` 残留。

### 主会话实测门禁与提交授权

主会话在当前源码上实测门禁全部通过：`cargo test --locked --all` **312 通过、0 失败**；`cargo fmt --check`、严格 clippy（`--all-targets --all-features -- -D warnings`）、`cargo +1.88 test --locked --all`、`cargo build --locked --release`、`git diff --check` 均 exit 0。`README.md` 与 `README.en.md` 均已改动，CI 的 README 双语同步检查会通过。既有环境缺口保留：无 zsh（两个 zsh payload SKIP）、LLVM profile v10/v9 不兼容无可信覆盖率百分比、MSRV 实际为 1.88、lockfile v4。

已获用户授权：独立复核通过并逐文件讲解后，自动 commit → push → PR → CI → merge main。已确认 main 分支保护要求 6 个 status check 全绿、无需人工批准；仓库未启用 auto-merge，将走普通 PR 合并。Git 操作由主会话另行安排，本实现代理不自行 stage/commit/push。

边界/需确认：完整安全历史可检索是用户明确的正常契约，不再列为需要用户确认的产品假设。首次查询编辑/清空恢复储备，初始裁剪意义不变；启动项保留原查询相关分数，原未入池项取无 query/limit/threshold 的完整历史分数。沿用 CLI OR 到清空、裸 `./`/`../` 相对路径语义，无新增需确认点。未暂存、未提交；状态为实现与自验完成，待独立复核通过后由主会话逐文件讲解并自动走 Git 流程。
