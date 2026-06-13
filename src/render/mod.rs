//! render runtime：第二个 runtime 实例，动态帧率，消费者角色。
//!
//! 设计定理（见会话分析）：**render runtime = simulation runtime − 生命周期
//! + 动态时钟 + sim 写日志摄入口 + 插值原语**。四层封闭不破——render 逻辑同样由
//! predicate + calculation 构建，只是：
//!
//! 1. **触发源有两条**（与 sim 的「唯一触发源」对偶）：
//!    - render clock tick → 连续更新（[`RenderRuntime::continuous`]），ECS 稠密扫，
//!      render 的主热路径（插值天生每帧）；
//!    - 摄入的 sim 写日志 → 事件反应（[`RenderRuntime::reaction`]），复用谓词代数。
//! 2. **生命周期 sim 独占**：render 无 spawn/destroy 共享实体的入口（你的约束）；
//!    render 的 sidecar 行被动跟随 sim 的生灭增量。
//! 3. **字段级 D1 单向依赖**：render 只写 render 字段（[`store::RFieldId`]）、只读
//!    sim 字段（经 tracked 镜像）；sim 永不读 render——并发解耦的结构强制（A7）。
//!
//! ## 三类注册
//! - [`RenderRuntime::track`]：声明镜像某 sim 字段并按 [`Interp`] 维护插值输出
//!   （`fold` 的 render 对偶，runtime 自动每帧维护，calc 不手算）。
//! - [`RenderRuntime::reaction`]：sim 写谓词 → render 字段写（死亡淡出、命中火花、
//!   状态切换起动画）。v1 订阅者 = writer 本身。
//! - [`RenderRuntime::continuous`]：render clock 每帧对每个存活实例运行（相机阻尼、
//!   派生 transform）。
//!
//! ## 成本（render 侧不变量）
//! 每 render 帧：O(本 sim 区间在动 ∩ 存活) 的插值重算（A8 稀疏性经写日志延伸到
//! 连续更新）+ O(存活) 的连续扫；每 sim 帧：O(|事件|) 的反应路由。剔除/LOD
//! （Cr3，§6.1 物化为可见集实体）进一步压低存活扫的 N，留作后续。

pub mod clock;
pub mod ctx;
pub mod handoff;
pub mod interp;
pub mod store;

pub use clock::RenderClock;
pub use ctx::{ContinuousFn, ReactionFn, RenderCtx, RenderInput};
pub use handoff::{Publisher, SimFrame, TrackedDelta};
pub use interp::{Interp, Track};
pub use store::{RFieldId, RenderStore};

use std::collections::{HashMap, HashSet};

use crate::entity::{EntityTypeId, FieldId, InstanceId};
use crate::predicate::{Cond, Expr, Proj, ValRef};
use crate::runtime::Runtime;
use crate::value::Value;

struct Reaction {
    #[allow(dead_code)]
    name: String,
    /// scope：`type(ty, sim_field)`。v1 订阅者 = writer 本身。
    ty: EntityTypeId,
    sim_field: FieldId,
    cond: Cond,
    projs: Vec<Proj>,
    batched: bool,
    writes: HashSet<RFieldId>,
    f: ReactionFn,
}

struct Continuous {
    #[allow(dead_code)]
    name: String,
    ty: EntityTypeId,
    writes: HashSet<RFieldId>,
    f: ContinuousFn,
}

