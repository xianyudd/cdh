# 搜索交接、发现范围与排序修复计划

- 稳定任务 ID：`SEARCH-DISCOVERY-RANKING-001`。
- 状态：**实现与自验完成，待独立复核通过后提交**（此前独立 ready=false 保留为历史结论；312 项测试与 release 验证不替代独立验收）。两处收口完成：完整安全历史储备让 `CDH_DISCOVER=0`/扫描剪枝/目录符号链接历史在清空查询后可检索；`search_50k` 新增 `query_compilations` 编译次数断言。主会话实测门禁全绿：`cargo test --locked --all` 312 通过，`cargo fmt --check`、严格 clippy、`cargo +1.88 test --locked --all`、`cargo build --locked --release`、`git diff --check` 均 exit 0。已获用户授权：独立复核通过、主会话逐文件讲解后自动 commit → push → PR → CI → merge main（main 分支要求 6 个 status check 全绿、无需人工批准、仓库未启用 auto-merge，走普通 PR 合并）。未暂存、未提交，不标为最终完成。
- 分支：`fix/search-discovery-ranking`。
- 登记：[tasks.md](../tasks.md)。
- 证据：[search-discovery-ranking.tdd.md](../testing/search-discovery-ranking.tdd.md)。
- 分工：designer 与 acceptance-reader 提供已核对的设计/独立验收；唯一 implementer 落地并自行验证；verifier 后续独立只读复核。

## 范围与非目标

修复发现 roots 依赖 CLI 过滤/limit/threshold、CLI 查询未交接、全路径 fuzzy 后代噪音和发现项用途无关的结构排序。保留存在性/ignore/excludes、安全扫描预算、symlink、非 TTY 历史最佳推荐/exit 2、Unicode、模糊操作符、选择及分页不变量。

不硬编码任何工具路径或目录用途，不扩大扫描范围/预算，不改变真实历史、安装二进制、认证、agent 配置或权限。独立复核通过并经主会话逐文件向用户讲解前，不暂存/提交/推送/开 PR；验收通过后已获用户授权自动走 commit → push → PR → CI 全绿 → merge main，main 分支保护要求 6 个 status check 全绿、无需人工批准、仓库未启用 auto-merge，走普通 PR 合并；Git 操作由主会话另行安排，本 implementer 不自行执行。不外部发布额外产物。新增夹具与探针放仓库 scratch 并用 RAII 清理，既有测试设施保留。

## 最终设计与取舍

1. **单次推荐读取**：`prepare_recommendations` 返回推荐项与完整 `scan_paths`；raw/uniq 在关键词过滤之前收集合格 anchors，独立于 limit/threshold。复用既有时间戳、防抖、uniq 衰减、归一化/上下文逻辑；存在性按唯一路径缓存、anchors 去重后稳定排序。非 TTY 不启用 anchors 收集。
2. **兼容入口**：保留 `pick`、`pick_with_history` 与 recommend 公开签名；crate 内 `pick_with_search` 交接 anchors、原始 argv、实际 CLI 覆盖的 ignore 正则和启动 HOME/pwd。`build_search_app` 在池与扫描前过滤 excludes/ignore，流入批次再次检查，派生 roots 与子树剪枝仍由现有 discover 执行。
3. **两种查询模式**：CLI 字面大小写不敏感 OR、保持 argv 边界。搜索框以可逆双引号/反斜杠显示并继续按 OR 编辑，页头简短 OR 标记；清空后恢复原 nucleo（空格 AND、显式操作符）。未闭合引号允许继续编辑；两个 README 和双语 help 已说明。
4. **路径意图**：裸绝对路径、`~`/`~/`、`./`/`../` 按启动上下文词法解析，不 canonicalize、不增加 roots；缺失 HOME/pwd 也不退回 fuzzy。`~user` 不展开，普通 `a/b` 在手输模式仍 fuzzy；显式模式前缀保留 Pattern。末组件补全，尾斜杠限定组件边界；路径空格/Unicode 按字面处理，输出保持原始路径。
5. **共享强度**：存在性优先；路径精确/末组件补全/后代前缀，随后 basename 精确/前缀/子串/模糊，最后祖先匹配。coverage 低于类别；完整 nucleo Pattern 先控制资格，basename 不绕过否定。等强度先历史，历史按推荐分，发现按深度→热度→长度→字典序；空查询历史前缀保持原序。实现目前在候选循环外构造查询、排序缓存结构键、高亮仅可见行计算；单次编译是设计目标，但现有 50k 测试未保护次数，再次复核记为 P3。
6. **UI 稳定性**：继续一次 drain-all 后合并/refilter，保留选中路径、隐藏切换、分页/resize 逻辑。只增加克制的 OR 标记，不重设计布局。

## 子目标落地

