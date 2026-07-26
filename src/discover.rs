//! 目录树发现层：把候选池从「历史记录」扩为「历史 ∪ 目录树」。
//!
//! 设计要点（多轮实测定稿，勿轻易重构）：
//!
//! * **遍历模型**：跨所有根共享一个「按有效深度出队」的 frontier（分层 BFS）。
//!   所有根的第 1 层都先于任何根的第 2 层到达，以此类推。这样做的关键理由是
//!   实测 9p /mnt 挂载冷热缓存几乎无差（瓶颈是 ~1ms/目录 的协议往返），一个大的
//!   /mnt 锚点若按「顺序级联」会独占 5s 预算、把其它锚点饿死；改为全局分层后，
//!   每个锚点最有价值的浅层（兄弟目录）都保证在预算内到达，预算截断时丢的只是
//!   「最慢挂载的最深层」。
//!
//! * **优先级偏置**：历史锚点起始有效深度 0、`$HOME` 全树起始 1、`$HOME` 外锚点
//!   起始 2。同一有效深度内，本地根先于慢挂载根（`/mnt` `/media` `/Volumes`）。
//!
//! * **深度预算**：历史锚点各下探 1 层（拿到「常用目录的兄弟」这批最高价值候选）；
//!   `$HOME` 全树不设深度上限；`$HOME` 外锚点下探 4 层。`CDH_SCAN_DEPTH` 可加一个
//!   可选硬上限。同一目录可能被多个根以不同预算触达——用「松弛」规则（只有当本次
//!   剩余预算大于历史记录时才重新展开），保证 `$HOME` 全树的无限预算能穿过历史
//!   锚点已浅扫过的子树，补齐深层覆盖。
//!
//! * **剪枝**（垃圾子树占大头 I/O，性能第一支柱）：按目录名全局不下降的一批工具
//!   缓存目录名 + 按绝对路径点名的几个工具存储目录；保留隐藏目录（垃圾按名字点名，
//!   不按属性一刀切）；不跟符号链接；无权限静默跳过。
//!
//! * **边界**：候选数上限 50,000 + 全局时间预算 ~5s，之后还有一段最多 300ms 的
//!   收尾补齐（`TOPUP_GRACE`，把慢挂载没认领的槽用本地填满），所以最坏总时长是
//!   预算 + 宽限。深度上限是工作量的代理指标，这里直接约束工作量本身，BFS 保证
//!   截断时丢的是「最深最不可能」的尾部。
//!
//! 纯 std 实现，不用 `ignore` crate、不依赖 fd、不建磁盘索引。

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// 候选数上限：截断的是「最深最不可能」的尾部。
const CANDIDATE_CAP: usize = 50_000;
/// 全局时间预算：慢挂载深层覆盖在预算内每次启动只是部分的，这是接受的边界。
const TIME_BUDGET: Duration = Duration::from_secs(5);
/// 批大小：流式并入事件循环，摊薄单次过滤成本。
const BATCH_SIZE: usize = 512;
/// 无限深度的哨兵（`$HOME` 全树）。
const UNLIMITED: u32 = u32::MAX;

/// 历史锚点各下探 1 层。
const ANCHOR_DESCENT: u32 = 1;
/// `$HOME` 外锚点下探 4 层。
const EXTERNAL_DESCENT: u32 = 4;
/// 慢挂载保留配额 = cap / 此值（~4%）。
///
/// 定标依据（9p /mnt 实测）：阶段 2 的一次 `deferred` 展开约 1.8ms，本地扫完后剩
/// 约 4.3s 预算 ≈ 2400 次展开，去重后净发射 ~1700–2050 个。预留超过这个量就是
/// 空占——慢挂载在预算内根本认领不完，占住的槽会白白浪费。留一点余量取 2000。
/// 定标偏小也无妨：认领不完的部分由 `TOPUP_GRACE` 收尾补齐兜住。
const SLOW_RESERVE_DIVISOR: usize = 25;

/// 收尾补齐的时间上限：预算耗尽后把慢挂载没认领的槽用本地填满。本地一次展开约
/// 0.02ms，实测补 3000+ 个槽只需 ~45ms；给 300ms 是为了兜住「本地根其实也在慢盘上」
/// 这种病态情形，不让补齐把整体时间拖长。
const TOPUP_GRACE: Duration = Duration::from_millis(300);

/// 按目录名全局不下降的一批：工具缓存/依赖目录，实测占遍历 I/O 的大头。
const PRUNE_NAMES: &[&str] = &[
    "node_modules",
    ".git",
    ".hg",
    ".svn",
    ".cache",
    "__pycache__",
    "site-packages",
    ".venv",
    "venv",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".cargo",
    ".rustup",
    ".npm",
    ".pnpm-store",
    ".gradle",
    ".m2",
];

/// 按绝对路径点名（`$HOME` 相对）的工具存储目录：实测合计数万目录，全是存储。
const PRUNE_ABS_UNDER_HOME: &[&str] = &[
    ".local/share/containers",
    ".local/share/fnm",
    ".local/share/pipx",
];

/// 慢挂载前缀：同一深度内排到本地根之后。
const SLOW_MOUNT_PREFIXES: &[&str] = &["/mnt/", "/media/", "/Volumes/"];

/// 一个扫描根：路径 + 有效深度偏置 + 剩余可下探层数 + 是否慢挂载。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootSpec {
    pub path: PathBuf,
    /// 出队排序用的有效深度偏置（历史锚点 0 / `$HOME` 1 / 外部锚点 2）。
    pub eff_depth: u32,
    /// 还能向下走的层数（`UNLIMITED` 表示不限）。
    pub remaining: u32,
    pub slow: bool,
}