/// 第二个 runtime 实例：动态帧率的 render 侧。
pub struct RenderRuntime {
    store: RenderStore,
    tracks: Vec<Track>,
    /// (类型, sim 字段) → tracks 下标（可多个：同一 sim 字段可被多个 track 以不同
    /// 插值种类镜像；摄入增量时逐个喂，否则除最后注册者外全部停在默认值）。
    track_of: HashMap<(EntityTypeId, FieldId), Vec<usize>>,
    /// sim 字段默认值快照（构造期从 sim schema 取，出生 snap 用）。
    sim_defaults: HashMap<(EntityTypeId, FieldId), Value>,
    reactions: Vec<Reaction>,
    continuous: Vec<Continuous>,
    /// render 侧 D1：(类型, render 字段) → 归属者描述（注册期冲突即错）。
    field_owner: HashMap<(EntityTypeId, RFieldId), String>,
    clock: RenderClock,
    /// 已摄入的最新 sim 帧号（去重：同一 SimFrame 跨多个 render 帧只摄入一次）。
    last_ingested: u64,
    /// 本 sim 区间在动、需每 render 帧重算插值的 (实例, track 下标)（A8 稀疏集）。
    active: Vec<(InstanceId, usize)>,
}

impl RenderRuntime {
    /// 从 sim runtime 的 schema 构造（两侧共享 schema）。每个 sim 类型预留一个镜像，
    /// 并快照全字段默认值（出生 snap 用——render 只见写日志增量，未写出的 tracked
    /// 字段须从 schema 默认值取值）。
    pub fn new(sim: &Runtime) -> Self {
        let mut sim_defaults = HashMap::new();
        for t in 0..sim.type_count() {
            let ty = EntityTypeId(t as u32);
            for (fi, def) in sim.field_defaults(ty).into_iter().enumerate() {
                sim_defaults.insert((ty, FieldId(fi as u32)), def);
            }
        }
        RenderRuntime {
            store: RenderStore::new(sim.type_count()),
            tracks: vec![],
            track_of: HashMap::new(),
            sim_defaults,
            reactions: vec![],
            continuous: vec![],
            field_owner: HashMap::new(),
            clock: RenderClock::new(),
            last_ingested: 0,
            active: vec![],
        }
    }

    // ---- 注册期 ----

    /// 在某类型注册一个纯 render 字段（render 独占写，D1 render 侧）。
    pub fn add_render_field(&mut self, ty: EntityTypeId, default: Value) -> RFieldId {
        self.store.add_render_field(ty, default)
    }

    /// 声明镜像某 sim 字段并按 `kind` 维护插值输出，返回插值结果所在的 render 字段。
    /// 这是 render 侧的 `fold`：runtime 每帧增量维护，连续 calc 直接读输出字段。
    ///
    /// 同一 sim 字段可被多次 track（不同插值种类各自一个输出字段）。输出字段经
    /// `claim_writes` 登记 D1 归属（与 reaction/continuous 同纪律，冲突即错）。
    pub fn track(
        &mut self,
        ty: EntityTypeId,
        sim_field: FieldId,
        kind: Interp,
    ) -> Result<RFieldId, String> {
        let default = self.sim_defaults.get(&(ty, sim_field)).cloned().unwrap_or(Value::Null);
        let out = self.store.add_render_field(ty, Value::Null);
        // 输出字段走 D1 登记（out 恒为新铸 id，但宿主可手构 RFieldId 抢注，故仍检查）。
        self.claim_writes(ty, &[out], &format!("track({}.{})", ty.0, sim_field.0))?;
        let slot = self.store.add_track(ty, sim_field, default);
        let idx = self.tracks.len();
        self.tracks.push(Track { ty, sim_field, out, slot, kind });
        self.track_of.entry((ty, sim_field)).or_default().push(idx);
        Ok(out)
    }

    /// 注册 render 事件反应。`cond` 只许引用 new/old/常量（render 反应不点查订阅者
    /// 行——v1 约束；越界在此报错）。`projs` 只许 new/old/writer。
    #[allow(clippy::too_many_arguments)]
    pub fn reaction(
        &mut self,
        name: &str,
        ty: EntityTypeId,
        sim_field: FieldId,
        cond: Cond,
        projs: Vec<Proj>,
        batched: bool,
        writes: &[RFieldId],
        f: ReactionFn,
    ) -> Result<(), String> {
        validate_reaction_cond(&cond)?;
        validate_reaction_projs(&projs)?;
        self.claim_writes(ty, writes, name)?;
        self.reactions.push(Reaction {
            name: name.to_string(),
            ty,
            sim_field,
            cond,
            projs,
            batched,
            writes: writes.iter().copied().collect(),
            f,
        });
        Ok(())
    }