| 子目标 | 角色 | 当前状态 | 交付 |
| --- | --- | --- | --- |
| S1 | designer | 设计交接完成 | anchors 数据流、兼容 wrapper、OR 到清空的语义、路径/排序规则 |
| S2 | acceptance-reader | 独立验收交接完成 | 实际 argv→PTY、raw/uniq roots、非 TTY、路径/Unicode/安全及变异门槛 |
| S3 | implementer | 实现与自验完成（两处收口），待独立复核 | 完整安全历史储备及分数交接；无发现/剪枝/符号链接在清空查询后可检索的真实 PTY；50k 确定编译次数断言 `query_compilations` 与逐候选变异 RED |
| S4 | verifier | 再次复核未通过（ready=false；历史结论，针对上一版源码） | 308 项测试、15 项核心变异、release PTY 文件 13 项通过；P2/P3 为对上一版源码的阻塞结论，本轮修复已自验、仍待独立复核 |

## 首轮实际验收（保留历史证据）

原始 RED：先只新增旧代码能编译的 3 个库排序/路径回归和 2 个真实 PTY 交接/发现回归，分别运行并失败；同组修复后 GREEN。随后定向恢复过滤 roots、空查询、AND、拆 quoted token、删除 basename/深度/路径隔离/历史偏好，8 项隔离源码变异均被对应测试抓住，正对照通过。缺失路径上下文另做实际 RED/GREEN。

真实 PTY 覆盖未访问 HOME 外兄弟目标在 query/`--limit 1`/`--threshold 2` 下仍可选、CLI OR/quoted spaces、绝对/tilde/相对 Unicode 空格路径和 CLI ignore/excludes。非 TTY 子进程验证 history-only 字节/退出码和文件不变。新 app 构造/批次测试覆盖过滤、回流、重排及选择/分页；既有扫描预算、symlink、Unicode/高亮/布局/禁色测试全套保留。

首轮 release 探针使用当时本地源码二进制，Enter/Ctrl+C 两次 termios 完整恢复，目标发现且夹具历史不变。50k release 查询排序单次 13.66295 ms，无机器相关时限断言；此数据不能证明单次编译，逐候选编译变异在再次复核中存活。

首轮检查命令：`just fmt-check`、`just shell-lint`、`just test`、`cargo clippy --locked --all-targets --all-features -- -D warnings`、`cargo build --locked`、`cargo build --locked --release`、`cargo +1.88 test --locked --all`。结果记录在 TDD 文档及 `scratch/search-final-checks.log`、`scratch/search-msrv-188-green.log` 等日志。

## 独立复核后修复

只修复 verifier 确认的两项问题。先添加库身份/删除及真实 CLI→PTY 回归，实际 RED；Unicode 新回归先确认正对照 GREEN，再在隔离副本关闭 normalization 得到断言 RED，之后才修改生产逻辑。

复用完整合格 anchors 保留 History 身份与最新访问时间，未入池路径仍可扫描回流；恢复项批量追加到历史前缀，不改变原推荐顺序，也不让其进入发现结构排序。被裁剪项恢复分数为 0，不伪造推荐分；等强度历史偏好由来源保证。删除成功后撤销该路径身份，取消排除不复活已删除历史。公开入口、扫描预算、ignore/excludes 和非 TTY 契约不变。

新增 Unicode 断言覆盖 `cafe`→`café`、basename 强度、精确 scalar 高亮及 Smart 方向性；原测试按实际 negation/anchoring 行为重命名。五项修复相关变异全被断言捕获，正对照通过，源码副本清理。定向修复自验完整套件 **308 通过、0 失败**；必需门禁、严格 clippy、MSRV 1.88、release 历史偏好 PTY 与 Enter/Ctrl+C termios 探针全通过。证据与命令见 TDD 的“独立复核后的定向修复”节和 `scratch/search-repair-*`。这些历史 GREEN 保留，但再次独立复核发现恢复仍依赖扫描发射，不能据此声明历史回流完整。

## 再次独立复核：阻塞（ready=false）

1. **P2 历史回流残留**：`src/picker.rs:2006,2014` 的完整历史登记仅保存身份，未主动恢复启动查询裁剪的候选。扫描器在 `src/discover.rs:486,491` 剪枝或跳过的目录，以及 `CDH_DISCOVER=0` 下的目录，清空查询后仍不可检索。隔离 release PTY 中，HOME/node_modules 与 HOME/seed 均为合格历史、无 ignore/excludes；直接 `cdh node_modules` 可选，`cdh seed` → Ctrl+U → `node_modules` 显示 Page 0/0。符号链接与关闭发现同样失败；原 HEAD 也复现，是旧缺陷未修完整而非新回归。证据 `scratch/verifier-release-probe.log`、`scratch/verifier-head-history-probe.log`。后续应恢复全部安全合格历史候选、保持初始推荐与非 TTY 契约，并为这三类场景补可失败验收；本轮文档收尾不实施源码修复。
2. **P3 查询编译次数缺口**：`src/picker_tests.rs:4906` 的 50k 测试只有数量与首项断言；把 `src/picker.rs:772` 的查询构造移进候选循环执行 50,000 次，仍 exit 0。需可确定的编译次数断言，不能用机器耗时阈值或此测试名称代替证据。日志 `scratch/verifier-compile-per-candidate.log`；测试对匹配行为仍有价值。源码行号均为此次复核快照。