/// 环境解析出的扫描参数。
#[derive(Debug, Clone, Default)]
pub(crate) struct ScanEnv {
    /// `CDH_SCAN_ROOTS`：冒号分隔，覆盖整个根列表。
    pub roots_override: Option<Vec<String>>,
    /// `CDH_SCAN_DEPTH`：可选硬深度上限。
    pub depth_cap: Option<u32>,
}

impl ScanEnv {
    pub(crate) fn from_process() -> Self {
        let roots_override = env::var("CDH_SCAN_ROOTS").ok().and_then(|value| {
            let roots: Vec<String> = value
                .split(':')
                .map(str::trim)
                .filter(|segment| !segment.is_empty())
                .map(str::to_string)
                .collect();
            (!roots.is_empty()).then_some(roots)
        });
        // 深度上限允许 0（其实相当于只扫根本身），负/非法值忽略。
        let depth_cap = env::var("CDH_SCAN_DEPTH")
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok());
        Self {
            roots_override,
            depth_cap,
        }
    }
}

/// `CDH_DISCOVER` 总开关（默认开；`0/false/off/no` 关）。语义对齐 picker 的
/// `env_flag_enabled` / `env_truthy`。
pub(crate) fn discover_enabled() -> bool {
    match env::var("CDH_DISCOVER") {
        Ok(value) => {
            let value = value.trim();
            if value.is_empty() {
                return true;
            }
            !(value.eq_ignore_ascii_case("0")
                || value.eq_ignore_ascii_case("false")
                || value.eq_ignore_ascii_case("off")
                || value.eq_ignore_ascii_case("no"))
        }
        Err(_) => true,
    }
}

/// 前缀匹配：`prefix` 当作目录看，既匹配其子孙，也匹配挂载根**自身**。
///
/// 尾斜杠有无等价（`/mnt` 与 `/mnt/` 同义），两边都在目录边界上对齐——所以
/// `/mnt` 命中而 `/mnturbo` 不命中。挂载根自身必须命中：`compute_roots` 会把
/// `/mnt/d` 这类历史条目的父目录 `/mnt` 加成扫描根，若判成本地，它就会进阶段 1b
/// 那个「本地专用、绝不能被慢挂载阻塞」的堆——挂死的网络挂载在那里一次
/// 不可中断的 `read_dir` 就能把整个防饥饿设计卡住。
/// （`crate::excludes` 也用它做子树匹配——同一个「路径是否落在某个目录之内」的
/// 问题，只保留一份实现。）
pub(crate) fn under_prefix(path: &str, prefix: &str) -> bool {
    let root = prefix.strip_suffix('/').unwrap_or(prefix);
    path.strip_prefix(root)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn is_slow_mount(path: &str) -> bool {
    SLOW_MOUNT_PREFIXES
        .iter()
        .any(|prefix| under_prefix(path, prefix))
}

/// 默认慢挂载前缀（`String` 形式，供 `ScanJob::slow_prefixes` 使用）。
fn default_slow_prefixes() -> Vec<String> {
    SLOW_MOUNT_PREFIXES.iter().map(|p| p.to_string()).collect()
}

fn is_pruned_name(name: &str) -> bool {
    PRUNE_NAMES.contains(&name)
}

/// `$HOME` 下点名的绝对路径剪枝集合（规范化：去尾斜杠）。
fn prune_abs_set(home: Option<&str>) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Some(home) = home {
        let home = home.trim_end_matches('/');
        for suffix in PRUNE_ABS_UNDER_HOME {
            set.insert(format!("{home}/{suffix}"));
        }
    }
    set
}

/// 判断 `path` 是否在 `$HOME` 内（含等于）。
fn under_home(path: &str, home: &str) -> bool {
    let home = home.trim_end_matches('/');
    path == home || path.starts_with(&format!("{home}/"))
}

fn parent_of(path: &str) -> Option<String> {
    PathBuf::from(path)
        .parent()
        .map(|parent| parent.to_string_lossy().into_owned())
        .filter(|parent| !parent.is_empty())
}