    /// 注册 render 连续更新：render clock 每帧对该类型每个存活实例运行一次。
    pub fn continuous(
        &mut self,
        name: &str,
        ty: EntityTypeId,
        writes: &[RFieldId],
        f: ContinuousFn,
    ) -> Result<(), String> {
        self.claim_writes(ty, writes, name)?;
        self.continuous.push(Continuous {
            name: name.to_string(),
            ty,
            writes: writes.iter().copied().collect(),
            f,
        });
        Ok(())
    }

    /// D1 render 侧：render 字段静态归属唯一写者，注册期冲突即错。
    /// 先整片校验（含片内重复）再插入——失败时不留半截脏归属（错误路径整洁）。
    fn claim_writes(&mut self, ty: EntityTypeId, writes: &[RFieldId], owner: &str) -> Result<(), String> {
        for (i, &w) in writes.iter().enumerate() {
            if let Some(prev) = self.field_owner.get(&(ty, w)) {
                return Err(format!(
                    "D1（render 侧）冲突：render 字段 {}.{} 已归属 {prev}",
                    ty.0, w.0
                ));
            }
            if writes[..i].contains(&w) {
                return Err(format!("{owner} 重复声明 render 字段 {}.{}", ty.0, w.0));
            }
        }
        for &w in writes {
            self.field_owner.insert((ty, w), owner.to_string());
        }
        Ok(())
    }

    /// 本 render runtime 关心的 tracked (类型, sim 字段) 集——用于构建 [`Publisher`]。
    pub fn tracked_fields(&self) -> Vec<(EntityTypeId, FieldId)> {
        self.tracks.iter().map(|t| (t.ty, t.sim_field)).collect()
    }

    // ---- 帧循环 ----

    /// 摄入一份 sim 快照（新于已摄入帧才生效；同一 SimFrame 跨多 render 帧只摄入
    /// 一次）。顺序：出生 → tracked 增量（含停动结算）→ 事件反应 → 死亡。
    pub fn ingest(&mut self, sf: &SimFrame) {
        if sf.sim_frame <= self.last_ingested {
            return;
        }
        // 1. 出生：行须先于增量/反应存在。birth 已把每个 track 的 prev/cur snap 到
        //    sim 默认值；这里把输出字段也 snap 一次（未被 init 写入的 tracked 字段否则
        //    永远停在 Null——若同帧 init 携带了值，下面的 apply_delta 会再覆盖）。
        for &inst in &sf.births {
            self.store.birth(inst);
            for ti in 0..self.tracks.len() {
                let tr = self.tracks[ti];
                if tr.ty != inst.ty {
                    continue;
                }
                if let Some((prev, cur)) = self.store.track_pair(inst, tr.slot) {
                    self.store.write_render(inst, tr.out, tr.kind.sample(&prev, &cur, 1.0));
                }
            }
        }
        let births: HashSet<InstanceId> = sf.births.iter().copied().collect();
        // 2. tracked 增量 → 镜像 + 活动集（停动的 cell 结算到 cur）。同一 sim 字段的
        //    全部 track 都喂（track_of 是多值），否则除最后注册者外都停在默认值。
        let prev_active = std::mem::take(&mut self.active);
        let mut new_active: Vec<(InstanceId, usize)> = vec![];
        for d in &sf.tracked {
            if let Some(tis) = self.track_of.get(&(d.inst.ty, d.sim_field)) {
                let just_born = births.contains(&d.inst);
                for &ti in tis {
                    let slot = self.tracks[ti].slot;
                    self.store
                        .apply_delta(d.inst, slot, d.old.clone(), d.new.clone(), just_born);
                    new_active.push((d.inst, ti));
                }
            }
        }
        let new_set: HashSet<(InstanceId, usize)> = new_active.iter().copied().collect();
        for (inst, ti) in prev_active {
            if !new_set.contains(&(inst, ti)) {
                // 上一区间在动、本区间不动：把插值输出结算到静止位（= 上一区间 alpha=1
                // 的取值，cur）。经 kind.sample 求值，保证与活动扫输出同型（Lerp 出 Float）。
                let tr = self.tracks[ti];
                if let Some((prev, cur)) = self.store.track_pair(inst, tr.slot) {
                    self.store.write_render(inst, tr.out, tr.kind.sample(&prev, &cur, 1.0));
                }
            }
        }
        self.active = new_active;
        // 3. 事件反应（订阅者 = writer，行仍存活；先于死亡结算）。
        self.run_reactions(sf);
        // 4. 死亡：回收 render 行（v1 即时回收；render 自管寿命/淡出留作后续）。
        for &inst in &sf.deaths {
            self.store.death(inst);
        }
        self.last_ingested = sf.sim_frame;
    }

