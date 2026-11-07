# SEARCH-DISCOVERY-RANKING-001：可执行验收证据

- 当前状态：**实现与自验完成，待独立复核/待提交**。两处收口闭环：完整安全历史储备让 `CDH_DISCOVER=0`/扫描剪枝/目录符号链接历史在清空查询后可检索；`search_50k` 新增 `query_compilations` 编译次数断言；主会话实测门禁全绿，见末节。此前“再次独立复核：未通过（ready=false）”保留为历史证据；仍待独立 verifier，未暂存、未提交，不代替最终独立验收。

## 范围与安全边界

修复 CLI 查询交接、完整历史发现 anchors、路径意图和通用排序；保留公共 picker/recommend 接口、扫描预算、symlink/excludes 规则及非 TTY 历史契约。没有提交、暂存、推送、安装更新、真实历史/认证读取或配置改动。

当前源码生成 `target/debug/cdh` 和 `target/release/cdh`；未使用安装二进制。PTY 夹具使用仓库 `scratch/cdh-pty-*`，Drop 清理失败路径；release 探针使用 `scratch/search-release-probe-*` 的 TemporaryDirectory。已有库测试使用原有 RAII 基础设施。检查 `/tmp/*cdh*` 无匹配；没有删除不属于本任务的目录。

## 顺序与实际 RED/GREEN

先更新任务/计划，再仅添加能在旧实现编译的回归，运行 RED，然后修改生产逻辑。证据不是对现象的转述。

| 保证 | 承重测试/路径 | 实际 RED | 实际 GREEN |
| --- | --- | --- | --- |
| 裸路径不接纳无关 fuzzy | `search_regression_path_intent_rejects_unrelated_fuzzy_paths` | 原实现 2 条结果而非 1，失败 | 同测试通过；删除路径分支的变异也失败 |
| basename 胜过高历史分祖先后代 | `search_regression_basename_beats_hot_ancestor_match` | 原实现选错下标，失败 | 同测试通过；取消叶目录分类的变异失败 |
| 发现项按浅层结构而非名称/用途排序 | `search_regression_discovery_prefers_shallow_structure`、`structural_order_is_independent_of_names_and_batch_order` | 原实现字典序首项错误，失败 | 同测试通过；额外使用比深层路径更长的浅层路径，删除深度键仍会失败 |
| 初始 CLI 查询交接 | `search_regression_cli_query_is_visible_before_typing`、`cli_query_construction_keeps_or_after_edits_until_clear` | 原实现真实 argv→PTY 首帧无 OR，失败 | 首帧、实际可选路径和 app 构造通过；清空初始化的变异失败 |
| 发现输入独立于 query/limit/threshold | `search_regression_discovery_survives_query_limit_and_threshold`、`scan_paths_collect_raw_and_uniq_before_query_limit_and_threshold` | 初始旧代码 PTY 首先在交接缺失失败；修复后单独恢复“过滤后 items 作 roots”的隔离变异，在外部兄弟目录发现条件失败，非编译失败 | 未访问外部目标在 `needle`、`--limit 1`、`--threshold 2` 下均可选；raw-only/uniq-only 收集与无结果 anchors 单测通过 |
| CLI OR 不变 AND，quoted spaces 不拆分 | `cli_multi_tokens_stay_or_and_quoted_spaces_stay_one_token_on_pty`、`cli_quoted_query_matches_and_highlights_one_literal_token` | OR→AND 和 controller 拆 quoted token 两项隔离变异分别失败 | 真实 alpha/beta 分离历史均可选；单 argv 含空格仅保留一项；编辑仍 OR、清空恢复 AND |
| ignore/excludes 不被完整 roots 绕过 | `scan_paths_keep_ignore_pwd_and_missing_policy_before_query`、`search_app_filters_excluded_anchors_and_actual_ignore_before_ingestion`、`cli_ignore_override_and_excludes_protect_discovery_on_pty` | 新增专项测试，无原始 RED，不冒充旧行为缺陷 | app 构造先过滤 anchors/展示项；批次拒绝排除项且不污染 known_paths；真实 `--ignore-re` 覆盖和 excludes 都选择允许项；夹具文件不变 |
| 路径精确/组件补全/尾斜杠与 HOME/pwd | `paths_expand_lexically_with_component_completion_and_trailing_boundary`、`absolute_tilde_relative_unicode_space_paths_select_literal_target_on_pty` | 路径隔离有原始 RED；扩展语法测试为新增 GREEN | 绝对、字面 tilde、相对、Unicode/空格真实 CLI 可选；尾斜杠不匹配相似目录 |
| 缺失 HOME/pwd 不退回无关 fuzzy | `unresolved_home_or_pwd_never_falls_back_to_unrelated_fuzzy_paths` | 实施走查中新测试实际失败 | 未解析上下文保留字面路径意图，同测试通过 |
| 等强度历史偏好 | `equivalent_basename_strength_keeps_history_preference_even_with_longer_path`、既有 source 测试 | source 变异失败（不是依赖两个完全相同路径的唯一断言） | 长历史路径仍胜短发现路径；历史分只在等强度下决胜 |
| 非 TTY 不扫描，只输出历史最高项/exit 2 | `non_tty_query_remains_history_only_with_no_newline_or_state_changes` | 新增专项测试，无原始 RED | 未访问 HOME 目标存在仍 exit 2/stdout 空；有历史输出精确字节且无换行；历史/设置不变 |
| 异步发现、分页/resize 选择不变量 | 新 app 构造/ingest 测试与既有 `ingest_preserves_selected_path`、`resize_page_size_keeps_current_directory_selected` | 未声称这些旧行为有缺陷 | 初始非空查询、批次重排、被排除路径回流和页面尺寸变化均保留有效选择 |

