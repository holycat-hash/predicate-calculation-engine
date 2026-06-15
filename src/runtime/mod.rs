//! runtime 层：唯一的调度者与索引持有者（§1.1）。
//!
//! 职责：维护数据双缓冲（单存储 + 写日志，帧界提交）；收集帧 N 写集；帧 N+1
//! 路由给谓词；维护谓词索引与 fold 增量状态；管理实例生命周期、id 分配与
//! ref 反向表（§6.3）；并作为系统内建 writer 向内建 cell 写入。
//!
//! runtime 不承载任何业务逻辑；它对谓词的全部「理解」来自注册期编译（§5）。
//!
//! ## 帧模型（§2）
//! ```text
//! step(帧 N):  阶段一(路由)  持帧 N-1 写集 W：索引查找 → 条件判定 →
//!                            填充 each 触发 / batch 缓冲 / 更新 fold
//!              阶段二(执行)  被触发的 calculation 运行，write 进帧 N 写缓冲
//!              帧边界        提交写缓冲 → 快照；生命周期结算（spawn/destroy/ref 置 null）
//! ```
//!
//! ## 白送优化（A 层，已落地）
//! SoA 列存（`store`）；单存储 + 写日志双缓冲；值桶 / 共享排序阈值表 /
//! crossed 区间查询（`route`）；等价条件合并求值；fold 增量维护（min/max
//! 多重集）；Clock 谓词退化为 ECS 稠密遍历（注册期识别）；帧 scratch 缓冲
//! 跨帧复用；免费 profiler（[`Profile`]，D2 送的遥测）。
//! 执行阶段零序约束 + 无原子：写从不落共享存储，落本地写缓冲，提交在帧界
//! ——`parallel` feature 下执行阶段按触发并行（D1 + 写局部性保证无竞争）。
//!
//! ## 开发者档位（C 层）
//! C1 [`Tier`]、C2 [`CalcOptions::reads`]、C3 [`Residency`]、
//! C4 [`Determinism`]、C5 [`Detect`]、C6 [`RowPolicy`]。

pub mod clock;
mod route;
mod store;

/// render 复用 sim 的谓词求值器 / 投影（条件评估单一真源，render 不再 fork）。
pub(crate) use route::{CompiledCond, project_ro};
pub use store::{RowPolicy, Store};

use std::collections::{HashMap, HashSet};

use crate::calculation::{CalcFn, CalcId, Ctx, Input};
use crate::entity::{CellAddr, EntityTypeId, FIELD_ALIVE, FieldDef, FieldId, InstanceId};
use crate::predicate::{Cond, Delivery, Predicate, Scope};
use crate::value::Value;

/// 一条写记录。`inst` 既是被写实例也是 writer（写局部性：calculation 只能写自己）。
///
/// 写日志即双缓冲：`old` 是上一帧提交值——W 稀疏（设计公理），旧值只需写过的
/// cell，恰好就是日志本身。
#[derive(Debug, Clone)]
pub struct WriteRec {
    pub inst: InstanceId,
    pub field: FieldId,
    /// 上一帧提交值（双缓冲免费可得）。
    pub old: Value,
    pub new: Value,
}

// ---- 开发者档位（C 层）----

/// C1 执行档位。合法性（无竞争、快照一致）是白送的；收益性要求 calc 体落在
/// 可编译受限子集内——runtime 看不穿图灵完备代码，必须开发者标注。
/// 这是 §3.5 准入标准的镜像定理：从谓词词汇换到执行层，同一原则。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tier {
    /// 任意图灵完备代码（默认）。
    #[default]
    General,
    /// 受限 kernel 子集：无动态分配（`spawn` 被禁）、宜无分支发散。
    /// 代价：表达力受限 + 发散风险自负。backend 可据此把执行批入
    /// SIMD/GPU kernel；本实现强制 spawn 禁令并保留标注供调度。
    Kernel,
}

/// C3 GPU/CPU 驻留。谓词图给出静态数据流拓扑，但边权是动态写量，
/// 小 W 帧 GPU 启动开销倒挂——没有静态正解，必须 pin 或 profile + 滞回。
/// 本实现记录 pin 并经 [`Profile`] 提供边权遥测；GPU backend 预留。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Residency {
    /// 交给 runtime 自适应（B 层，依赖遥测）。
    #[default]
    Auto,
    Cpu,
    Gpu,
}

/// C4 确定性档位。D3 把自由序变成合法默认；lockstep 网络 / 回放调试
/// 需要买回确定性，价格是性能。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Determinism {
    /// 自由序（默认）：batch 交付序未定义，backend 任选（含最优序）。
    #[default]
    Free,
    /// 规范序：batch 按 (writer, field) 规范键排序交付；并行执行的
    /// 写折叠按触发序归并。诚实条款：浮点 sum 的位级回放不确定性
    /// 与本档是同一笔账——fold 归约序由路由序固定。
    Canonical,
}

/// C5 检测档位。常开检测污染最热路径——告警等级是编译/构建档位，
/// 不是运行时仲裁。默认跟随构建档：debug 构建 Warn，release 构建 Silent。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detect {
    /// 检测到「同字段被同一 calculation 多次运行写入不同值」（§2）、
    /// 读集越界（C2）、kernel 档违规（C1）时 panic。
    Strict,
    /// 同上,但仅 eprintln 告警。
    Warn,
    /// 静默折叠（§8 开放问题四的 release 答案）。
    Silent,
}

impl Default for Detect {
    fn default() -> Self {
        if cfg!(debug_assertions) {
            Detect::Warn
        } else {
            Detect::Silent
        }
    }
}

/// calculation 注册的可选档位（C 层入口）。
#[derive(Debug, Clone, Default)]
pub struct CalcOptions {
    /// C2 读集声明。写集因 D1 全静态；读集藏在体内——声明换热冷分离与
    /// 预取精度，不声明则退化为 profile 猜测。非 Silent 档下 `read_own`
    /// 越界会被检测。
    pub reads: Option<Vec<FieldId>>,
    /// C1 执行档位。
    pub tier: Tier,
    /// C3 驻留 pin。
    pub residency: Residency,
}