独立 `cargo test`、`cargo test --locked --all`、`cargo +1.88 test --locked --all` 各 **308 通过、0 失败**；fmt、严格 clippy、`git diff --check` 通过（`scratch/verifier-required-checks.log`）。正对照及 roots/初始查询/OR/quoted token/basename/深度/路径隔离/历史偏好/身份/访问时间/删除/Unicode normalization 与 highlights/ignore/缺失上下文共 **15 项核心变异**均被断言捕获，无编译失败（`scratch/verifier-mutations/results.txt`）；逐候选编译变异存活不计入通过项。

当前源码 release 构建通过，release PTY 文件 **13 项通过**（`scratch/verifier-release-pty.log`）。独立探针验证外部未访问兄弟目标在空启动/query/limit/threshold 下可选，绝对/tilde/相对 Unicode 空格路径隔离，空历史交互发现与非 TTY exit 2，Enter/Ctrl+C 完整 termios 恢复及隔离历史字节不变（`scratch/verifier-release-probe.py`、`scratch/verifier-release-probe.log`）。本次文档收尾重新运行 `just fmt-check`、`just shell-lint`、`just test` 全 exit 0，zsh 两个 payload 仍 SKIP；没有新增源码或测试、没有解除阻塞。

verifier 临时源码/构建副本及 HOME/XDG 夹具已清理。收尾检查无 `/tmp/*cdh*` 残留；仓库 scratch 中仍有首轮源码变异副本、repair evidence 和 verifier 脚本/日志。它们属于既有工作，本轮没有创建或擅自删除，具体保留范围见 TDD。修复两项缺口并再次独立复核通过前，不标为已实现/已验证待提交，更不标为已提交或完成。

## 不能隐藏的缺口与假设

- 仓库 Cargo.toml/CI 已明确迁移至 MSRV 1.88，HEAD 同样如此；本次不降级依赖。独立 Rust 1.74 实跑 exit 101，先因既有 lockfile v4 失败，不是本轮回归，不声称整仓 1.74 支持。
- `just shell-lint` exit 0，但本机无 zsh，两个 zsh payload SKIP；没有改 shell，不把 SKIP 报成验证通过。
- 首轮及独立覆盖率尝试均在 profile merge 失败：Rust LLVM 22 产生 v10，现有 LLVM 18 仅接受 v9；独立 288 项仪器化库测试虽通过，仍没有可信百分比，不能证明 >=80%。日志 `scratch/verifier-coverage.log`，未安装工具。
- 用户已明确完整安全历史可检索是正常契约，不再作为需确认假设。本轮首次编辑/清空恢复储备，初始 query/limit/threshold 仍有意义；启动项沿用原分数，储备项取完整未过滤历史分数。沿用 CLI OR 到清空、裸 `./`/`../` 相对路径语义，不新增其他产品需求。
- 已有 debug/release PTY 与库布局回归，但未新跑每种真实终端尺寸/颜色组合或 abort panic 旅程；TermGuard/panic hook 未改。此前独立复核未通过；本轮自验通过后仍需独立 verifier，不以绿色门禁替代验收结论。

## 最后两项缺口：本轮实施与自验

1. 先补实际 RED：无扫描批次的完整历史/分数库回归失败；无发现、node_modules 剪枝、目录符号链接三项真实 CLI→PTY 各在直接启动正对照之后失败。新增 Filter 实例级测试计数，50k 正对照 GREEN；隔离副本把查询构造移入循环，次数断言 RED（50000 != 1），没有编译错误。
2. `prepare_recommendations` 同一次 raw/uniq 读取与存在性缓存中收集完整历史分数；有启动 query 时保留独立过滤信号，确保原推荐分数/转移语义不变。完整储备不排序；空启动 query 复用已经计算的分数。controller 只增加 crate 内 metadata 交接，不改变非 TTY top/exit 2。
3. `build_search_app` 在完整储备、身份与池之前应用 ignore/excludes，沿用已经排序的 scan_paths 顺序；储备安装在初始查询计算之后，保留首帧 limit/threshold。首次编辑/清空时一次 splice 恢复 History，随后只 refilter 一次，不依赖扫描批次、不新增 stat/排序。扫描先到也取相同完整历史 score/exists；去重、latest visit、uniq-only 与删除后不复活身份保持。
4. 查询编译计数只在 `cfg(test)` 存在，按 Filter 实例隔离；50k fuzzy 和 CLI OR 每次 refilter 精确 1 次，空查询 0 次。无墙钟阈值，未重做原 15 项变异或全仓性能盘点。
5. 自验 `just fmt-check`、`just shell-lint`、`just test`、`cargo fmt --check`、严格 clippy、`cargo +1.88 test --locked --all` 均 exit 0，完整套件 **312 通过**；release 构建、PTY 文件 **16 项**、release 50k 次数回归通过。证据 `scratch/search-final-gaps-*.log`，详见 TDD 最后节。临时源码副本和本轮脚本清理，既有 scratch 不动，仅保留小日志；没有暂存/提交/安装/真实历史/认证/配置操作。

本节是 implementer 自验，不是独立验收结论，最终仍交 verifier。