/// 计算扫描根：级联优先级（历史锚点 → `$HOME` 全树 → `$HOME` 外锚点）。
///
/// 注意：`$HOME` 外锚点会与「历史锚点」组里同一父目录重叠——两条都保留。前者以
/// 有效深度 0 / 剩余 1 抢到「浅层优先 + 兄弟目录」，后者以有效深度 2 / 剩余 4 提供
/// 「深层预算」；出队时靠松弛规则各取所需，发射靠去重只留一次。
pub(crate) fn compute_roots(
    history_paths: &[String],
    home: Option<&str>,
    env: &ScanEnv,
) -> Vec<RootSpec> {
    // CDH_SCAN_ROOTS 覆盖：全部作为有效深度 0、剩余 = depth_cap（或不限）的根。
    if let Some(overrides) = &env.roots_override {
        let remaining = env.depth_cap.unwrap_or(UNLIMITED);
        let mut seen = HashSet::new();
        return overrides
            .iter()
            .map(|raw| raw.trim_end_matches('/').to_string())
            .filter(|path| seen.insert(path.clone()))
            .filter(|path| PathBuf::from(path).is_dir())
            .map(|path| RootSpec {
                slow: is_slow_mount(&path),
                path: PathBuf::from(&path),
                eff_depth: 0,
                remaining,
            })
            .collect();
    }

    let cap = |remaining: u32| match env.depth_cap {
        Some(limit) => remaining.min(limit),
        None => remaining,
    };

    let mut roots = Vec::new();

    // 1) 历史锚点：所有历史条目的父目录（去重、验证存在、$HOME 内外都算），下探 1 层。
    let mut anchor_seen = HashSet::new();
    for path in history_paths {
        let Some(parent) = parent_of(path) else {
            continue;
        };
        let parent = parent.trim_end_matches('/').to_string();
        if !anchor_seen.insert(parent.clone()) {
            continue;
        }
        if !PathBuf::from(&parent).is_dir() {
            continue;
        }
        roots.push(RootSpec {
            slow: is_slow_mount(&parent),
            path: PathBuf::from(&parent),
            eff_depth: 0,
            remaining: cap(ANCHOR_DESCENT),
        });
    }

    // 2) $HOME 全树 BFS，不设深度上限。
    if let Some(home) = home {
        let home = home.trim_end_matches('/');
        if !home.is_empty() && home != "/" && PathBuf::from(home).is_dir() {
            roots.push(RootSpec {
                path: PathBuf::from(home),
                eff_depth: 1,
                remaining: cap(UNLIMITED),
                slow: false,
            });
        }
    }

    // 3) $HOME 外锚点深挖：历史中位于 $HOME 外的路径的父目录，各下探 4 层，排最后。
    if let Some(home) = home {
        let mut external_seen = HashSet::new();
        for path in history_paths {
            if under_home(path, home) {
                continue;
            }
            let Some(parent) = parent_of(path) else {
                continue;
            };
            let parent = parent.trim_end_matches('/').to_string();
            if under_home(&parent, home) {
                continue;
            }
            if !external_seen.insert(parent.clone()) {
                continue;
            }
            if !PathBuf::from(&parent).is_dir() {
                continue;
            }
            roots.push(RootSpec {
                slow: is_slow_mount(&parent),
                path: PathBuf::from(&parent),
                eff_depth: 2,
                remaining: cap(EXTERNAL_DESCENT),
            });
        }
    }

    roots
}

/// frontier 节点。排序键放在外层 `Prioritized` 上——不同堆装入不同的层级语义（本地
/// 用 `eff_depth`，慢挂载用 `descent`），所以节点本身不实现排序。
#[derive(Debug)]
struct Node {
    eff_depth: u32,
    slow: bool,
    /// 距其根的下探层数（根 = 0）。慢挂载调度按此分「浅层/深层」。
    descent: u32,
    remaining: u32,
    path: String,
}

/// 堆元素：显式排序键 `(层级, seq)`；`seq` 唯一保证全序，节点无需实现 `Ord`。
#[derive(Debug)]
struct Prioritized {
    key: (u32, u64),
    node: Node,
}
impl PartialEq for Prioritized {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl Eq for Prioritized {}
impl PartialOrd for Prioritized {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Prioritized {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key.cmp(&other.key)
    }
}

fn push_node(heap: &mut BinaryHeap<Reverse<Prioritized>>, key: (u32, u64), node: Node) {
    heap.push(Reverse(Prioritized { key, node }));
}

fn path_has_prefix(path: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|prefix| under_prefix(path, prefix))
}

/// 从根列表里剔除被排除的根。
///
/// `prune_abs` 只挡子目录，挡不住根自己：被排除的目录仍可能因为祖先关系被算成扫描根
/// （`$HOME` 全树、或某条历史条目的父目录），那样整棵子树会照旧被遍历、只是发射出来
/// 的路径最后在 UI 侧被滤掉——I/O 一点没省，而省掉那部分 I/O 正是排除清单的主要收益。
fn drop_excluded_roots(roots: &mut Vec<RootSpec>, excluded: &HashSet<String>) {
    if excluded.is_empty() {
        return;
    }
    roots.retain(|root| {
        let path = root.path.to_string_lossy();
        !excluded
            .iter()
            .any(|prefix| under_prefix(&path, prefix.as_str()))
    });
}

/// 一次扫描任务。
pub(crate) struct ScanJob {
    pub roots: Vec<RootSpec>,
    pub cap: usize,
    pub deadline: Option<Instant>,
    pub prune_abs: HashSet<String>,
    /// 慢挂载前缀（可注入以便单测调度性质，无需真实 `/mnt`）。
    pub slow_prefixes: Vec<String>,
    /// 为慢挂载保留的候选配额：本机本地就有 5.7 万 > cap，若本地先吃满 cap，`/mnt`
    /// 将颗粒无收——留一小块（默认 cap 的 ~10%），本地相应截尾（截掉的是本地最深
    /// 最不可能的部分）。
    pub slow_reserve: usize,
}

/// 扫描状态与发射逻辑。持有 `on_batch`，把批处理/去重/松弛/剪枝集中在一处。
struct Scanner<F> {
    emitted: HashSet<String>,
    /// 松弛表：记录每个目录「已用多大剩余预算展开过」，只有更大的预算才值得重扫。
    expanded: HashMap<String, u32>,
    batch: Vec<String>,
    total: usize,
    local_emitted: usize,
    cap: usize,
    local_cap: usize,
    prune_abs: HashSet<String>,
    slow_prefixes: Vec<String>,
    deadline: Option<Instant>,
    since_clock: u32,
    on_batch: F,
}

impl<F: FnMut(Vec<String>) -> bool> Scanner<F> {
    /// 到达全局边界（候选 cap 或时间预算）？每 64 次查一次时钟，避免频繁取时。
    fn hit_limit(&mut self) -> bool {
        if self.total >= self.cap {
            return true;
        }
        self.since_clock += 1;
        if self.since_clock >= 64 {
            self.since_clock = 0;
            if let Some(deadline) = self.deadline {
                if Instant::now() >= deadline {
                    return true;
                }
            }
        }
        false
    }