原始命令与日志：

- `cargo test --locked --all search_regression -- --nocapture`：旧实现库测试 3 失败（cargo 在库失败后不会继续集成测试），`scratch/search-ranking-red.log`。
- `cargo test --locked --test pty_input search_regression -- --nocapture`：旧实现真实 PTY 2 失败，`scratch/search-handoff-red.log`。
- 同组修复后命令全通过，`scratch/search-ranking-green.log`；最终全套见下节。
- `cargo test --locked --lib unresolved_home -- --nocapture`：分别 1 失败/1 通过，`scratch/search-unresolved-{red,green}.log`。

## 定向变异（不是覆盖率替代品）

运行 `python3 -I scratch/search-ranking-mutate.py`，源码只修改 `scratch/search-ranking-mutation-copy/` 隔离副本，没有改写或还原原工作区源码。正对照 exit 0；以下 8 项均 exit 101，出现对应断言失败且没有编译错误：过滤 roots、空初始查询、OR→AND、拆 quoted token、取消 basename 排序、取消深度排序、取消路径隔离、取消历史偏好。

逐项命令与输出保留在 `scratch/search-ranking-mutation-copy/evidence/`；`results.txt` 给出退出码、编译错误检查和预期是否满足。副本是故意变异的测试产物，不是交付源码。新增缺失上下文回归是在该批变异之后单独完成的 RED/GREEN。

## 终端探针的承重性

新集成测试经过真实 controller argv、推荐读取、app 构造、异步 scan、nucleo/字面匹配及 crossterm 输入，不仅断言辅助函数返回值。

等待采用有上限的可观察条件而不是固定 sleep。修正了两次测试探针错误：`dirs` 会误命中页脚 Hidden dirs；直接去 ANSI 无法重建 ratatui 的增量帧与光标跳过的空格。现在重放实际使用的绝对定位/style 子集，并用 `terminal_snapshot_preserves_spaces_unicode_and_incremental_overwrites` 对保留单元、空格与宽 Unicode 作正负断言。这个测试探针不声称是通用 VT100 模拟器。

发现批次刻意保留当前选中路径，因此首项验收显式按 Home 后 Enter，不把“新第一项”与“当前选择”混为一谈。

release 命令：`python3 -I scratch/search-ranking-release-probe.py target/release/cdh`。`--limit 1 --threshold 2 target` 的隔离 HOME/XDG 历史外兄弟目标确实流入界面；Enter 返回精确目标且 exit 0，Ctrl+C 返回空且 exit 1，两次 `tcgetattr` 完整属性均恢复，夹具历史字节不变。输出在 `scratch/search-release-pty-green.log`。本轮没有再注入 abort panic；TermGuard/panic hook 未改。

## 兼容性取舍

- CLI 为大小写不敏感字面 OR，保留 argv 空格/引号/反斜杠边界；输入框以可逆双引号/反斜杠表示，OR 保持到清空。未闭合双引号把余下内容当一个 token，不阻塞编辑。CLI 不解释 fuzzy 操作符。
- 空启动查询/清空后保持 nucleo 的 AND、操作符、Smart Unicode normalization；完整 Pattern 先决定资格，basename 不会绕过否定条件。字面路径不进行 fuzzy 字符跳跃或 normalization 折叠。
- 裸 `/`、`~`/`~/`、`./`/`../` 是路径；`~user` 不展开，普通 `a/b` 在手输模式仍 fuzzy；显式 `^`/`'`/`!` 前缀留给 Pattern。只词法处理、不 canonicalize、不增加 query roots；输出保持历史/发现的原始路径。
- 强度为路径精确、末组件补全、后代前缀、basename 精确/前缀/子串/模糊、祖先匹配。coverage 在类别之后，两个祖先命中不能压过 basename。等强度先 History；同强度 Discovered 按深度、热度、长度、字典序。空查询历史前缀顺序不变。
- 无 `.config`、tests、backups 或具体工具用途权重；扫描深度/cap/时间预算、慢挂载配额和 symlink 不变。
- 既有 `does_not_follow_symlinks`、`count_cap_truncates`、`depth_cap_clamps_cascade_budgets`、`slow_reserve_lets_deep_slow_through_under_cap`、排除根、Unicode grapheme/highlight/窄布局/禁色及设置 golden 测试在完整套件中通过。它们是兼容回归，不宣称本轮新增端到端测试覆盖了每个扫描内部分支。