// ---- 免费 profiler（白送优化，D2 买单）----

/// 路由输入就是每 cell 写频，触发计数自然产出——自适应策略（B 层）的
/// 遥测零边际成本。这是阈值物化、分片粒度、行压缩时机等自适应决策的前提。
#[derive(Debug, Default)]
pub struct Profile {
    /// 已推进帧数。
    pub frames: u64,
    /// 上一帧写集大小 |W|。
    pub last_writes: usize,
    /// 上一帧触发集大小 |F|。
    pub last_triggers: usize,
    write_counts: HashMap<(EntityTypeId, FieldId), u64>,
    trigger_counts: Vec<u64>,
}

impl Profile {
    /// 某 cell 列的累计写频。
    pub fn writes(&self, ty: EntityTypeId, field: FieldId) -> u64 {
        self.write_counts.get(&(ty, field)).copied().unwrap_or(0)
    }

    /// 某 calculation 的累计触发数。
    pub fn triggers(&self, calc: CalcId) -> u64 {
        self.trigger_counts
            .get(calc.0 as usize)
            .copied()
            .unwrap_or(0)
    }

    /// 写频降序的热 cell 列表（(type, field), 次数）。
    pub fn hot_cells(&self) -> Vec<((EntityTypeId, FieldId), u64)> {
        let mut v: Vec<_> = self.write_counts.iter().map(|(k, c)| (*k, *c)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }
}

// ---- C1/C2/C3 上游集成：每帧执行计划（Schedule）----

/// `Residency::Auto` 的 GPU 建议阈值（累计触发数）：超过则认为该 calc 足够
/// 数据并行、值得考虑 GPU 驻留。纯启发式 seam——真实后端应加滞回防抖（C3 doc）。
const GPU_HINT_THRESHOLD: u64 = 1024;

/// 一个被触发 calc 的本帧执行计划组（按 calc 分组、组内连续）。
///
/// C 档位的注册期标注在此**解析为本帧可执行计划**——不再是 `opts` 里的惰性元数据：
/// 组内连续是 C1 kernel 批 / C2 读集局部性的结构前提；`residency` 是 C3 解析结果
/// （`Auto` 经 [`Profile`] 遥测给出建议）。后端可照此降级（SIMD/GPU/CPU 分区）
/// 而不 fork 核心循环（seam 真）。
#[derive(Debug, Clone)]
pub struct ScheduleGroup {
    pub calc: CalcId,
    /// 本帧该 calc 的触发数（连续一段）。
    pub count: usize,
    /// C1 执行档位。
    pub tier: Tier,
    /// C3 驻留（`Auto` 已解析为 CPU/GPU 建议）。
    pub residency: Residency,
    /// C2 声明读集（热列；None = 未声明）。
    pub reads: Option<Vec<FieldId>>,
}

/// 本帧的执行计划：按 calc 分组的有序触发计划（[`Runtime::last_schedule`]）。
#[derive(Debug, Clone, Default)]
pub struct Schedule {
    pub groups: Vec<ScheduleGroup>,
}

impl Schedule {
    /// 解析后驻留 CPU / GPU 的 calc 分区（C3 上游集成的可观测产物）。
    pub fn residency_partition(&self) -> (Vec<CalcId>, Vec<CalcId>) {
        let mut cpu = vec![];
        let mut gpu = vec![];
        for g in &self.groups {
            match g.residency {
                Residency::Gpu => gpu.push(g.calc),
                _ => cpu.push(g.calc),
            }
        }
        (cpu, gpu)
    }
}

pub(crate) struct RegisteredCalc {
    pub name: String,
    /// 挂在哪个 entity 类型下。
    pub ty: EntityTypeId,
    pub pred: Predicate,
    pub n_groups: u32,
    /// 条件不引用订阅者（own/self）→ 类型扇出时每写求值一次（§5 等价合并）。
    pub cond_indep: bool,
    /// 条件结构等价类 id（等价合并的 memo 键）。
    pub cond_class: u32,
    /// 谓词预编译产物（白送优化）：扁平后缀程序，运行期紧循环求值。
    pub compiled: route::CompiledCond,
    pub declared_writes: HashSet<FieldId>,
    pub opts: CalcOptions,
    pub f: CalcFn,
}

/// scope 注册期编译产物：合取组的析取原子（(a|b) & c → [[a,b],[c]]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Atom {
    Own(FieldId),
    Inst { ref_field: FieldId, field: FieldId },
    Type(EntityTypeId, FieldId),
}

#[derive(Default)]
pub(crate) struct Indexes {
    /// (writer 类型, 字段) → own 订阅（订阅者 = writer 自己）。O(1) 哈希链（§4）。
    pub own: HashMap<(EntityTypeId, FieldId), Vec<(CalcId, u32)>>,
    /// (被盯类型, 字段) → type 订阅索引（值桶 / 阈值表 / 扫描退化，见 route）。
    pub type_: HashMap<(EntityTypeId, FieldId), route::TypeIndex>,
    /// (订阅者类型, ref 字段, 被盯字段) → inst 订阅。
    pub inst: HashMap<(EntityTypeId, FieldId, FieldId), Vec<(CalcId, u32)>>,
    /// ref 反向表（§6.3）：target 实例 → {(持有者, ref 字段)}。
    /// 同时服务 inst 路由与销毁结算（ref 置 null）。
    pub ref_reverse: HashMap<InstanceId, HashSet<(InstanceId, FieldId)>>,
}

pub(crate) struct Trigger {
    pub calc: CalcId,
    pub subscriber: InstanceId,
    pub input: Input,
}