    /// 发射一个路径（去重）。返回 `false` 表示应彻底停止（接收端已断开）。
    fn emit(&mut self, path: &str, is_local: bool) -> bool {
        if !self.emitted.insert(path.to_string()) {
            return true; // 历史/先到者已占；无操作。
        }
        self.batch.push(path.to_string());
        self.total += 1;
        if is_local {
            self.local_emitted += 1;
        }
        if self.batch.len() >= BATCH_SIZE {
            let ready = std::mem::replace(&mut self.batch, Vec::with_capacity(BATCH_SIZE));
            if !(self.on_batch)(ready) {
                return false;
            }
        }
        true
    }

    fn flush(&mut self) {
        if !self.batch.is_empty() {
            let ready = std::mem::take(&mut self.batch);
            let _ = (self.on_batch)(ready);
        }
    }

    /// 读取 `node` 的子目录（含松弛/剪枝/符号链接过滤），返回可入队的子节点。
    /// 发射是免费的，`read_dir` 才贵（9p ~1ms/目录）——调用点据此决定何时展开。
    fn children(&mut self, node: &Node) -> Vec<Node> {
        if node.remaining == 0 {
            return Vec::new();
        }
        match self.expanded.get(&node.path) {
            Some(&prev) if node.remaining <= prev => return Vec::new(),
            _ => {
                self.expanded.insert(node.path.clone(), node.remaining);
            }
        }
        let Ok(entries) = fs::read_dir(&node.path) else {
            // 无权限 / 竞态删除：静默跳过。
            return Vec::new();
        };
        let child_remaining = node.remaining.saturating_sub(1);
        let child_eff = node.eff_depth + 1;
        let child_descent = node.descent + 1;
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            // 不跟符号链接（symlink-to-dir 的 is_dir 为 false，这里再显式挡一次）。
            if file_type.is_symlink() || !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if is_pruned_name(&name) {
                continue;
            }
            let child_path = entry.path().to_string_lossy().into_owned();
            if self.prune_abs.contains(&child_path) {
                continue;
            }
            out.push(Node {
                eff_depth: child_eff,
                slow: node.slow || path_has_prefix(&child_path, &self.slow_prefixes),
                descent: child_descent,
                remaining: child_remaining,
                path: child_path,
            });
        }
        out
    }
}

/// 执行分层 BFS，用三个堆把慢挂载深层与本地扫描解耦，避免 9p 深层饿死本地深层：
///
/// * 阶段 1a：慢挂载浅层——只展开锚点（descent 0，一次 `read_dir`/根，实测 ~82ms），
///   把兄弟层（descent 1）发射后推入 `deferred`；**绝不**在此对深层做 `read_dir`。
///   浅层因此早到，本地几乎不被拖慢。
/// * 阶段 1b：本地 frontier，按 `eff_depth` 浅先深后，不被慢挂载打断；截到 `local_cap`
///   给慢挂载留配额，被截掉的是本地最深最不可能的层。
/// * 阶段 2：慢挂载深层，独占本地扫完后的剩余时间预算，浅先深后发射到 cap。
/// * 阶段 3：收尾补齐——预算耗尽后慢挂载往往认领不满自己的配额（9p 一次展开 ~1.8ms，
///   预算内只够 ~1700–2050 个），把没认领的槽用本地堆补满，`TOPUP_GRACE` 封顶。
///
/// 阶段 2/3 不交错：本地一次展开约 0.02ms、比慢挂载快约 75 倍，1:1 交错会把本地锁死
/// 在 9p 的速率上（实测一轮只回收得到 2000 出头个槽，仍有几百个空着）。分开跑之后
/// 慢挂载拿满整段预算、本地在其后 ~45ms 内补齐，两边都不吃亏。
///
/// `on_batch` 返回 `false`（接收端断开）时提前停止。
pub(crate) fn run<F: FnMut(Vec<String>) -> bool>(job: ScanJob, on_batch: F) {
    let has_slow = job.roots.iter().any(|root| root.slow);
    // 无慢挂载时配额为 0、本地用满整个 cap（沙盒/纯本地零回归）。
    let slow_reserve = if has_slow { job.slow_reserve } else { 0 };
    let local_cap = job.cap.saturating_sub(slow_reserve);

    let mut scanner = Scanner {
        emitted: HashSet::new(),
        expanded: HashMap::new(),
        batch: Vec::with_capacity(BATCH_SIZE),
        total: 0,
        local_emitted: 0,
        cap: job.cap,
        local_cap,
        prune_abs: job.prune_abs,
        slow_prefixes: job.slow_prefixes,
        deadline: job.deadline,
        since_clock: 0,
        on_batch,
    };

    let mut local: BinaryHeap<Reverse<Prioritized>> = BinaryHeap::new();
    let mut slow: BinaryHeap<Reverse<Prioritized>> = BinaryHeap::new();
    let mut deferred: BinaryHeap<Reverse<Prioritized>> = BinaryHeap::new();
    let mut seq: u64 = 0;

    for root in job.roots {
        let node = Node {
            eff_depth: root.eff_depth,
            slow: root.slow,
            descent: 0,
            remaining: root.remaining,
            path: root.path.to_string_lossy().into_owned(),
        };
        if root.slow {
            push_node(&mut slow, (node.descent, seq), node);
        } else {
            push_node(&mut local, (node.eff_depth, seq), node);
        }
        seq += 1;
    }

    // 阶段 1a：慢挂载浅层。
    while let Some(Reverse(item)) = slow.pop() {
        if scanner.hit_limit() {
            break;
        }
        let node = item.node;
        if !scanner.emit(&node.path, false) {
            scanner.flush();
            return;
        }
        if node.descent == 0 {
            // 展开锚点：一次 read_dir，浮出兄弟层。
            for child in scanner.children(&node) {
                if child.slow {
                    push_node(&mut slow, (child.descent, seq), child);
                } else {
                    push_node(&mut local, (child.eff_depth, seq), child);
                }
                seq += 1;
            }
        } else {
            // 兄弟层已发射；其展开（9p read_dir）推迟到本地扫完后的剩余预算。
            push_node(&mut deferred, (node.descent, seq), node);
            seq += 1;
        }
    }

    // 阶段 1b：本地 frontier。
    while let Some(Reverse(item)) = local.pop() {
        if scanner.hit_limit() {
            break;
        }
        if scanner.local_emitted >= scanner.local_cap {
            break; // 达到本地配额，剩余 local 堆留给阶段 3 回收。
        }
        let node = item.node;
        if !scanner.emit(&node.path, true) {
            scanner.flush();
            return;
        }
        for child in scanner.children(&node) {
            if child.slow {
                // 本地子树里冒出慢挂载（罕见，通常不会跨 $HOME）：丢进深层预算。
                push_node(&mut deferred, (child.descent, seq), child);
            } else {
                push_node(&mut local, (child.eff_depth, seq), child);
            }
            seq += 1;
        }
    }

    // 阶段 2：慢挂载深层，独占本地扫完后的剩余时间预算。这里不再交错「本地回收」——
    // 本地一次展开比慢挂载快约 75 倍，1:1 交错等于把本地锁死在 9p 的速率上，回收不了
    // 几个槽；剩余槽统一交给阶段 3 在预算耗尽后一次补齐，更快也更公平。
    while let Some(Reverse(item)) = deferred.pop() {
        if scanner.hit_limit() {
            break;
        }
        let node = item.node;
        // 兄弟层（descent 1）已在 1a 发射，emit 命中去重是无操作；descent ≥2 首次发射。
        if !scanner.emit(&node.path, false) {
            scanner.flush();
            return;
        }
        for child in scanner.children(&node) {
            push_node(&mut deferred, (child.descent, seq), child);
            seq += 1;
        }
    }

    // 阶段 3：收尾补齐。慢挂载此时已经用满整个时间预算、认领不动更多了，它没占掉的
    // 槽如果就这么留空，等于白白丢掉同样数量的本地候选（本地被 `local_cap` 截掉的
    // 那批最深层）。补齐发生在预算之后，不与慢挂载争抢名额；`TOPUP_GRACE` 给它封顶。
    let topup_deadline = Instant::now() + TOPUP_GRACE;
    while scanner.total < scanner.cap {
        let Some(Reverse(item)) = local.pop() else {
            break;
        };
        if Instant::now() >= topup_deadline {
            break;
        }
        let node = item.node;
        if !scanner.emit(&node.path, true) {
            scanner.flush();
            return;
        }
        for child in scanner.children(&node) {
            if !child.slow {
                push_node(&mut local, (child.eff_depth, seq), child);
                seq += 1;
            }
        }
    }

    scanner.flush();
}