## 首轮检查、性能与环境缺口

- `just fmt-check`、`just shell-lint`、`just test`：最终均 exit 0；285 个库测试 + 19 个集成测试 = **304 通过、0 失败**（PTY 文件内含 10 个真实终端旅程测试、1 个非 TTY 测试和 1 个增量帧探针测试）。日志 `scratch/search-final-checks.log`。shell 命令 exit 0，但本机无 zsh，两个 zsh payload 明确 SKIP；没有改 shell。
- `cargo clippy --locked --all-targets --all-features -- -D warnings`、`cargo build --locked`、`cargo build --locked --release`：通过。
- `cargo +1.88 test --locked --all`：最终同样 304 通过、0 失败；这是 Cargo.toml 与 CI 声明的真实 MSRV。`cargo +1.74 test --locked --all` exit 101，先失败于原有 lockfile v4，日志 `scratch/search-msrv-174.log`；没有降级依赖，也没有宣称 1.74 整仓兼容。
- `cargo test --locked --release --lib search_50k -- --nocapture`：五万候选全部匹配且首项承重断言通过，实测过滤排序 13.66295 ms（单次本机数据，不是跨机器时限承诺），`scratch/search-50k-release.log`。实现目前在候选循环外构造查询、排序前缓存数值强度/结构键、高亮仅可见行计算，保留 drain-all 后一次 refilter；**此测试只保护匹配数量和首项，不能证明或保护“查询只编译一次”**。再次独立复核已确认逐候选编译变异仍通过，见末节 P3。
- 已实际运行 `LLVM_COV=/usr/bin/llvm-cov-18 LLVM_PROFDATA=/usr/bin/llvm-profdata-18 CARGO_LLVM_COV_TARGET_DIR=target/search-ranking-coverage cargo llvm-cov --locked --lib --summary-only`；仪器化测试通过，但 profile uses version 10/expected 9，merge exit 1。Rust 1.97 使用 LLVM 22.1.6，现有工具仅 LLVM 18.1.3，无匹配 llvm-tools；**没有可信覆盖率百分比，不能证明 >=80%**。未安装任何工具。日志 `scratch/search-coverage-attempt.log`。

首轮环境缺口：匹配 LLVM 工具下的覆盖率、zsh 两个 payload。首轮已有实际 debug/release PTY 验证和库布局回归，但没有新跑所有真实终端尺寸、颜色组合或 abort panic 旅程。

## 独立复核后的定向修复

独立 verifier 未通过首轮验收：被 query/limit/threshold 裁剪的已访问目录重新发现后丢失 History 身份、访问时间与历史删除语义；原 Unicode normalization 测试只有 ASCII 断言，关闭归一化仍能存活。本节为 repair implementer 的实际自验记录，不代替 verifier 再次复核。

### 修改生产逻辑之前的 RED

- 仅补测试后运行 `cargo test --locked --lib rediscovered_pruned_history -- --nocapture`，exit 101、2 项断言失败：浅层未访问目录错误压过已访问深层目录；Ctrl+D 后 raw 中的历史行仍存在。日志 `scratch/search-repair-history-red.log`。
- `cargo test --locked --test pty_input rediscovered_pruned_history -- --nocapture`，exit 101：真实 `cdh target` 正对照通过，随后 `cdh seed` → Ctrl+U → `target` → Home/Enter 错选未访问 HOME/target。日志 `scratch/search-repair-pty-red.log`。
- 新 `smart_unicode_normalization_matches_and_highlights_accented_basename` 在未改生产实现时 GREEN；`python3 -I scratch/search-repair-unicode-mutate.py` 只在隔离源码副本将 Smart 改为 Never，exit 101，明确失败于 `cafe` 必须匹配 `/x/café` 的断言，非编译失败。日志 `scratch/search-repair-unicode-control.log`、`scratch/search-repair-unicode-copy/evidence/normalization-never-red.log`。原 negation/anchoring 测试改为准确名称，不再冒称 Unicode 覆盖。

### 最小修复及 GREEN