    /// 推进一个 render 帧：更新时钟 → 插值重算（仅活动集，A8）→ 连续更新。
    pub fn render_frame(&mut self, dt: f64, alpha: f64) {
        self.clock.begin_frame(dt, alpha);
        let alpha = self.clock.alpha;
        // 插值扫：只碰本区间在动的 cell（稀疏）。静止物的输出字段已在结算时落到 cur。
        let active = std::mem::take(&mut self.active);
        for &(inst, ti) in &active {
            let tr = self.tracks[ti];
            if let Some((prev, cur)) = self.store.track_pair(inst, tr.slot) {
                let out = tr.kind.sample(&prev, &cur, alpha);
                self.store.write_render(inst, tr.out, out);
            }
        }
        self.active = active;
        self.run_continuous();
    }

    /// 规范宿主入口：取走 publisher 全部未消费帧、顺序摄入（不丢生灭/事件），再推进
    /// 一个 render 帧。单线程与并发双线程的每个 render 帧都调它即可。若要在两个 sim
    /// 帧之间画多个插值帧，调一次 `sync` 后续接若干 [`RenderRuntime::render_frame`]
    /// （只变 alpha、不再摄入）。
    pub fn sync(&mut self, publisher: &Publisher, dt: f64, alpha: f64) {
        for sf in publisher.drain() {
            self.ingest(&sf);
        }
        self.render_frame(dt, alpha);
    }

    fn run_continuous(&mut self) {
        for ci in 0..self.continuous.len() {
            let ty = self.continuous[ci].ty;
            let declared = self.continuous[ci].writes.clone();
            let mut insts = vec![];
            self.store.for_each_live(ty, |i| insts.push(i));
            for inst in insts {
                let writes = {
                    let mut ctx = RenderCtx { store: &self.store, self_id: inst, clock: self.clock, writes: vec![] };
                    (self.continuous[ci].f)(&mut ctx);
                    ctx.writes
                };
                commit_writes(&mut self.store, inst, &declared, writes);
            }
        }
    }