/// 全存快照（GGPO 式 rollback netcode 的接口，C 层「有代价」优化）。
///
/// 捕获全部**动态**仿真状态：`Store`（类型化无装箱列 → 整存克隆退化为连续
/// memcpy，这正是「廉价」的物理来源）、fold 增量、ref 反向表、clock 闹钟、待路由
/// 写集、spawn 队列、帧号。注册期**静态**物——谓词索引、calc 闭包、C 档位配置、
/// 免费 profiler 遥测——不入快照（rollback 不改注册，遥测是单调诊断量）。
///
/// 用法（格斗 / lockstep）：每帧 `snapshot()` 存入环形缓冲；预测输入错误时
/// `restore()` 回到正确帧，喂修正输入重 `step()`。配合 [`Determinism::Canonical`]
/// （C4）令重放位级确定。`Clone`：环形缓冲可保留多帧。
#[derive(Clone)]
pub struct Snapshot {
    store: Store,
    fold_state: HashMap<(CalcId, InstanceId), route::FoldAcc>,
    fold_dirty: Vec<(CalcId, InstanceId)>,
    ref_reverse: HashMap<InstanceId, HashSet<(InstanceId, FieldId)>>,
    clock: clock::Clock,
    schedule: Schedule,
    frame: u64,
    pending: Vec<WriteRec>,
    spawn_queue: Vec<(EntityTypeId, Vec<(FieldId, Value)>)>,
    last_routed: Vec<WriteRec>,
}

impl Snapshot {
    /// 此快照对应的帧号（环形缓冲按帧检索）。
    pub fn frame(&self) -> u64 {
        self.frame
    }
}

pub struct Runtime {
    store: Store,
    calcs: Vec<RegisteredCalc>,
    /// D1 单写者制：(类型, 字段) → 归属 calculation。注册期冲突即错。
    field_owner: HashMap<(EntityTypeId, FieldId), CalcId>,
    idx: Indexes,
    /// Clock 谓词退化为 ECS（白送优化）：`type(Clock, frame)` + 恒真条件的
    /// calc ≅ 经典 ECS system。注册期识别，跳过路由，稠密列遍历直触发。
    ecs_calcs: Vec<CalcId>,
    /// 条件结构 → 等价类 id（§5 等价合并）。
    cond_classes: HashMap<String, u32>,
    /// fold 增量状态（§3.4）：runtime 维护，per (谓词, 订阅者实例)。
    fold_state: HashMap<(CalcId, InstanceId), route::FoldAcc>,
    /// 死亡撤销后须重投递新聚合值的 fold（(谓词, 订阅者)），下一帧路由时并入交付。
    fold_dirty: Vec<(CalcId, InstanceId)>,
    /// 帧 N-1 提交的写集，本帧路由。唯一触发源（§0）。
    pending: Vec<WriteRec>,
    /// calculation 请求的创建，帧边界生效。
    spawn_queue: Vec<(EntityTypeId, Vec<(FieldId, Value)>)>,
    /// 路由期临时结构，跨帧复用容量（帧 arena 的工程形）。
    scratch: route::Scratch,
    profile: Profile,
    /// C1/C2/C3 上游集成：上一帧解析出的执行计划（按 calc 分组 + 驻留分区）。
    schedule: Schedule,
    detect: Detect,
    determinism: Determinism,
    clock: clock::Clock,
    frame: u64,
    /// render 摄入开关：开启后每帧留存路由写集供 render 消费（默认关，零额外成本）。
    render_feed: bool,
    /// 本帧路由的写集（= §0 唯一触发源），render 经此摄入，与 sim 谓词所见同一流。
    last_routed: Vec<WriteRec>,
}

impl Runtime {
    pub fn new() -> Self {
        let mut rt = Runtime {
            store: Store::new(),
            calcs: vec![],
            field_owner: HashMap::new(),
            idx: Indexes::default(),
            ecs_calcs: vec![],
            cond_classes: HashMap::new(),
            fold_state: HashMap::new(),
            fold_dirty: vec![],
            pending: vec![],
            spawn_queue: vec![],
            scratch: route::Scratch::default(),
            profile: Profile::default(),
            schedule: Schedule::default(),
            detect: Detect::default(),
            determinism: Determinism::default(),
            clock: clock::Clock::placeholder(),
            frame: 0,
            render_feed: false,
            last_routed: vec![],
        };
        let ty = rt.register_entity_type(
            "Clock",
            vec![
                FieldDef::new("frame", Value::Int(0)),
                FieldDef::new("alarm", Value::Null),
            ],
            true,
        );
        rt.clock = clock::Clock::new(
            ty,
            rt.store.alive_instances(ty)[0],
            rt.field(ty, "frame"),
            rt.field(ty, "alarm"),
        );
        rt
    }

    // ---- 档位（C 层）----

    /// C5 检测档位（默认跟随构建档：debug→Warn / release→Silent）。
    pub fn set_detect(&mut self, d: Detect) {
        self.detect = d;
    }

    /// C4 确定性档位（默认 Free——D3 把自由序变成合法默认）。
    pub fn set_determinism(&mut self, d: Determinism) {
        self.determinism = d;
    }

    /// 免费 profiler（D2 送的遥测）：每 cell 写频、每 calc 触发数、|W|/|F|。
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// 上一帧的执行计划（C1/C2/C3 上游集成的可观测产物）：按 calc 分组的有序
    /// 触发计划 + 解析后的驻留分区。后端按此降级（SIMD/GPU/CPU），无需 fork 核心。
    pub fn last_schedule(&self) -> &Schedule {
        &self.schedule
    }

    /// 开启 render 摄入：此后每次 [`Runtime::step`] 留存本帧路由写集，供第二个
    /// （动态帧率）runtime 经 [`Runtime::committed_writes`] 消费（见 [`crate::render`]）。
    /// 默认关闭——不开则零额外成本。
    pub fn enable_render_feed(&mut self) {
        self.render_feed = true;
    }

    /// 本帧路由的写集（= §0 唯一触发源）。render 的摄入源：与 sim 谓词所见同一流，
    /// 故 render 是写流的又一消费者，含外部 spawn 的出生写。须先 [`Runtime::enable_render_feed`]。
    pub fn committed_writes(&self) -> &[WriteRec] {
        &self.last_routed
    }

    // ---- 全存快照 / 回滚（C 层「有代价」优化：rollback netcode，见 [`Snapshot`]）----

