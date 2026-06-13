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
//! SoA 列存（[`store`]）；单存储 + 写日志双缓冲；值桶 / 共享排序阈值表 /
//! crossed 区间查询（[`route`]）；等价条件合并求值；fold 增量维护（min/max
//! 多重集）；Clock 谓词退化为 ECS 稠密遍历（注册期识别）；帧 scratch 缓冲
//! 跨帧复用；免费 profiler（[`Profile`]，D2 送的遥测）。
//! 执行阶段零序约束 + 无原子：写从不落共享存储，落本地写缓冲，提交在帧界
//! ——`parallel` feature 下执行阶段按触发并行（D1 + 写局部性保证无竞争）。
//!
//! ## 开发者档位（C 层）
//! C1 [`Tier`]、C2 [`CalcOptions::reads`]、C3 [`Residency`]、
//! C4 [`Determinism`]、C5 [`Detect`]、C6 [`store::RowPolicy`]。

pub mod clock;
mod route;
mod store;

pub use store::{RowPolicy, Store};

use std::collections::{HashMap, HashSet};

use crate::calculation::{CalcFn, CalcId, Ctx, Input};
use crate::entity::{
    CellAddr, EntityTypeId, FieldDef, FieldId, InstanceId, FIELD_ALIVE,
};
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
        if cfg!(debug_assertions) { Detect::Warn } else { Detect::Silent }
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
        self.trigger_counts.get(calc.0 as usize).copied().unwrap_or(0)
    }

    /// 写频降序的热 cell 列表（(type, field), 次数）。
    pub fn hot_cells(&self) -> Vec<((EntityTypeId, FieldId), u64)> {
        let mut v: Vec<_> = self.write_counts.iter().map(|(k, c)| (*k, *c)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
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
    pub declared_writes: HashSet<FieldId>,
    pub opts: CalcOptions,
    pub f: CalcFn,
}

/// scope 注册期编译产物：合取组的析取原子（(a|b) & c → [[a,b],[c]]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// 帧 N-1 提交的写集，本帧路由。唯一触发源（§0）。
    pending: Vec<WriteRec>,
    /// calculation 请求的创建，帧边界生效。
    spawn_queue: Vec<(EntityTypeId, Vec<(FieldId, Value)>)>,
    /// 路由期临时结构，跨帧复用容量（帧 arena 的工程形）。
    scratch: route::Scratch,
    profile: Profile,
    detect: Detect,
    determinism: Determinism,
    clock: clock::Clock,
    frame: u64,
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
            pending: vec![],
            spawn_queue: vec![],
            scratch: route::Scratch::default(),
            profile: Profile::default(),
            detect: Detect::default(),
            determinism: Determinism::default(),
            clock: clock::Clock::placeholder(),
            frame: 0,
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
        // D1 单写者冲突检查
        for &w in declared_writes {
            if w == FIELD_ALIVE {
                continue;
            }
            if ty == self.clock.ty {
                return Err("Clock 的 cell 由 runtime 内建 writer 独占".into());
            }
            if let Some(prev) = self.field_owner.insert((ty, w), id) {
                return Err(format!(
                    "D1 冲突：{}.{} 已归属 {}",
                    self.store.type_name(ty),
                    self.store.field_name(ty, w),
                    self.calcs[prev.0 as usize].name
                ));
            }
        }
        // scope 展平为合取组
        let groups = flatten_scope(&pred.scope)?;
        if groups.len() > 1 && !matches!(pred.delivery, Delivery::Each(_)) {
            return Err("合取 scope 暂仅支持 each 交付（batch/fold 下的合取语义未定，§8）".into());
        }
        // ECS 快路识别（白送优化）：type(Clock, frame) + 恒真条件 + each
        // ≅ 经典 ECS system——跳过路由，step 时稠密列遍历直触发。
        let is_ecs = groups.len() == 1
            && groups[0].len() == 1
            && groups[0][0] == Atom::Type(self.clock.ty, self.clock.f_frame)
            && matches!(pred.cond, Cond::True)
            && matches!(pred.delivery, Delivery::Each(_));
        if is_ecs {
            self.ecs_calcs.push(id);
        } else {
            // 原子校验与索引挂接
            for (gi, group) in groups.iter().enumerate() {
                for atom in group {
                    match *atom {
                        Atom::Own(field) => {
                            check_field(&self.store, ty, field)?;
                            self.idx.own.entry((ty, field)).or_default().push((id, gi as u32));
                        }
                        Atom::Inst { ref_field, field } => {
                            check_field(&self.store, ty, ref_field)?;
                            if !self.store.is_ref_field(ty, ref_field) {
                                return Err(format!(
                                    "inst scope 的 ref 必须来自自己实例的 ref 类型字段（§3.2），字段 {} 不是 ref",
                                    self.store.field_name(ty, ref_field)
                                ));
                            }
                            self.idx
                                .inst
                                .entry((ty, ref_field, field))
                                .or_default()
                                .push((id, gi as u32));
                        }
                        Atom::Type(watched_ty, field) => {
                            check_field(&self.store, watched_ty, field)?;
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
        self.profile.trigger_counts.push(0);
        self.calcs.push(RegisteredCalc {
            name: name.to_string(),
            ty,
            n_groups: groups.len() as u32,
            cond_indep,
            cond_class,
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
        let mut recs = std::mem::take(&mut self.pending);
        let inst = spawn_now(&mut self.store, &mut self.idx.ref_reverse, ty, init, &mut recs);
        self.pending = recs;
        inst
    }

    /// 外部销毁 API：语义等价于代为写入 `_alive = false`（§6.3），帧边界结算。
    pub fn destroy(&mut self, inst: InstanceId) {
        if self.store.alive(inst) {
            let old = self.store.read(inst, FIELD_ALIVE);
            self.store.set(inst, FIELD_ALIVE, Value::Bool(false));
            self.pending.push(WriteRec { inst, field: FIELD_ALIVE, old, new: Value::Bool(false) });
            let mut recs = std::mem::take(&mut self.pending);
            settle_death(&mut self.store, &mut self.idx.ref_reverse, inst, &mut recs);
            self.pending = recs;
        }
    }

    // ---- 帧循环 ----

    /// 推进一帧：路由上一帧写集 → 执行触发的 calculation → 帧边界提交与结算。
    pub fn step(&mut self) {
        self.frame += 1;
        let mut w = std::mem::take(&mut self.pending);
        // runtime 内建 writer：Clock.frame 每帧写入（订阅它=显式轮询，§6.2）；alarm 到点写
        self.clock.tick(self.frame, &mut self.store, &mut w);
        // 免费 profiler：路由输入就是每 cell 写频（D2）
        self.profile.frames = self.frame;
        self.profile.last_writes = w.len();
        for rec in &w {
            *self.profile.write_counts.entry((rec.inst.ty, rec.field)).or_default() += 1;
        }
        // 阶段一：路由（索引查找 → 条件判定 → 交付物化）
        let mut triggers = route::route(
            &self.store,
            &self.idx,
            &self.calcs,
            &mut self.fold_state,
            &mut self.scratch,
            &w,
            self.determinism,
        );
        // ECS 快路：跳过路由，稠密列遍历直触发
        self.push_ecs_triggers(&mut triggers);
        self.profile.last_triggers = triggers.len();
        for t in &triggers {
            self.profile.trigger_counts[t.calc.0 as usize] += 1;
        }
        // 阶段二：执行。零序约束（快照读 + D1 + 写局部）：帧内任何调度语义等价。
        // 写落本地缓冲、帧界提交——无原子、无伪共享。
        let (exec_buf, spawns) =
            run_triggers(&self.store, &self.calcs, self.detect, self.frame, &triggers);
        self.spawn_queue.extend(spawns);
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
        let old = self.store.read(inst, field);
        maintain_ref_reverse(&self.store, &mut self.idx.ref_reverse, inst, field, &old, &v);
        self.store.set(inst, field, v.clone());
        self.pending.push(WriteRec { inst, field, old, new: v });
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
            let Delivery::Each(projs) = &rc.pred.delivery else { unreachable!() };
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
            spawn_now(&mut self.store, &mut self.idx.ref_reverse, ty, init, &mut next);
        }
        // 行压缩 / 留洞按 RowPolicy（C6）在此批处理——帧内行结构稳定
        for d in deaths {
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
) -> (Vec<(FieldId, Value)>, Vec<(EntityTypeId, Vec<(FieldId, Value)>)>) {
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
/// 零序约束（白送优化）：快照读 + D1 + 写局部 ⇒ 帧内任何调度语义等价；
/// 唯一例外（同实例多次 each 的折叠序）已被宣布 undefined。
/// `parallel` feature 下按触发并行（rayon），各触发写本地缓冲（无原子、
/// 无伪共享），折叠按触发序归并——并行调度不引入额外不确定性。
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
        triggers.par_iter().map(|t| run_one(store, calcs, detect, t)).collect()
    };
    #[cfg(not(feature = "parallel"))]
    let results: Vec<_> = triggers.iter().map(|t| run_one(store, calcs, detect, t)).collect();

    let mut folded: Vec<WriteRec> = vec![];
    let mut by_cell: HashMap<CellAddr, usize> = HashMap::new();
    let mut spawns = vec![];
    for (t, (writes, sp)) in triggers.iter().zip(results) {
        spawns.extend(sp);
        let rc = &calcs[t.calc.0 as usize];
        for (field, v) in writes {
            assert!(
                rc.declared_writes.contains(&field) || field == FIELD_ALIVE,
                "calculation {} 写了未声明字段（D1 要求静态写集）",
                rc.name
            );
            // 写折叠（§2）：同一 calc 一次/多次运行对同字段的写折叠为一条
            let key = CellAddr { inst: t.subscriber, field };
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
                    folded.push(WriteRec { inst: t.subscriber, field, old, new: v });
                }
            }
        }
    }
    (folded, spawns)
}

fn check_field(store: &Store, ty: EntityTypeId, field: FieldId) -> Result<(), String> {
    if (field.0 as usize) < store.fields(ty).len() {
        Ok(())
    } else {
        Err(format!("类型 {} 无字段 id {}", store.type_name(ty), field.0))
    }
}

/// scope 展平为「合取的析取组」：(a|b) & c → [[a,b],[c]]。
/// 更深的嵌套（合取再并）拒绝——组合需求物化中间量（§1.4、§6.1）。
fn flatten_scope(s: &Scope) -> Result<Vec<Vec<Atom>>, String> {
    match s {
        Scope::Own(f) => Ok(vec![vec![Atom::Own(*f)]]),
        Scope::Inst { ref_field, field } => {
            Ok(vec![vec![Atom::Inst { ref_field: *ref_field, field: *field }]])
        }
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
        recs.push(WriteRec { inst, field, old, new: v });
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
                recs.push(WriteRec { inst: holder, field: rf, old, new: Value::Null });
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