    fn run_reactions(&mut self, sf: &SimFrame) {
        for ri in 0..self.reactions.len() {
            let ty = self.reactions[ri].ty;
            let sim_field = self.reactions[ri].sim_field;
            let batched = self.reactions[ri].batched;
            let declared = self.reactions[ri].writes.clone();
            // 收集命中（先收集再运行，避开借用交叠）。
            let mut hits: Vec<(InstanceId, Vec<Value>)> = vec![];
            for rec in &sf.events {
                if rec.inst.ty == ty
                    && rec.field == sim_field
                    && eval_reaction_cond(&self.reactions[ri].cond, &rec.new, &rec.old)
                {
                    hits.push((rec.inst, project_reaction(&self.reactions[ri].projs, rec)));
                }
            }
            if batched {
                let mut groups: HashMap<InstanceId, Vec<Vec<Value>>> = HashMap::new();
                let mut order: Vec<InstanceId> = vec![];
                for (inst, row) in hits {
                    groups.entry(inst).or_insert_with(|| {
                        order.push(inst);
                        vec![]
                    });
                    groups.get_mut(&inst).unwrap().push(row);
                }
                for inst in order {
                    if !self.store.alive(inst) {
                        continue;
                    }
                    let rows = groups.remove(&inst).unwrap();
                    let writes = {
                        let mut ctx = RenderCtx { store: &self.store, self_id: inst, clock: self.clock, writes: vec![] };
                        (self.reactions[ri].f)(&mut ctx, &RenderInput::Batch(rows));
                        ctx.writes
                    };
                    commit_writes(&mut self.store, inst, &declared, writes);
                }
            } else {
                for (inst, row) in hits {
                    if !self.store.alive(inst) {
                        continue;
                    }
                    let writes = {
                        let mut ctx = RenderCtx { store: &self.store, self_id: inst, clock: self.clock, writes: vec![] };
                        (self.reactions[ri].f)(&mut ctx, &RenderInput::Each(row));
                        ctx.writes
                    };
                    commit_writes(&mut self.store, inst, &declared, writes);
                }
            }
        }
    }

    // ---- 检视 ----

    /// 读某实例的 render 字段（检视 / 提交给 GPU 前取值）。
    pub fn read(&self, inst: InstanceId, f: RFieldId) -> Value {
        self.store.read_render(inst, f)
    }

    pub fn alive(&self, inst: InstanceId) -> bool {
        self.store.alive(inst)
    }

    pub fn clock(&self) -> RenderClock {
        self.clock
    }

    pub fn store(&self) -> &RenderStore {
        &self.store
    }

    /// 已摄入的最新 sim 帧号（检视握手进度）。
    pub fn last_ingested(&self) -> u64 {
        self.last_ingested
    }

    /// 当前「本区间在动」的活动 cell 数（检视 A8 稀疏性：静止场景应趋零）。
    pub fn active_count(&self) -> usize {
        self.active.len()
    }
}

/// 写折叠（render 侧）：同 calc 一次运行对同 render 字段的多次写取最终值；D1 校验。
fn commit_writes(
    store: &mut RenderStore,
    inst: InstanceId,
    declared: &HashSet<RFieldId>,
    writes: Vec<(RFieldId, Value)>,
) {
    let mut folded: Vec<(RFieldId, Value)> = vec![];
    for (f, v) in writes {
        // 硬断言（与 sim 引擎 run_triggers 同纪律）：未声明写破坏 D1 render 侧。
        assert!(
            declared.contains(&f),
            "render calc 写了未声明的 render 字段 {}（D1 render 侧要求静态写集）",
            f.0
        );
        if let Some(slot) = folded.iter_mut().find(|(ff, _)| *ff == f) {
            slot.1 = v;
        } else {
            folded.push((f, v));
        }
    }
    for (f, v) in folded {
        store.write_render(inst, f, v);
    }
}

// ---- 反应条件 / 投影（v1 子集：new / old / 常量）----

fn validate_reaction_cond(c: &Cond) -> Result<(), String> {
    fn expr_ok(e: &Expr) -> bool {
        match e {
            Expr::Val(v) => !matches!(v, ValRef::Own(_) | ValRef::SelfRef),
            Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) | Expr::Div(a, b) => {
                expr_ok(a) && expr_ok(b)
            }
        }
    }
    let ok = match c {
        Cond::True | Cond::InRange(..) | Cond::InSet(_) | Cond::Changed | Cond::Became(_) => true,
        Cond::Cmp(a, _, b) => expr_ok(a) && expr_ok(b),
        Cond::Crossed(t, _) => expr_ok(t),
        Cond::And(a, b) | Cond::Or(a, b) | Cond::AndNot(a, b) => {
            return validate_reaction_cond(a).and(validate_reaction_cond(b))
        }
    };
    if ok {
        Ok(())
    } else {
        Err("render 反应条件 v1 只许引用 new/old/常量（不点查订阅者行）".into())
    }
}