    /// 拍一张全存快照（捕获全部动态仿真状态）。类型化无装箱列使整存克隆退化为
    /// 连续 memcpy——这是 GGPO「整 store 廉价快照」的物理前提。注册期静态物不入快照。
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            store: self.store.clone(),
            fold_state: self.fold_state.clone(),
            fold_dirty: self.fold_dirty.clone(),
            ref_reverse: self.idx.ref_reverse.clone(),
            clock: self.clock.clone(),
            schedule: self.schedule.clone(),
            frame: self.frame,
            pending: self.pending.clone(),
            spawn_queue: self.spawn_queue.clone(),
            last_routed: self.last_routed.clone(),
        }
    }

    /// 回滚到某快照：恢复全部动态状态，随后 [`Runtime::step`] 可用修正输入重放。
    /// 注册（索引 / calc / 档位）与遥测不受影响。id 分配状态（代际 / free 列表）
    /// 一并恢复——重放的 spawn 复用同 id、同代际，确定性可重现。
    pub fn restore(&mut self, snap: &Snapshot) {
        self.store = snap.store.clone();
        self.fold_state = snap.fold_state.clone();
        self.fold_dirty = snap.fold_dirty.clone();
        self.idx.ref_reverse = snap.ref_reverse.clone();
        self.clock = snap.clock.clone();
        self.schedule = snap.schedule.clone();
        self.frame = snap.frame;
        self.pending = snap.pending.clone();
        self.spawn_queue = snap.spawn_queue.clone();
        self.last_routed = snap.last_routed.clone();
    }

    // ---- 注册期 ----

    /// 注册 entity 类型（行策略默认 Stable）。`_alive` 自动注入为 0 号字段。
    pub fn register_entity_type(
        &mut self,
        name: &str,
        fields: Vec<FieldDef>,
        singleton: bool,
    ) -> EntityTypeId {
        self.register_entity_type_with(name, fields, singleton, RowPolicy::default())
    }

    /// 注册 entity 类型并指定行身份策略（C6）。
    /// singleton 类型立即创建实例 0（注册生效于帧边界的严格化留作 TODO，§5）。
    pub fn register_entity_type_with(
        &mut self,
        name: &str,
        mut fields: Vec<FieldDef>,
        singleton: bool,
        policy: RowPolicy,
    ) -> EntityTypeId {
        fields.insert(0, FieldDef::new("_alive", Value::Bool(false)));
        let ty = self.store.add_type(name, fields, singleton, policy);
        if singleton {
            self.spawn(ty, vec![]);
        }
        ty
    }

    /// 按名查字段 id（不存在则 panic；可失败版本见 [`Runtime::try_field`]）。
    pub fn field(&self, ty: EntityTypeId, name: &str) -> FieldId {
        self.try_field(ty, name).unwrap_or_else(|e| panic!("{e}"))
    }

    pub fn try_field(&self, ty: EntityTypeId, name: &str) -> Result<FieldId, String> {
        self.store.try_field(ty, name)
    }

    /// 已注册 entity 类型数（含内建 Clock）。
    pub fn type_count(&self) -> usize {
        self.store.types.len()
    }

    /// 某类型全体字段的默认值（含 0 号 `_alive`）。render 侧据此为出生实例的 tracked
    /// 字段做 birth-snap（render 只见写日志增量，未写出的字段须从 schema 默认值取值）。
    pub fn field_defaults(&self, ty: EntityTypeId) -> Vec<Value> {
        self.store
            .fields(ty)
            .iter()
            .map(|f| f.default.clone())
            .collect()
    }

    /// 检视用：某类型当前全体存活实例。
    pub fn alive(&self, ty: EntityTypeId) -> Vec<InstanceId> {
        self.store.alive_instances(ty)
    }

    pub fn clock(&self) -> &clock::Clock {
        &self.clock
    }

    /// 定时语义（§6.2）：到点 runtime 写 `Clock.alarm = payload`，订阅者 each 触发。
    pub fn set_alarm(&mut self, at_frame: u64, payload: Value) {
        self.clock.set_alarm(at_frame, payload);
    }

    /// 注册 calculation 及其唯一前置 predicate（单谓词制，§1.4），默认档位。
    pub fn register_calculation(
        &mut self,
        name: &str,
        ty: EntityTypeId,
        pred: Predicate,
        declared_writes: &[FieldId],
        f: CalcFn,
    ) -> Result<CalcId, String> {
        self.register_calculation_opt(name, ty, pred, declared_writes, CalcOptions::default(), f)
    }

    /// 注册 calculation,带开发者档位（C1/C2/C3，见 [`CalcOptions`]）。
    ///
    /// `declared_writes`：该 calc 的静态写集（D1 检查对象）。`_alive` 豁免
    /// （自决通道）。
    ///
    /// 注册期编译流水线（§5）：scope 展平/索引挂接、D1 检查、inst-ref 校验、
    /// 条件前滤绑定（值桶/阈值表/self-eq 快路）、等价条件归类、ECS 快路识别。
    pub fn register_calculation_opt(
        &mut self,
        name: &str,
        ty: EntityTypeId,
        pred: Predicate,
        declared_writes: &[FieldId],
        opts: CalcOptions,
        f: CalcFn,
    ) -> Result<CalcId, String> {
        let id = CalcId(self.calcs.len() as u32);
        // scope 展平为合取组，并对同一 OR 组内重复原子去重，避免同一写重复交付。
        let mut groups = flatten_scope(&pred.scope)?;
        for group in &mut groups {
            let mut seen = HashSet::new();
            group.retain(|atom| seen.insert(*atom));
        }
        if groups.len() > 1 && !matches!(pred.delivery, Delivery::Each(_)) {
            return Err("合取 scope 暂仅支持 each 交付（batch/fold 下的合取语义未定，§8）".into());
        }

        // D1 单写者冲突检查。先校验、后挂接，保证 Err 路径不污染 runtime。
        let mut unique_writes = vec![];
        let mut seen_writes = HashSet::new();
        for &w in declared_writes {
            if !seen_writes.insert(w) {
                continue;
            }
            check_field(&self.store, ty, w)?;
            if ty == self.clock.ty {
                return Err("Clock 的 cell 由 runtime 内建 writer 独占".into());
            }
            if w == FIELD_ALIVE {
                continue;
            }
            if let Some(prev) = self.field_owner.get(&(ty, w)) {
                let prev_name = self
                    .calcs
                    .get(prev.0 as usize)
                    .map(|c| c.name.as_str())
                    .unwrap_or("<未注册 calculation>");
                return Err(format!(
                    "D1 冲突：{}.{} 已归属 {}",
                    self.store.type_name(ty),
                    self.store.field_name(ty, w),
                    prev_name
                ));
            }
            unique_writes.push(w);
        }

        // ECS 快路识别（白送优化）：type(Clock, frame) + 恒真条件 + each
        // ≅ 经典 ECS system——跳过路由，step 时稠密列遍历直触发。
        let is_ecs = groups.len() == 1
            && groups[0].len() == 1
            && groups[0][0] == Atom::Type(self.clock.ty, self.clock.f_frame)
            && matches!(pred.cond, Cond::True)
            && matches!(pred.delivery, Delivery::Each(_));

        if !is_ecs {
            for group in &groups {
                for atom in group {
                    match *atom {
                        Atom::Own(field) => {
                            check_field(&self.store, ty, field)?;
                        }
                        Atom::Inst { ref_field, .. } => {
                            check_field(&self.store, ty, ref_field)?;
                            if !self.store.is_ref_field(ty, ref_field) {
                                return Err(format!(
                                    "inst scope 的 ref 必须来自自己实例的 ref 类型字段（§3.2），字段 {} 不是 ref",
                                    self.store.field_name(ty, ref_field)
                                ));
                            }
                        }
                        Atom::Type(watched_ty, field) => {
                            check_field(&self.store, watched_ty, field)?;
                        }
                    }
                }
            }
        }

        for &w in &unique_writes {
            self.field_owner.insert((ty, w), id);
        }
        if is_ecs {
            self.ecs_calcs.push(id);
        } else {
            // 所有校验已完成；以下挂接不再返回 Err。
            for (gi, group) in groups.iter().enumerate() {
                for atom in group {
                    match *atom {
                        Atom::Own(field) => {
                            self.idx
                                .own
                                .entry((ty, field))
                                .or_default()
                                .push((id, gi as u32));
                        }
                        Atom::Inst { ref_field, field } => {
                            self.idx
                                .inst
                                .entry((ty, ref_field, field))
                                .or_default()
                                .push((id, gi as u32));
                        }
                        Atom::Type(watched_ty, field) => {
                            self.idx
                                .type_
                                .entry((watched_ty, field))
                                .or_default()
                                .insert(&pred.cond, id, gi as u32);
                        }
                    }
                }
            }
        }
        // 等价条件归类（§5 等价合并）：结构等价的 sub-independent 条件
        // 共享每写一次的求值
        let cond_indep = route::cond_sub_independent(&pred.cond);
        let key = format!("{:?}", pred.cond);
        let next = self.cond_classes.len() as u32;
        let cond_class = *self.cond_classes.entry(key).or_insert(next);
        let compiled = route::CompiledCond::compile(&pred.cond);
        self.profile.trigger_counts.push(0);
        self.calcs.push(RegisteredCalc {
            name: name.to_string(),
            ty,
            n_groups: groups.len() as u32,
            cond_indep,
            cond_class,
            compiled,
            declared_writes: declared_writes.iter().copied().collect(),
            opts,
            pred,
            f,
        });
        Ok(id)
    }

    // ---- 生命周期（§6.3）----

    /// 外部创建实例。runtime 代写 `_alive = true` 与初始字段——都是普通 write，
    /// 下一帧观察者用 `type(E, _alive) where became(true)` 感知出生。
    pub fn spawn(&mut self, ty: EntityTypeId, init: Vec<(FieldId, Value)>) -> InstanceId {
        validate_spawn_init(&self.store, ty, &init).unwrap_or_else(|e| panic!("{e}"));
        let mut recs = std::mem::take(&mut self.pending);
        let inst = spawn_now(
            &mut self.store,
            &mut self.idx.ref_reverse,
            ty,
            init,
            &mut recs,
        );
        self.pending = recs;
        inst
    }

    /// 外部销毁 API：语义等价于代为写入 `_alive = false`（§6.3），帧边界结算。
    pub fn destroy(&mut self, inst: InstanceId) {
        if self.store.alive(inst) {
            let old = self.store.read(inst, FIELD_ALIVE);
            self.store.set(inst, FIELD_ALIVE, Value::Bool(false));
            self.pending.push(WriteRec {
                inst,
                field: FIELD_ALIVE,
                old,
                new: Value::Bool(false),
            });
            self.revoke_dead_from_folds(inst);
            let mut recs = std::mem::take(&mut self.pending);
            settle_death(&mut self.store, &mut self.idx.ref_reverse, inst, &mut recs);
            self.pending = recs;
        }
    }

    /// 成员死亡撤销（§6.3）：死者从所有 fold 贡献集中按实例精确移除，
    /// 受影响的 fold 标脏——下一帧由 [`Runtime::step`] 重投递收缩后的新聚合值。
    /// 死亡无写（不是对被 fold 字段的 write），故撤销必须在此显式补做。
    fn revoke_dead_from_folds(&mut self, dead: InstanceId) {
        let keys: Vec<_> = self.fold_state.keys().copied().collect();
        for (c, sub) in keys {
            if let Some(st) = self.fold_state.get_mut(&(c, sub)) {
                if route::fold_revoke_member(st, dead) {
                    self.fold_dirty.push((c, sub));
                }
            }
        }
    }

    // ---- 帧循环 ----

    /// 推进一帧：路由上一帧写集 → 执行触发的 calculation → 帧边界提交与结算。
    pub fn step(&mut self) {
        self.frame += 1;
        let mut w = std::mem::take(&mut self.pending);
        // runtime 内建 writer：Clock.frame 每帧写入（订阅它=显式轮询，§6.2）；alarm 到点写
        let mut clock_writes = vec![];
        self.clock.tick(self.frame, &self.store, &mut clock_writes);
        w.extend(clock_writes.iter().cloned());
        // 免费 profiler：路由输入就是每 cell 写频（D2）
        self.profile.frames = self.frame;
        self.profile.last_writes = w.len();
        for rec in &w {
            *self
                .profile
                .write_counts
                .entry((rec.inst.ty, rec.field))
                .or_default() += 1;
        }
        // render 摄入：把本帧路由的写集（= 唯一触发源）留存给第二个 runtime 消费。
        // 仅在开启时付这一次 O(|W|) 克隆（W 稀疏，设计公理）。
        if self.render_feed {
            self.last_routed = w.clone();
        }
        // 成员死亡撤销（上一帧结算）标脏的 fold：本帧并入交付，重投递新聚合值。
        let dirty_folds = std::mem::take(&mut self.fold_dirty);
        // 阶段一：路由（索引查找 → 条件判定 → 交付物化）
        let mut triggers = route::route(
            &self.store,
            &self.idx,
            &self.calcs,
            &mut self.fold_state,
            &mut self.scratch,
            &w,
            self.determinism,
            &dirty_folds,
        );
        // ECS 快路：跳过路由，稠密列遍历直触发
        self.push_ecs_triggers(&mut triggers);
        self.profile.last_triggers = triggers.len();
        for t in &triggers {
            self.profile.trigger_counts[t.calc.0 as usize] += 1;
        }
        // C1/C2/C3 上游集成：按 calc 分组（kernel 批 / 读集局部性的结构前提），
        // 物化本帧执行计划并解析驻留（C3，含 Auto 的 profile 建议）。
        //
        // 重排的合法性是 *store-scoped* 的，并依赖 calc 副作用受限（effect-confinement）：
        //   D1（写集互斥）+ 写局部 + 快照读 + calc 无 ctx 外副作用
        //   ⇒ 提交 store 的持久 cell 字段值与触发序无关；
        //      新生实体身份仅在固定调度（Canonical）下与序无关，Free 下至多差一个 id 置换
        //      （spawn 按触发序分配 id，重排即重排 id 赋予）。
        // 跨 calc 写不同 cell（D1）⇒ 折叠互不影响；同 calc 同 cell 由稳定排序保相对序。
        // 不变量不覆盖闭包的 ambient 副作用（log/RNG/static）——那在 store 之外，由
        // effect-confinement 假设排除（execution 层为不透明 Box<dyn Fn>，本实现只能假设、
        // 不能机检；补 kernel-IR seam 后可升级为保证）。
        triggers.sort_by_key(|t| t.calc.0);
        self.schedule = self.build_schedule(&triggers);
        // 阶段二：执行。零序约束（快照读 + D1 + 写局部 + effect-confinement）：帧内任何
        // 调度对提交 store 语义等价（精确命题见上）。写落本地缓冲、帧界提交——无原子、无伪共享。
        let (exec_buf, spawns) =
            run_triggers(&self.store, &self.calcs, self.detect, self.frame, &triggers);
        self.spawn_queue.extend(spawns);
        for rec in &clock_writes {
            self.store.set(rec.inst, rec.field, rec.new.clone());
        }
        // 帧边界：提交 + 生命周期结算
        self.commit(exec_buf);
    }

    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// 检视用快照读（上一帧提交值）。
    pub fn read(&self, inst: InstanceId, field: FieldId) -> Value {
        self.store.read(inst, field)
    }

    /// 测试/演示用外部激励：语义同 runtime 内建 writer 的一次普通 write。
    pub fn debug_write(&mut self, inst: InstanceId, field: FieldId, v: Value) {
        validate_external_write(&self.store, self.clock.ty, inst, field)
            .unwrap_or_else(|e| panic!("{e}"));
        if !self.store.alive(inst) {
            return;
        }
        let old = self.store.read(inst, field);
        maintain_ref_reverse(
            &self.store,
            &mut self.idx.ref_reverse,
            inst,
            field,
            &old,
            &v,
        );
        self.store.set(inst, field, v.clone());
        self.pending.push(WriteRec {
            inst,
            field,
            old,
            new: v,
        });
    }

    /// 从分组后的触发序构建本帧执行计划（C1/C2/C3 解析）。
    fn build_schedule(&self, triggers: &[Trigger]) -> Schedule {
        let mut groups: Vec<ScheduleGroup> = vec![];
        for t in triggers {
            match groups.last_mut() {
                Some(g) if g.calc == t.calc => g.count += 1,
                _ => {
                    let rc = &self.calcs[t.calc.0 as usize];
                    groups.push(ScheduleGroup {
                        calc: t.calc,
                        count: 1,
                        tier: rc.opts.tier,
                        residency: self.suggest_residency(t.calc),
                        reads: rc.opts.reads.clone(),
                    });
                }
            }
        }
        Schedule { groups }
    }

    /// C3 驻留解析：pin 直接采纳；`Auto` 用免费 profiler 遥测给建议（B 层）——
    /// 高扇出 + kernel 档 → 建议 GPU，否则 CPU。真实后端应在此加滞回防抖；
    /// 本实现是 seam + 建议（无 GPU 后端），证明 C3 标注已上游集成（非惰性元数据）。
    fn suggest_residency(&self, calc: CalcId) -> Residency {
        let rc = &self.calcs[calc.0 as usize];
        match rc.opts.residency {
            Residency::Cpu => Residency::Cpu,
            Residency::Gpu => Residency::Gpu,
            Residency::Auto => {
                let hot = self.profile.triggers(calc) >= GPU_HINT_THRESHOLD;
                if hot && rc.opts.tier == Tier::Kernel {
                    Residency::Gpu
                } else {
                    Residency::Cpu
                }
            }
        }
    }

    /// ECS 快路触发：等价于把本帧 Clock.frame 写路由给恒真 type 订阅，
    /// 但免去索引查找与条件判定，直接稠密遍历订阅类型的存活行。
    fn push_ecs_triggers(&self, triggers: &mut Vec<Trigger>) {
        if self.ecs_calcs.is_empty() {
            return;
        }
        let w = WriteRec {
            inst: self.clock.inst,
            field: self.clock.f_frame,
            old: Value::Int(self.frame as i64 - 1),
            new: Value::Int(self.frame as i64),
        };
        for &c in &self.ecs_calcs {
            let rc = &self.calcs[c.0 as usize];
            let Delivery::Each(projs) = &rc.pred.delivery else {
                unreachable!()
            };
            self.store.for_each_alive(rc.ty, |sub| {
                triggers.push(Trigger {
                    calc: c,
                    subscriber: sub,
                    input: Input::Each(route::project(projs, &w, sub, &self.store)),
                });
            });
        }
    }

    /// 帧边界：提交执行期写缓冲 → 形成下一帧快照与写集；处理 spawn/destroy 结算。
    fn commit(&mut self, exec_buf: Vec<WriteRec>) {
        for (ty, init) in &self.spawn_queue {
            validate_spawn_init(&self.store, *ty, init).unwrap_or_else(|e| panic!("{e}"));
        }
        let mut next: Vec<WriteRec> = vec![];
        let mut deaths: Vec<InstanceId> = vec![];
        for rec in exec_buf {
            maintain_ref_reverse(
                &self.store,
                &mut self.idx.ref_reverse,
                rec.inst,
                rec.field,
                &rec.old,
                &rec.new,
            );
            self.store.set(rec.inst, rec.field, rec.new.clone());
            if rec.field == FIELD_ALIVE && rec.new == Value::Bool(false) {
                deaths.push(rec.inst);
            }
            next.push(rec);
        }
        for (ty, init) in std::mem::take(&mut self.spawn_queue) {
            spawn_now(
                &mut self.store,
                &mut self.idx.ref_reverse,
                ty,
                init,
                &mut next,
            );
        }
        // 行压缩 / 留洞按 RowPolicy（C6）在此批处理——帧内行结构稳定
        for d in deaths {
            // 成员死亡撤销：先从 min/max fold 多重集精确移除其贡献（§6.3），再结算行。
            self.revoke_dead_from_folds(d);
            settle_death(&mut self.store, &mut self.idx.ref_reverse, d, &mut next);
        }
        // 上一帧若有外部写（debug_write/spawn/destroy 残留），合并入下一帧写集
        next.extend(std::mem::take(&mut self.pending));
        self.pending = next;
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::calculation::SnapshotRead for Store {
    fn read(&self, inst: InstanceId, field: FieldId) -> Value {
        Store::read(self, inst, field)
    }
}

// ---- 阶段二：执行 ----

/// 单触发执行：返回 (写缓冲, spawn 请求)。线程本地缓冲——写从不落共享存储。
fn run_one(
    store: &Store,
    calcs: &[RegisteredCalc],
    detect: Detect,
    t: &Trigger,
) -> (
    Vec<(FieldId, Value)>,
    Vec<(EntityTypeId, Vec<(FieldId, Value)>)>,
) {
    if !store.alive(t.subscriber) {
        return (vec![], vec![]);
    }
    let rc = &calcs[t.calc.0 as usize];
    let mut ctx = Ctx {
        snapshot: store,
        self_id: t.subscriber,
        writes: vec![],
        spawns: vec![],
        detect,
        reads: rc.opts.reads.as_deref(),
        kernel: rc.opts.tier == Tier::Kernel,
        calc_name: &rc.name,
    };
    (rc.f)(&mut ctx, &t.input);
    (ctx.writes, ctx.spawns)
}

/// 执行全部触发并做写折叠。
///
/// 零序约束（白送优化）：快照读 + D1 + 写局部 + effect-confinement ⇒ 帧内任何调度对
/// 提交 store 语义等价（持久 cell 字段值；spawn 身份在 Free 下至多差 id 置换）。
/// 唯一例外（同实例多次 each 的折叠序）已被宣布 undefined。
/// `parallel` feature 下按触发并行（rayon），各触发写本地缓冲（无原子、无伪共享），
/// 折叠按触发序归并——故同一触发数组下提交 store 确定，并行不引入额外不确定性
///（前提同上：闭包无 ctx 外副作用；ambient 副作用不在本不变量范围内）。
fn run_triggers(
    store: &Store,
    calcs: &[RegisteredCalc],
    detect: Detect,
    frame: u64,
    triggers: &[Trigger],
) -> (Vec<WriteRec>, Vec<(EntityTypeId, Vec<(FieldId, Value)>)>) {
    #[cfg(feature = "parallel")]
    let results: Vec<_> = {
        use rayon::prelude::*;
        triggers
            .par_iter()
            .map(|t| run_one(store, calcs, detect, t))
            .collect()
    };
    #[cfg(not(feature = "parallel"))]
    let results: Vec<_> = triggers
        .iter()
        .map(|t| run_one(store, calcs, detect, t))
        .collect();

    let mut folded: Vec<WriteRec> = vec![];
    let mut by_cell: HashMap<CellAddr, usize> = HashMap::new();
    let mut spawns = vec![];
    for (t, (writes, sp)) in triggers.iter().zip(results) {
        spawns.extend(sp);
        let rc = &calcs[t.calc.0 as usize];
        for (field, v) in writes {
            if field == FIELD_ALIVE {
                assert!(
                    matches!(v, Value::Bool(false)),
                    "calculation {} 只能通过 destroy_self() 写 _alive=false",
                    rc.name
                );
            }
            assert!(
                rc.declared_writes.contains(&field) || field == FIELD_ALIVE,
                "calculation {} 写了未声明字段（D1 要求静态写集）",
                rc.name
            );
            // 写折叠（§2）：同一 calc 一次/多次运行对同字段的写折叠为一条
            let key = CellAddr {
                inst: t.subscriber,
                field,
            };
            match by_cell.get(&key) {
                Some(&i) => {
                    // C5 检测档位：debug 检测 / release 静默折叠（§8 开放问题四）
                    if folded[i].new != v && detect != Detect::Silent {
                        let msg = format!(
                            "[PCE] 帧 {frame}：{} 多次运行对同字段写入不同值，折叠顺序未定义（§2）",
                            rc.name
                        );
                        if detect == Detect::Strict {
                            panic!("{msg}");
                        }
                        eprintln!("{msg}");
                    }
                    folded[i].new = v;
                }
                None => {
                    by_cell.insert(key, folded.len());
                    let old = store.read(t.subscriber, field);
                    folded.push(WriteRec {
                        inst: t.subscriber,
                        field,
                        old,
                        new: v,
                    });
                }
            }
        }
    }
    (folded, spawns)
}

fn check_field(store: &Store, ty: EntityTypeId, field: FieldId) -> Result<(), String> {
    if (ty.0 as usize) >= store.types.len() {
        return Err(format!("无类型 id {}", ty.0));
    }
    if (field.0 as usize) < store.fields(ty).len() {
        Ok(())
    } else {
        Err(format!(
            "类型 {} 无字段 id {}",
            store.type_name(ty),
            field.0
        ))
    }
}

fn validate_spawn_init(
    store: &Store,
    ty: EntityTypeId,
    init: &[(FieldId, Value)],
) -> Result<(), String> {
    if (ty.0 as usize) >= store.types.len() {
        return Err(format!("无类型 id {}", ty.0));
    }
    for &(field, _) in init {
        check_field(store, ty, field)?;
        if field == FIELD_ALIVE {
            return Err("不能在 spawn init 中写 _alive；请使用 Runtime::spawn/Runtime::destroy 管理生命周期".into());
        }
    }
    Ok(())
}

fn validate_external_write(
    store: &Store,
    clock_ty: EntityTypeId,
    inst: InstanceId,
    field: FieldId,
) -> Result<(), String> {
    check_field(store, inst.ty, field)?;
    if inst.ty == clock_ty {
        return Err("Clock 的 cell 由 runtime 内建 writer 独占；请使用 Runtime::set_alarm".into());
    }
    if field == FIELD_ALIVE {
        return Err("不能用 debug_write 写 _alive；请使用 Runtime::destroy".into());
    }
    Ok(())
}

/// scope 展平为「合取的析取组」：(a|b) & c → [[a,b],[c]]。
/// 更深的嵌套（合取再并）拒绝——组合需求物化中间量（§1.4、§6.1）。
fn flatten_scope(s: &Scope) -> Result<Vec<Vec<Atom>>, String> {
    match s {
        Scope::Own(f) => Ok(vec![vec![Atom::Own(*f)]]),
        Scope::Inst { ref_field, field } => Ok(vec![vec![Atom::Inst {
            ref_field: *ref_field,
            field: *field,
        }]]),
        Scope::Type(ty, f) => Ok(vec![vec![Atom::Type(*ty, *f)]]),
        Scope::Or(a, b) => {
            let (mut ga, gb) = (flatten_scope(a)?, flatten_scope(b)?);
            if ga.len() != 1 || gb.len() != 1 {
                return Err("scope 形如 (a&b)|c 未支持：把中间量物化为 entity（§6.1）".into());
            }
            ga[0].extend(gb.into_iter().next().unwrap());
            Ok(ga)
        }
        Scope::And(a, b) => {
            let mut ga = flatten_scope(a)?;
            ga.extend(flatten_scope(b)?);
            Ok(ga)
        }
    }
}

/// 创建实例：分配 id（代际号防 ABA），runtime 代写 `_alive = true` 与初始字段。
fn spawn_now(
    store: &mut Store,
    ref_reverse: &mut HashMap<InstanceId, HashSet<(InstanceId, FieldId)>>,
    ty: EntityTypeId,
    init: Vec<(FieldId, Value)>,
    recs: &mut Vec<WriteRec>,
) -> InstanceId {
    let inst = store.alloc(ty);
    recs.push(WriteRec {
        inst,
        field: FIELD_ALIVE,
        old: Value::Bool(false),
        new: Value::Bool(true),
    });
    store.set(inst, FIELD_ALIVE, Value::Bool(true));
    for (field, v) in init {
        let old = store.read(inst, field);
        maintain_ref_reverse(store, ref_reverse, inst, field, &old, &v);
        store.set(inst, field, v.clone());
        recs.push(WriteRec {
            inst,
            field,
            old,
            new: v,
        });
    }
    inst
}

/// 销毁结算（帧边界，§6.3）：沿反向表把所有指向死者的 ref cell 写成 null
/// ——普通 write，下一帧持有者用 `became(null)` 收尸；解除其 inst 订阅
/// （本实现 inst 路由经反向表查询，删表即解除）；id 归还复用，行按
/// RowPolicy 留洞或压缩（C6）。
fn settle_death(
    store: &mut Store,
    ref_reverse: &mut HashMap<InstanceId, HashSet<(InstanceId, FieldId)>>,
    dead: InstanceId,
    recs: &mut Vec<WriteRec>,
) {
    if let Some(holders) = ref_reverse.remove(&dead) {
        for (holder, rf) in holders {
            if store.alive(holder) {
                let old = store.read(holder, rf);
                store.set(holder, rf, Value::Null);
                recs.push(WriteRec {
                    inst: holder,
                    field: rf,
                    old,
                    new: Value::Null,
                });
            }
        }
    }
    // 移除死者自己持有的 ref 的反向项
    let nfields = store.fields(dead.ty).len();
    for fi in 0..nfields {
        let f = FieldId(fi as u32);
        if store.is_ref_field(dead.ty, f) {
            if let Value::Ref(target) = store.read(dead, f) {
                if let Some(set) = ref_reverse.get_mut(&target) {
                    set.remove(&(dead, f));
                }
            }
        }
    }
    store.release(dead);
}

/// ref 反向表维护：每次 ref 类型 cell 提交时增删对应反向项。
fn maintain_ref_reverse(
    store: &Store,
    ref_reverse: &mut HashMap<InstanceId, HashSet<(InstanceId, FieldId)>>,
    inst: InstanceId,
    field: FieldId,
    old: &Value,
    new: &Value,
) {
    if !store.is_ref_field(inst.ty, field) {
        return;
    }
    if let Value::Ref(t) = old {
        if let Some(set) = ref_reverse.get_mut(t) {
            set.remove(&(inst, field));
        }
    }
    if let Value::Ref(t) = new {
        ref_reverse.entry(*t).or_default().insert((inst, field));
    }
}