- `build_search_app` 复用完整合格 `scan_paths` 保存历史身份与 latest raw visit（uniq-only 为 None），excludes/实际 ignore 仍在注册前过滤；该注册表不抢占 `known_paths`，不增加 roots、不改变扫描资格/预算。
- `ingest_discovered` 恢复登记过的 History 来源和访问时间。恢复项每轮 drain 只做一次 splice，追加到原推荐前缀后，真正 Discovered 才进入结构排序后缀；保留既有选中路径与分页不变量。
- 被裁剪项没有保留下来的推荐分，恢复时 score 为 0，不伪造访问分或重新计算推荐；等强度 History 优先不依赖该分数，原推荐项的分数与顺序不变。未裁剪推荐项仍由既有去重规则保留。
- 删除 History 记录成功后同步撤销该路径的历史身份；取消排除后只能作为无访问时间的 Discovered 回流，不恢复已删除记录。子树中未删除的其他历史身份仍可保留。
- 2 项库回归覆盖 query、limit、threshold 三种裁剪、raw 重复时间取最新、uniq-only 身份、历史/发现前后缀、选择/resize、实际 raw/uniq 删除及取消排除回流。真实 PTY 的单项测试覆盖未裁剪正对照和三种裁剪，均选已访问深层 target，夹具历史字节不变。
- Unicode 回归同时断言 ASCII→accented 资格、basename 强度、精确 scalar 高亮 `[3, 4, 5, 6]`，以及显式 accented 查询不能反向匹配无音符路径，锁住 Smart 的方向性。
- 定向命令 GREEN 日志：`scratch/search-repair-history-green.log`、`scratch/search-repair-pty-green.log`、`scratch/search-repair-unicode-green.log`。首次修复后测试还有一处新增分页断言误用 0 基页码，按既有非空 1 基页码修正，并增加 resize 前后同一选择断言；没有为此改分页生产逻辑。

### 修复后的变异与最终检查

- `python3 -I scratch/search-repair-mutate.py`：正对照 exit 0；清空完整历史身份、丢访问时间、保留已删除身份、关闭 normalization、丢 normalized highlights 五项隔离变异均 exit 101，因对应断言失败且没有编译错误。`scratch/search-repair-mutation-copy/evidence/results.txt` 与逐项日志保留。两轮 repair 变异源码副本及本轮 mutant target 已清理，保留证据；未动首轮 evidence。
- `just fmt-check`、`just shell-lint`、`just test` 全部 exit 0；当前 **288 个库测试 + 20 个集成测试 = 308 通过、0 失败**。完整日志 `scratch/search-repair-full-green.log`。shell 输出仍明确两个 zsh payload SKIP，本轮未改 shell。
- `cargo clippy --locked --all-targets --all-features -- -D warnings`、`cargo +1.88 test --locked --all`（同样 308 全通过）、`cargo build --locked --release` 全部 exit 0；日志 `scratch/search-repair-clippy.log`、`scratch/search-repair-msrv-188-green.log`、`scratch/search-repair-release-build.log`。
- `cargo test --locked --release --test pty_input rediscovered_pruned_history -- --nocapture` exit 0，四个真实 release 场景全通过；日志 `scratch/search-repair-release-pty-green.log`。
- `python3 -I scratch/search-ranking-release-probe.py target/release/cdh` exit 0：Enter/Ctrl+C 完整 termios 恢复、目标发现、夹具历史不变；日志 `scratch/search-repair-release-termios-green.log`。
- `git diff --check` 通过。测试使用既有 RAII/仓库 scratch 夹具，无本轮 PTY/变异源码副本或 `/tmp/*cdh*` 残留。不暂存、提交、更新安装二进制或触碰真实历史/认证/agent 配置。

定向修复阶段没有新增产品假设，沿用已文档化的 CLI OR 到清空与裸相对路径语义。MSRV 1.74/lockfile v4、zsh 缺失、LLVM profile 不兼容且无可信覆盖率百分比的首轮限制仍存在；repair 阶段未安装工具或重试覆盖率。上述为当时自验通过记录；再次独立 verifier 的实际未通过结论如下，不以此节 GREEN 替代最终验收。

## 再次独立复核：未通过（ready=false）

独立 verifier 未改生产源码，验证当前源码构建而非安装版本。结论为 **308 项测试与核心变异通过，但历史回流修复仍不完整，另有一项性能测试声明没有承重断言**。本轮 docs closeout 仅更新任务登记、计划与本证据文档，不改源码，不标为完成、已验证待提交或已提交。

### 剩余失败与 RED/GREEN 边界