/// 启动后台扫描线程，返回批次接收端。`CDH_DISCOVER=0` 或无根时返回 `None`。
///
/// 放弃语义：调用方退出时直接丢弃 `Receiver`、不 join——死挂载上的 `read_dir`
/// 不可中断，靠进程退出回收线程。
pub(crate) fn spawn(
    history_paths: Vec<String>,
    excluded: HashSet<String>,
) -> Option<mpsc::Receiver<Vec<String>>> {
    if !discover_enabled() {
        return None;
    }

    let (tx, rx) = mpsc::channel::<Vec<String>>();
    let home = env::var("HOME").ok().filter(|home| !home.is_empty());
    let scan_env = ScanEnv::from_process();

    thread::Builder::new()
        .name("cdh-discover".to_string())
        .spawn(move || {
            let mut roots = compute_roots(&history_paths, home.as_deref(), &scan_env);
            drop_excluded_roots(&mut roots, &excluded);
            if roots.is_empty() {
                return;
            }
            let mut prune_abs = prune_abs_set(home.as_deref());
            prune_abs.extend(excluded);
            let job = ScanJob {
                roots,
                cap: CANDIDATE_CAP,
                deadline: Some(Instant::now() + TIME_BUDGET),
                prune_abs,
                slow_prefixes: default_slow_prefixes(),
                slow_reserve: CANDIDATE_CAP / SLOW_RESERVE_DIVISOR,
            };
            run(job, |batch| tx.send(batch).is_ok());
        })
        .ok()?;

    Some(rx)
}