fn validate_reaction_projs(projs: &[Proj]) -> Result<(), String> {
    for p in projs {
        if matches!(p, Proj::Own(_)) {
            return Err("render 反应投影 v1 只许 new/old/writer（不投影 own）".into());
        }
    }
    Ok(())
}

fn eval_ro_expr(e: &Expr, new: &Value, old: &Value) -> Value {
    match e {
        Expr::Val(ValRef::New(p)) => new.get_path(p),
        Expr::Val(ValRef::Old(p)) => old.get_path(p),
        Expr::Val(ValRef::Const(v)) => v.clone(),
        Expr::Val(_) => Value::Null,
        Expr::Add(a, b) => ro_arith(a, b, new, old, |x, y| x + y),
        Expr::Sub(a, b) => ro_arith(a, b, new, old, |x, y| x - y),
        Expr::Mul(a, b) => ro_arith(a, b, new, old, |x, y| x * y),
        Expr::Div(a, b) => ro_arith(a, b, new, old, |x, y| x / y),
    }
}

fn ro_arith(a: &Expr, b: &Expr, new: &Value, old: &Value, f: fn(f64, f64) -> f64) -> Value {
    match (eval_ro_expr(a, new, old).as_f64(), eval_ro_expr(b, new, old).as_f64()) {
        (Some(x), Some(y)) => Value::Float(f(x, y)),
        _ => Value::Null,
    }
}

fn ro_val_eq(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

/// 反应条件求值（new/old/常量子集；与 route.rs 的 eval_cond 同义，去掉 own/self）。
fn eval_reaction_cond(cond: &Cond, new: &Value, old: &Value) -> bool {
    use crate::predicate::{CmpOp, Dir};
    match cond {
        Cond::True => true,
        Cond::Cmp(l, op, r) => {
            let (lv, rv) = (eval_ro_expr(l, new, old), eval_ro_expr(r, new, old));
            match op {
                CmpOp::Eq => ro_val_eq(&lv, &rv),
                CmpOp::Ne => !ro_val_eq(&lv, &rv),
                _ => match lv.cmp_num(&rv) {
                    Some(o) => match op {
                        CmpOp::Lt => o.is_lt(),
                        CmpOp::Le => o.is_le(),
                        CmpOp::Gt => o.is_gt(),
                        CmpOp::Ge => o.is_ge(),
                        _ => unreachable!(),
                    },
                    None => false,
                },
            }
        }
        Cond::InRange(a, b) => new.as_f64().is_some_and(|v| v >= *a && v <= *b),
        Cond::InSet(vs) => vs.iter().any(|v| ro_val_eq(v, new)),
        Cond::Changed => !ro_val_eq(new, old),
        Cond::Became(v) => ro_val_eq(new, v) && !ro_val_eq(old, v),
        Cond::Crossed(t, dir) => {
            let (Some(t), Some(o), Some(n)) =
                (eval_ro_expr(t, new, old).as_f64(), old.as_f64(), new.as_f64())
            else {
                return false;
            };
            match dir {
                Dir::Down => o >= t && n < t,
                Dir::Up => o <= t && n > t,
            }
        }
        Cond::And(a, b) => eval_reaction_cond(a, new, old) && eval_reaction_cond(b, new, old),
        Cond::Or(a, b) => eval_reaction_cond(a, new, old) || eval_reaction_cond(b, new, old),
        Cond::AndNot(a, b) => eval_reaction_cond(a, new, old) && !eval_reaction_cond(b, new, old),
    }
}

fn project_reaction(projs: &[Proj], rec: &crate::runtime::WriteRec) -> Vec<Value> {
    projs
        .iter()
        .map(|p| match p {
            Proj::New(path) => rec.new.get_path(path),
            Proj::Old(path) => rec.old.get_path(path),
            Proj::WriterId => Value::Ref(rec.inst),
            Proj::Own(_) => Value::Null, // 注册期已挡
        })
        .collect()
}