| 保证/剩余问题 | 独立复现或变异 RED | 对照/已通过范围 | 当前 GREEN 状态 |
| --- | --- | --- | --- |
| **P2 清空启动查询后可检索全部安全合格历史** | 隔离 release PTY：历史含 HOME/node_modules 与 HOME/seed，无 ignore/excludes；`cdh seed` → Ctrl+U → `node_modules` 显示 Page 0/0；符号链接目录与 `CDH_DISCOVER=0` 同样失败。日志 `scratch/verifier-release-probe.log`；原 HEAD 三场景也失败，`scratch/verifier-head-history-probe.log` | 每场景直接用目标启动都可选；可重新扫描的裁剪历史身份/时间/删除及历史偏好现有测试仍 GREEN | **尚无修复后 GREEN**；旧缺陷未修完整，而非新回归。不能以可重新扫描场景的 GREEN 声称全部历史回流已修复 |
| **P3 refilter 查询只编译一次** | 隔离副本将查询构造移进候选循环执行 50,000 次，`search_50k_candidates_compile_query_once_and_rank_all_matches` 仍 exit 0，1 通过/0 失败；变异存活。日志 `scratch/verifier-compile-per-candidate.log` | 原测试保护匹配数量与首项，单次 release 数据 13.66295 ms 仍保留为测量，不是次数证明；变异本机 debug 数据 175.319159 ms 同样不是时限断言 | **没有承重次数断言或相应 RED→GREEN**；需要可确定的次数断言，不能把测试名或机器耗时当证据 |

P2 定位为 `src/picker.rs:2006,2014`：池只由裁剪后的 items 构建，完整历史仅登记身份，必须等待 `ingest_discovered` 才恢复候选；`src/discover.rs:486,491` 跳过剪枝名/符号链接目录，发现关闭时也无批次。P3 定位为 `src/picker_tests.rs:4906` 与 `src/picker.rs:772`。行号为本次 verifier 快照，不是稳定内容定位。后续修复须保留存在性/ignore/excludes、安全扫描与非 TTY 契约，分别补 P2 三场景及 P3 次数断言；本次仅记录，不实施修复。

我假设清空启动查询后应能重新检索安全合格历史；若产品有意永久裁剪不可扫描历史，需要人类确认并明确记录兼容限制。在未确认或未修复前保持 P2 阻塞。CLI OR 到清空及裸相对路径语义沿用当前文档，没有扩张产品范围。

### 独立正对照与 15 项核心变异

`python3 -I scratch/verifier-mutate.py` 的正对照 exit 0。以下全部 exit 101，出现对应断言失败且无编译错误；与上文首轮和 repair RED/GREEN 同一保证对应，不改写已有证据：

| 保证映射 | 隔离变异/证据（均在 `scratch/verifier-mutations/`） | 结果 |
| --- | --- | --- |
| 完整 roots、初始 CLI 查询 | `filtered-roots.log`、`empty-query.log` | 2 项被捕获 |
| CLI OR、quoted argv 边界 | `or-to-and.log`、`split-quoted.log` | 2 项被捕获 |
| basename 强度、通用深度、路径隔离、等强度历史偏好 | `basename.log`、`depth.log`、`path-isolation.log`、`source.log` | 4 项被捕获 |
| 扫描回流的历史身份、最新访问时间、删除身份 | `identity.log`、`visit.log`、`deleted-identity.log` | 3 项被捕获；不覆盖 P2 的无扫描批次场景 |
| Unicode Smart normalization 与高亮 | `normalization.log`、`highlights.log` | 2 项被捕获 |
| ignore 批次保护、缺失路径上下文隔离 | `ignore-ingest.log`、`missing-context.log` | 2 项被捕获 |

逐项退出码和编译错误判据见 `scratch/verifier-mutations/results.txt`；合计 **15 项被断言捕获**。P3 逐候选编译变异单独存活，不混入这 15 项，也不算覆盖率。

### 独立检查、release 与环境限制