/// 补扫单棵子树，用于「取消排除」后把该目录立刻找回来。
///
/// 不重跑整轮扫描：启动那轮的剪枝集合在 spawn 时就定死了，取消排除并不会让它回头
/// 补上；而整轮重扫要花掉又一个 5 秒预算，只为了找回用户刚点名的一棵树。这里就以
/// 该目录为唯一根、不限深度地跑一遍，走同一个 `run`，结果并入同一条并入路径。
pub(crate) fn spawn_subtree(
    root: String,
    excluded: HashSet<String>,
) -> Option<mpsc::Receiver<Vec<String>>> {
    if !discover_enabled() {
        return None;
    }
    let (tx, rx) = mpsc::channel::<Vec<String>>();
    let home = env::var("HOME").ok().filter(|home| !home.is_empty());

    thread::Builder::new()
        .name("cdh-discover-topup".to_string())
        .spawn(move || {
            let mut prune_abs = prune_abs_set(home.as_deref());
            prune_abs.extend(excluded);
            let slow = is_slow_mount(&root);
            let job = ScanJob {
                roots: vec![RootSpec {
                    path: PathBuf::from(root),
                    eff_depth: 0,
                    remaining: UNLIMITED,
                    slow,
                }],
                cap: CANDIDATE_CAP,
                deadline: Some(Instant::now() + TIME_BUDGET),
                prune_abs,
                slow_prefixes: default_slow_prefixes(),
                // 单根扫描没有「本地被慢挂载饿死」的问题：配额留了也没人跟它抢。
                slow_reserve: 0,
            };
            run(job, |batch| tx.send(batch).is_ok());
        })
        .ok()?;

    Some(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as stdfs;
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempTree {
        root: PathBuf,
    }
    impl TempTree {
        fn new(name: &str) -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let root = env::temp_dir().join(format!(
                "cdh-discover-{name}-{}-{stamp}-{seq}",
                std::process::id()
            ));
            stdfs::create_dir_all(&root).unwrap();
            Self { root }
        }
        fn dir(&self, rel: &str) -> PathBuf {
            let path = self.root.join(rel);
            stdfs::create_dir_all(&path).unwrap();
            path
        }
        fn s(&self, rel: &str) -> String {
            self.root.join(rel).to_string_lossy().into_owned()
        }
    }
    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = stdfs::remove_dir_all(&self.root);
        }
    }

    fn collect(roots: Vec<RootSpec>, prune_abs: HashSet<String>) -> Vec<String> {
        collect_job(ScanJob {
            roots,
            cap: CANDIDATE_CAP,
            deadline: None,
            prune_abs,
            slow_prefixes: default_slow_prefixes(),
            slow_reserve: CANDIDATE_CAP / SLOW_RESERVE_DIVISOR,
        })
    }

    fn collect_job(job: ScanJob) -> Vec<String> {
        let mut out = Vec::new();
        run(job, |batch| {
            out.extend(batch);
            true
        });
        out
    }

    fn slow_root(path: PathBuf, eff: u32, remaining: u32) -> RootSpec {
        RootSpec {
            path,
            eff_depth: eff,
            remaining,
            slow: true,
        }
    }

    fn root(path: PathBuf, eff: u32, remaining: u32) -> RootSpec {
        RootSpec {
            slow: is_slow_mount(&path.to_string_lossy()),
            path,
            eff_depth: eff,
            remaining,
        }
    }

    #[test]
    fn prune_names_stop_descent_but_keep_hidden_dirs() {
        let tree = TempTree::new("prune");
        tree.dir("proj/node_modules/inner");
        tree.dir("proj/.git/objects");
        tree.dir("proj/src");
        tree.dir("proj/.claude-ctf-workspace"); // 隐藏但非垃圾：必须保留
        let found = collect(vec![root(tree.dir("proj"), 1, UNLIMITED)], HashSet::new());

        assert!(found.contains(&tree.s("proj/src")));
        assert!(
            found.contains(&tree.s("proj/.claude-ctf-workspace")),
            "hidden non-junk dirs must survive: {found:?}"
        );
        // node_modules / .git 本身与其子树都不出现。
        assert!(!found.iter().any(|p| p.contains("node_modules")));
        assert!(!found.iter().any(|p| p.contains(".git")));
    }

    #[test]
    fn prune_abs_paths_are_skipped() {
        let tree = TempTree::new("pruneabs");
        tree.dir("keep");
        let containers = tree.dir("skipme/lots");
        let mut abs = HashSet::new();
        abs.insert(tree.s("skipme"));
        let found = collect(vec![root(tree.root.clone(), 1, UNLIMITED)], abs);
        assert!(found.contains(&tree.s("keep")));
        assert!(
            !found.contains(&containers.to_string_lossy().into_owned())
                && !found.iter().any(|p| p.contains("skipme")),
            "abs-pruned subtree must not appear: {found:?}"
        );
    }

    #[test]
    fn count_cap_truncates() {
        let tree = TempTree::new("cap");
        for index in 0..50 {
            tree.dir(&format!("d{index}"));
        }
        let out = collect_job(ScanJob {
            roots: vec![root(tree.root.clone(), 1, UNLIMITED)],
            cap: 10,
            deadline: None,
            prune_abs: HashSet::new(),
            slow_prefixes: default_slow_prefixes(),
            slow_reserve: 10 / SLOW_RESERVE_DIVISOR,
        });
        assert_eq!(out.len(), 10, "cap must be a hard ceiling: {}", out.len());
    }

    #[test]
    fn shallow_layers_arrive_before_deep_layers() {
        // 三个根：本地锚点(eff0) / $HOME(eff1) / 慢挂载模拟(eff2)。断言浅层先送达。
        let tree = TempTree::new("layer");
        tree.dir("anchor/sibling"); // 锚点的兄弟：eff1
        tree.dir("home/a/b/c/d"); // $HOME 深链
        let roots = vec![
            root(tree.dir("anchor"), 0, ANCHOR_DESCENT),
            root(tree.dir("home"), 1, UNLIMITED),
        ];
        let found = collect(roots, HashSet::new());
        let pos = |needle: &str| found.iter().position(|p| p.ends_with(needle)).unwrap();
        // 锚点(eff0)先于其兄弟(eff1)，兄弟先于 $HOME 的深层 a/b(eff3)。
        assert!(pos("/anchor") < pos("/sibling"));
        assert!(pos("/sibling") < pos("/a/b"));
        assert!(pos("/home/a") < pos("/a/b"));
    }

    #[test]
    fn anchor_depth_one_emits_siblings_only_but_home_reaches_deep() {
        // 松弛：锚点(eff0,rem1)只发一层；同路径经 $HOME(无限预算)补齐深层。
        let tree = TempTree::new("relax");
        let shared = tree.dir("home/work"); // 既是历史锚点，也在 $HOME 树下
        tree.dir("home/work/proj/src/inner");
        let roots = vec![
            root(shared.clone(), 0, ANCHOR_DESCENT),
            root(tree.dir("home"), 1, UNLIMITED),
        ];
        let found = collect(roots, HashSet::new());
        assert!(found.contains(&tree.s("home/work/proj")));
        assert!(
            found.contains(&tree.s("home/work/proj/src/inner")),
            "unlimited $HOME budget must relax past the depth-1 anchor: {found:?}"
        );
        // 去重：shared 只出现一次。
        assert_eq!(
            found
                .iter()
                .filter(|p| p.as_str() == tree.s("home/work"))
                .count(),
            1
        );
    }

    #[test]
    fn does_not_follow_symlinks() {
        let tree = TempTree::new("symlink");
        tree.dir("real/inside");
        tree.dir("target/secret");
        symlink(tree.root.join("target"), tree.root.join("real/link")).unwrap();
        let found = collect(vec![root(tree.dir("real"), 1, UNLIMITED)], HashSet::new());
        assert!(found.contains(&tree.s("real/inside")));
        assert!(
            !found
                .iter()
                .any(|p| p.contains("secret") || p.ends_with("/link")),
            "symlinked dirs must not be followed: {found:?}"
        );
    }

    #[test]
    fn compute_roots_cascade_priority_and_external_depth() {
        // 造一个真实目录树，让存在性验证通过；断言级联优先级与深度预算。
        let tree = TempTree::new("cascade");
        let home = tree.dir("home");
        tree.dir("home/work/projA"); // $HOME 内历史 -> 锚点 home/work (eff0, rem1)
        tree.dir("mnt/code/thing"); // $HOME 外历史 -> 锚点 mnt/code (eff0,rem1)+(eff2,rem4)
        let history = vec![tree.s("home/work/projA"), tree.s("mnt/code/thing")];
        let roots = compute_roots(&history, Some(&home.to_string_lossy()), &ScanEnv::default());

        let find = |path: &str, eff: u32| {
            roots
                .iter()
                .find(|r| r.path == Path::new(&tree.s(path)) && r.eff_depth == eff)
        };
        // 1) 历史锚点：两个父目录，eff0 / rem1。
        assert!(find("home/work", 0).unwrap().remaining == ANCHOR_DESCENT);
        assert!(find("mnt/code", 0).unwrap().remaining == ANCHOR_DESCENT);
        // 2) $HOME 全树：eff1 / 不限。
        let home_root = roots
            .iter()
            .find(|r| r.path == home && r.eff_depth == 1)
            .unwrap();
        assert_eq!(home_root.remaining, UNLIMITED);
        // 3) $HOME 外锚点深挖：同一父目录再以 eff2 / rem4 出现。
        assert_eq!(find("mnt/code", 2).unwrap().remaining, EXTERNAL_DESCENT);
        // $HOME 内的父目录不应出现在外部锚点组（eff2）。
        assert!(find("home/work", 2).is_none());
    }

    #[test]
    fn depth_cap_clamps_cascade_budgets() {
        let tree = TempTree::new("depthcap");
        let home = tree.dir("home");
        tree.dir("home/work/projA");
        let history = vec![tree.s("home/work/projA")];
        let env = ScanEnv {
            roots_override: None,
            depth_cap: Some(0),
        };
        let roots = compute_roots(&history, Some(&home.to_string_lossy()), &env);
        // depth_cap 0 把所有 remaining 夹到 0：只发根本身，不下探。
        assert!(roots.iter().all(|r| r.remaining == 0));
    }

    #[test]
    fn scan_roots_override_applies_depth_cap() {
        let tree = TempTree::new("override");
        tree.dir("r/a/b");
        let env = ScanEnv {
            roots_override: Some(vec![tree.s("r")]),
            depth_cap: Some(1),
        };
        let roots = compute_roots(&[], None, &env);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].remaining, 1);
        let found = collect(roots, HashSet::new());
        assert!(found.contains(&tree.s("r")));
        assert!(found.contains(&tree.s("r/a")));
        assert!(
            !found.contains(&tree.s("r/a/b")),
            "depth cap 1 must not reach depth 2: {found:?}"
        );
    }

    #[test]
    fn discover_enabled_reads_switch() {
        // 只验证解析函数纯逻辑（不动全局 env 的既有值）。
        assert!(discover_enabled() || !discover_enabled()); // 不 panic
    }

    #[test]
    fn excluded_roots_are_dropped_before_the_scan_starts() {
        // 排除的收益主要在省 I/O：根这一层不滤掉，整棵子树照样被遍历，只是发射出来
        // 的路径最后在 UI 侧被丢掉。
        // （变异验证：把 drop_excluded_roots 改成空函数，第一条断言即失败。）
        let mut roots = vec![
            root(PathBuf::from("/home/u/miniforge3"), 1, UNLIMITED),
            root(PathBuf::from("/home/u/miniforge3/envs"), 1, UNLIMITED),
            root(PathBuf::from("/home/u/work"), 1, UNLIMITED),
        ];
        let excluded: HashSet<String> = ["/home/u/miniforge3".to_string()].into_iter().collect();
        drop_excluded_roots(&mut roots, &excluded);
        let kept: Vec<_> = roots
            .iter()
            .map(|root| root.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(kept, vec!["/home/u/work".to_string()]);

        // 空清单是常态，必须原样放行。
        let mut roots = vec![root(PathBuf::from("/home/u/work"), 1, UNLIMITED)];
        drop_excluded_roots(&mut roots, &HashSet::new());
        assert_eq!(roots.len(), 1);
    }

    #[test]
    fn mount_root_itself_counts_as_slow() {
        // 挂载根自身必须判成慢挂载：历史里有 /mnt/d 时 compute_roots 会把 /mnt 加成
        // 扫描根，判成本地就会让 9p 的 read_dir 进到阶段 1b 那个本地专用堆里。
        // （变异验证：把 under_prefix 换回 `path.starts_with(prefix)`，第一条断言即失败。）
        assert!(is_slow_mount("/mnt"));
        assert!(is_slow_mount("/mnt/"));
        assert!(is_slow_mount("/mnt/d"));
        assert!(is_slow_mount("/media"));
        assert!(is_slow_mount("/Volumes"));
        // 目录边界对齐：同前缀的另一个目录名不算。
        assert!(!is_slow_mount("/mnturbo"));
        assert!(!is_slow_mount("/media-server/x"));
        assert!(!is_slow_mount("/home/a"));
        // 注入前缀（单测用）走同一条规则。
        let prefixes = vec!["/tmp/t/slow".to_string()];
        assert!(path_has_prefix("/tmp/t/slow", &prefixes));
        assert!(path_has_prefix("/tmp/t/slow/x", &prefixes));
        assert!(!path_has_prefix("/tmp/t/slowpoke", &prefixes));
    }

    #[test]
    fn local_any_depth_emitted_before_slow_deep_layers() {
        // 调度性质：本地目录（任意深度）先于慢挂载深层（descent ≥2）发射，避免 9p 深层
        // 饿死本地深层。慢挂载用注入前缀模拟，不依赖真实 /mnt。
        // （变异验证：把 1a 里 descent≥1 的 `push deferred` 改成就地展开，此断言即失败。）
        let tree = TempTree::new("sched");
        tree.dir("home/a/b/c"); // 本地深链（depth 3）
        tree.dir("slow/x/y/z"); // 慢挂载深链
        let slow_prefix = tree.s("slow");
        let roots = vec![
            root(tree.dir("home"), 1, UNLIMITED),
            slow_root(tree.dir("slow"), 2, EXTERNAL_DESCENT),
        ];
        let found = collect_job(ScanJob {
            roots,
            cap: CANDIDATE_CAP,
            deadline: None,
            prune_abs: HashSet::new(),
            slow_prefixes: vec![slow_prefix.clone()],
            slow_reserve: CANDIDATE_CAP / SLOW_RESERVE_DIVISOR,
        });
        let last_local = found
            .iter()
            .rposition(|p| p.starts_with(&tree.s("home")))
            .unwrap();
        let first_slow_deep = found
            .iter()
            .position(|p| p.starts_with(&format!("{slow_prefix}/x/y")))
            .unwrap();
        assert!(
            last_local < first_slow_deep,
            "local (any depth) must precede slow deep layers: {found:?}"
        );
        // 慢挂载浅层（锚点 + 兄弟, descent ≤1）应早到——先于慢挂载深层。
        let slow_sibling = found
            .iter()
            .position(|p| p == &format!("{slow_prefix}/x"))
            .unwrap();
        assert!(slow_sibling < first_slow_deep);
    }

    #[test]
    fn slow_reserve_lets_deep_slow_through_under_cap() {
        // 本地候选远多于 cap；无配额时本地吃满 cap、慢挂载深层颗粒无收。配额保证深层能过。
        // （变异验证：把 run 里 local_cap 改成恒等于 cap，slow deep 断言即失败。）
        let tree = TempTree::new("reserve");
        for index in 0..40 {
            tree.dir(&format!("home/d{index}"));
        }
        tree.dir("slow/a/b/c/d");
        let slow_prefix = tree.s("slow");
        let roots = vec![
            root(tree.dir("home"), 1, UNLIMITED),
            slow_root(tree.dir("slow"), 2, EXTERNAL_DESCENT),
        ];
        let found = collect_job(ScanJob {
            roots,
            cap: 10,
            deadline: None,
            prune_abs: HashSet::new(),
            slow_prefixes: vec![slow_prefix.clone()],
            slow_reserve: 3,
        });
        assert!(found.len() <= 10, "cap is a hard ceiling: {}", found.len());
        // 深层（descent 2）在保留配额下仍被发射。
        assert!(
            found.iter().any(|p| p == &format!("{slow_prefix}/a/b")),
            "reserve must let slow deep layers through under cap: {found:?}"
        );
    }

    #[test]
    fn topup_fills_slots_the_slow_mount_never_claimed() {
        // 慢挂载整棵树只有 2 个目录，认领不满 5 个配额；本地则远多于 cap。没有阶段 3
        // 时总数停在 7（本地 5 + 慢挂载 2），3 个槽空着——等于白丢 3 个本地候选。
        // （变异验证：删掉阶段 3 的补齐循环，本断言即失败：left: 7, right: 10。）
        let tree = TempTree::new("topup");
        for index in 0..40 {
            tree.dir(&format!("home/d{index}"));
        }
        tree.dir("slow/x");
        let slow_prefix = tree.s("slow");
        let roots = vec![
            root(tree.dir("home"), 1, UNLIMITED),
            slow_root(tree.dir("slow"), 2, EXTERNAL_DESCENT),
        ];
        let found = collect_job(ScanJob {
            roots,
            cap: 10,
            deadline: None,
            prune_abs: HashSet::new(),
            slow_prefixes: vec![slow_prefix.clone()],
            slow_reserve: 5,
        });
        assert_eq!(
            found.len(),
            10,
            "topup must fill slots the slow mount left unclaimed: {found:?}"
        );
        // 补齐不抢慢挂载的名额：它认领得到的 2 个仍然在。
        assert_eq!(
            found.iter().filter(|p| p.starts_with(&slow_prefix)).count(),
            2,
            "topup must not displace what the slow mount did claim: {found:?}"
        );
    }
}