- `cargo test`、`cargo test --locked --all`、`cargo +1.88 test --locked --all`：各 **288 库 + 20 集成 = 308 通过、0 失败**；`cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`git diff --check` 通过。命令及输出 `scratch/verifier-required-checks.log`。
- 当前源码 release 构建通过；`cargo test --locked --release --test pty_input` 的文件全部 **13 项通过、0 失败**（含真实 argv、OR/quoted spaces、Unicode 路径、排序、安全过滤与历史偏好；不把探针/非 TTY 项冒称 13 个真实终端旅程）。日志 `scratch/verifier-release-pty.log`。
- `python3 -I scratch/verifier-release-probe.py target/release/cdh` 的额外隔离探针：HOME 外未访问兄弟目标在空启动、CLI query、limit、threshold 四场景均可选；绝对/tilde/相对 Unicode 空格路径排除无关结果；空历史交互发现与非 TTY exit 2 保持。Enter/Ctrl+C 完整 termios 恢复、隔离历史字节不变。脚本及 JSON 输出 `scratch/verifier-release-probe.py`、`scratch/verifier-release-probe.log`；同一日志内 P2 三场景明确失败，不报告整份探针全部通过。
- `cargo +1.74 test --locked --all` 实跑 exit 101，因既有 lockfile v4 阻塞；Cargo.toml 已声明 MSRV 1.88，HEAD 同样如此，不是本轮回归，不声称 1.74 兼容。
- `just shell-lint` exit 0，但工具缺失导致两个 zsh payload 明确 SKIP，不能记为已验证。
- 独立覆盖率尝试中 **288 项仪器化库测试通过**，但 LLVM profile v10 与工具要求 v9 不兼容，merge 失败；日志 `scratch/verifier-coverage.log`。**没有可信覆盖率百分比，不能证明 >=80%**；首轮环境缺口未解决，未安装工具。
- 未新增 abort panic 终端旅程，也未声称遍历所有终端尺寸/颜色组合。

### 文档收尾检查与产物归属

本轮 docs closeout 重跑 `just fmt-check`、`just shell-lint`、`just test` 均 exit 0；格式检查包括 Rust 与 shell，shell 仍明确 SKIP 两个 zsh payload。`git diff --check` 通过，并检查 `git status --short` 与 `git diff --stat`；三个任务文档仍为未跟踪文件，tracked diff 统计不会包含它们。只改本任务三份文档，保留其他工作区源码、README、测试改动；无暂存、提交、推送、安装更新或真实历史/认证/配置操作。

verifier 的临时源码/构建副本及 HOME/XDG 夹具已清理，`/tmp/*cdh*` 检查无残留。本轮未创建仓库 scratch 夹具，未删除任何其他代理的产物；仍保留的既有内容如下，不能泛称 scratch 已清空：

- `scratch/verifier-*.py`、`scratch/verifier-*.log` 与 `scratch/verifier-mutations/`：独立复现、HEAD 对照、性能变异、门禁/release/覆盖率证据。
- `scratch/search-ranking-mutation-copy/`：首轮隔离源码副本（仍有 src/tests/scratch）及 evidence；`scratch/search-repair-mutation-copy/`、`scratch/search-repair-unicode-copy/`：保留 Cargo 元数据和 evidence，repair 变异源码及本轮 mutant target 已清理。
- `scratch/search-*.py`、`scratch/search-*.log`、`scratch/search-build-sha256.txt`：首轮及 repair 的 RED/GREEN、构建、MSRV、终端和性能原始记录。

以上均为既有证据或副本，不属于 docs closeout 可自行清理的产物。当时状态为 **阻塞（ready=false）**；当时绿色门禁与已捕获核心变异不消除 P2/P3。后续本轮仅针对这两项实施，记录如下。

## 最后两项验收缺口：implementer 本轮 RED→GREEN（待独立 verifier）

用户明确完整安全历史可检索是正常契约，无需再确认。只改推荐完整历史 metadata、controller 交接、picker 储备/测试计数、必要回归及三份任务文档；已有 README、i18n、lib/search 等工作区改动保留，没有重做调研、15 项核心变异或全仓性能盘点。独立复核通过并经主会话逐文件向用户讲解前不暂存/提交/推送/开 PR；已获用户授权：验收通过并讲解后自动走 commit → push → PR → CI 全绿 → merge main（main 分支要求 6 个 status check 全绿、无需人工批准、仓库未启用 auto-merge，走普通 PR 合并），由主会话安排，本 implementer 不自行执行。安装 binary、真实历史、认证、agent 配置/权限边界不变。

### 先新增实际 RED，再改生产 GREEN

| 保证 | 实际 RED | 本轮 GREEN |
| --- | --- | --- |
| 不依赖任何扫描批次的完整历史及分数 | `cargo test --locked --lib cleared_query_restores -- --nocapture` exit 101：清空/输入 target 后匹配为 0 而非 2；`scratch/search-final-gaps-history-red.log` | 同命令 exit 0，覆盖 query、limit、threshold；比较无裁剪历史推荐分，保留原启动项分数、History/latest visit/uniq-only/exists、去重及历史前缀；`scratch/search-final-gaps-history-green.log` |
| 无发现、扫描剪枝名、目录符号链接真实回流 | `cargo test --locked --test pty_input cleared_query_returns -- --nocapture` exit 101、3 项失败；每项直接用目标启动的正对照先成功，再 `cdh seed` → Ctrl+U → 目标失败；`scratch/search-final-gaps-pty-red.log` | 同命令 exit 0、3 项通过；每项包含直接启动、query、limit、threshold 四场景，按实际可见目标等待再 Home/Enter，返回原始路径，隔离 raw/uniq 字节不变；`scratch/search-final-gaps-pty-green.log` |
| 50k 查询编译次数承重 | 新增 `cfg(test)` Filter 实例级 Cell 计数与次数断言后，正对照 exit 0；隔离源码副本仅把 `self.query(query)` 移入候选循环，测试 exit 101，明确 `left: 50000 / right: 1`，无编译错误；`scratch/search-final-gaps-compile-control.log`、`scratch/search-final-gaps-compile-red.log` | 当前源码 debug 全库与 release `search_50k` 均 exit 0；fuzzy、CLI OR 每次 refilter 精确 1 次，空查询 0 次，仍保护全部 50,000 匹配和 project-0 首项；`scratch/search-final-gaps-compile-green.log`。不使用墙钟阈值 |

RED 阶段仅添加回归与测试计数观察点，没有先改候选修复。P3 原生产实现本来已在循环外编译；其 RED 来自隔离变异，不能冒称原查询算法失败。隔离副本使用仓库 scratch，由脚本新建唯一目录、finally 清理；不创建/保留第二套构建树。最后将新增库夹具从既有 TempDir 移到 repo scratch，复用唯一名字与 AppContext 构造，RAII 清理；为避免 repo 前缀的无关 fuzzy 命中，数量断言使用显式字面子串 `'target`。中途一处 PathBuf 类型限定错误已修复；共享 target 的隔离变异还暴露了 Cargo 包指纹复用，曾误运行变异库产物，已更新本轮 picker 源码时间戳强制重建真实工作区，再重跑全部门禁与 release；不把这些中间编译/探针失败当成成功变异。最终变异日志明确是 50000 != 1 的断言 RED，后续真实源码 50k GREEN。

### 最小修复、分数与兼容边界

- `RecommendationInput.history_items` 保存 query/limit/threshold 之前的完整安全历史与真实 score/exists。raw/uniq 各仍只读一次、存在性缓存仍只按唯一路径 stat；有启动 query 才维护独立完整信号，原过滤信号的归一化、uniq 名次、防抖及转移保持。统一评分公式不重复排序；空启动 query 直接复用已计算的分数。非 TTY 不收集完整历史/anchors。
- controller 仅交接完整 metadata，公开 recommend/picker 签名、非 TTY top 历史/无换行/exit 2 保持。储备分数按无 query/limit/threshold 的全历史计算，启动推荐项仍保留其原查询相关分数；不会像此前扫描恢复那样把被裁剪项分数设成 0。
- `build_search_app` 同时过滤 items、anchors 与 metadata 的实际 ignore/excludes。完整储备按已排序 scan_paths 建立，不再排序或 stat；安装在初始 CLI 查询计算之后，首帧继续保留原 limit/threshold 的意义。首次编辑/清空时一次 splice 恢复安全历史、一次 refilter；初始空 query 的 Ctrl+U 也能恢复 limit 裁剪储备。既有扫描先发射时使用同一历史分数与存在性，去重保证后续清空不重复。
- 完整储备恢复仍为 History，raw 重复时间取最新、uniq-only 为 None；原推荐前缀顺序不变，恢复项在历史前缀末尾。删除同时撤销身份、分数和待恢复项，取消排除不能复活已删除历史。原删除回归现在分别验证扫描恢复与无扫描清空恢复；ignore/excludes 回归也实际检查储备恢复后没有排除/忽略项的 known_paths 或身份/分数污染。推荐安全回归覆盖 missing 的默认拒绝及 include_missing 下 exists=false。
- 编译观察点只在 `cfg(test)` 存在、按 Filter 实例隔离，不引入进程全局计数竞争或 release 热路径成本。没有改变扫描 roots/预算/pruning/symlink 规则、终端布局或 TermGuard。

### 完整自验结果与环境缺口

- `just fmt-check`、`just shell-lint`、`just test`（即 `cargo test --locked --all`）、`cargo fmt --check`、`cargo clippy --locked --all-targets --all-features -- -D warnings`、`cargo +1.88 test --locked --all` 全 exit 0。完整套件每轮 **289 库 + 23 集成 = 312 通过、0 失败**；命令原文与输出 `scratch/search-final-gaps-required-checks.log`。MSRV 保持仓库实际 1.88，不重跑或降级到 1.74。
- 定向全库 289 项与 debug PTY 文件 16 项均通过，`scratch/search-final-gaps-lib-green.log`、`scratch/search-final-gaps-pty-all-green.log`；单独严格 clippy 通过，`scratch/search-final-gaps-clippy.log`。
- `cargo build --locked --release` exit 0；`cargo test --locked --release --test pty_input` **16 项通过、0 失败**（新增 3 项各包含上述四场景）；`cargo test --locked --release --lib search_50k -- --nocapture` 1 项通过。日志 `scratch/search-final-gaps-release-build.log`、`scratch/search-final-gaps-release-pty.log`、`scratch/search-final-gaps-compile-green.log`。真实 PTY 使用当前源码构建而非安装 binary。
- zsh 缺失的两个 payload 仍明确 SKIP；既有 LLVM profile v10/工具 v9 的覆盖率缺口不重试、没有可信百分比、不声称 >=80%。不安装工具，未新增 abort panic 或全部尺寸/颜色组合验证。
- 本轮隔离源码副本 `scratch/search-final-gaps-compile-copy/` 已由 finally 删除，本轮一次性 mutation 脚本也清理；无独立构建副本，PTY fixture 由现有 RAII 放在 repo scratch 并清理。保留 `scratch/search-final-gaps-*.log` 小证据，不动来源不明或其他代理的既有 scratch。检查无本轮 fixture 与 `/tmp/*cdh*` 残留，`git diff --check` 通过。

无新增需人类确认的产品假设；首次编辑/清空恢复储备与启动项/储备项分数基准如上明确。所有原工作区改动保留，没有 stage/commit/push/PR、安装、真实历史、认证或 Claude 配置操作。**本节仅是 implementer 自验；仍需独立 verifier，不自称最终完成。**

## 两处收口确认与主会话实测门禁（实现与自验完成）

### 收口 1：完整安全历史储备在清空查询后可检索

保证从“仅登记身份、依赖扫描批次发射”收口为“不依赖任何扫描批次”：首次编辑/清空查询时一次 splice 恢复完整安全历史候选，`CDH_DISCOVER=0`、扫描剪枝目录名与目录符号链接三类历史在清空查询后均可检索；恢复项按 History 前缀追加，不进入发现结构排序，保留分数/时间/身份/删除/安全与初始裁剪语义。RED→GREEN 摘要（由实现代理记录，完整命令见上表与 `scratch/search-final-gaps-*.log`）：库回归 `cleared_query_restores_safe_history_without_discovery_and_keeps_scores` 修复前 exit 101（清空/输入 target 后匹配为 0 而非 2），修复后 exit 0；真实 PTY `cleared_query_returns` 修复前 3 项失败且每项直接启动正对照先 GREEN，修复后 3 项全通过，各含直接启动与 query/limit/threshold 四场景，返回原始路径，隔离 raw/uniq 字节不变。

### 收口 2：search_50k 编译次数承重

`search_50k` 新增 `query_compilations` 确定次数断言（`cfg(test)` 且按 Filter 实例隔离计数，不引入热路径成本）：fuzzy 与 CLI OR 每次 refilter 精确编译 1 次、空查询 0 次，仍保护全部 50,000 匹配与首项。RED→GREEN 摘要（由实现代理记录，完整命令见上表与 `scratch/search-final-gaps-compile-*.log`）：隔离副本把查询构造移入候选循环后次数断言 RED（`left: 50000 / right: 1`）且无编译错误；当前真实源码 debug 全库与 release `search_50k` 均 GREEN，无墙钟阈值。

### 收口 3：纯空白 argv 不再作为字面 token（verifier 复核新发现，已修复并补回归）

独立 verifier 在本分支源码上发现 P3 回归：`controller.rs` 关键字 token 处理为 `if !a.is_empty() { opt.tokens.push(a); }` 时，纯空白 argv（`cdh " "`）被当作字面 token，非 TTY 从“空查询 → 输出最佳历史 exit 0”退化为“无候选 exit 2”。本例事实：修复为 `if !a.trim().is_empty()`，只丢弃纯空白参数、仍 push 原始 `a` 保留 argv 边界（`"alpha beta"` 词内空格原样保留），空白参数等同空查询、不会作为 TUI 初始查询；交互语义不变。回归测试 `tests/tui_settings_non_tty.rs` 新增 `blank_argv_matches_best_history_like_empty_query`（有历史时 `cdh " "` 与 `cdh ""` 均 exit 0 且 stdout 恰为最佳历史路径、无换行）与 `blank_argv_with_empty_history_still_exits_two`（无历史时 `cdh " "` 仍 exit 2）。变异验证：暂时恢复旧条件后该测试明确 RED（`cdh " "` exit 2），修复后 GREEN。门禁：`cargo test --locked --test tui_settings_non_tty` 4 项通过；完整 `cargo test --locked --all`（含本项后 289 库 + 25 集成 = 314 通过、0 失败）、`cargo fmt --check`、严格 clippy、`cargo +1.88 test --locked --all`、`cargo build --locked --release`、`git diff --check` 全绿。

### 主会话实测门禁

主会话在当前源码上实测：`cargo test --locked --all` **312 通过、0 失败**；`cargo fmt --check`、严格 clippy、`cargo +1.88 test --locked --all`、`cargo build --locked --release`、`git diff --check` 均 exit 0。`README.md` 与 `README.en.md` 均已改动，CI 的 README 双语同步检查会通过。既有环境缺口保留：无 zsh（两个 zsh payload SKIP）、LLVM profile v10/v9 不兼容无可信覆盖率百分比、MSRV 实际为 1.88、lockfile v4。

### 授权边界

已获用户授权：独立复核通过后，由主会话逐文件向用户讲解，再自动 commit → push → PR → CI → merge main；已确认 main 分支保护要求 6 个 status check 全绿、无需人工批准，仓库未启用 auto-merge，将走普通 PR 合并。讲解与验收完成前不 stage/commit/push；Git 操作由主会话安排，本实现代理不自行执行。
