//! runtime 层：唯一的调度者与索引持有者（§1.1）。
//!
//! 职责：维护数据双缓冲；收集帧 N 写集；帧 N+1 路由给谓词；维护谓词索引与
//! fold 增量状态；管理实例生命周期、id 分配与 ref 反向表（§6.3）；并作为
//! 系统内建 writer 向内建 cell 写入（Clock.frame、alarm、`_alive`、ref 置 null）。
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
//! ## 与验收红线的差距（脚手架阶段，按 §4 成本表逐项收敛）
//! - type scope + 等值/阈值条件目前是线性扇出，TODO：值桶 / 共享排序阈值表
//! - 等价谓词合并、包含关系消解（§5）未实现
//! - 路由/执行的分片并行未实现（架构上无数据竞争，D1 + 写局部性保证）

pub mod clock;
mod route;

use std::collections::{HashMap, HashSet};

use crate::calculation::{CalcFn, CalcId, Ctx, Input, SnapshotRead};
use crate::entity::{
    CellAddr, EntityType, EntityTypeId, FieldDef, FieldId, InstanceId, FIELD_ALIVE,
};
use crate::predicate::{Cond, Delivery, Expr, Predicate, Scope, ValRef};
use crate::value::Value;

/// 一条写记录。`inst` 既是被写实例也是 writer（写局部性：calculation 只能写自己）。
#[derive(Debug, Clone)]
pub struct WriteRec {
    pub inst: InstanceId,
    pub field: FieldId,
    /// 上一帧提交值（双缓冲免费可得）。
    pub old: Value,
    pub new: Value,
}

/// 已提交数据快照（双缓冲的「前台」）。执行期只读。
pub struct Store {
    types: Vec<EntityType>,
    slots: Vec<Vec<Option<Slot>>>,
    free: Vec<Vec<(u32, u32)>>, // (id, 下一任代际号)
}

struct Slot {
    generation: u32,
    fields: Vec<Value>,
}

impl Store {
    fn slot(&self, inst: InstanceId) -> Option<&Slot> {
        self.slots[inst.ty.0 as usize]
            .get(inst.id as usize)?
            .as_ref()
            .filter(|s| s.generation == inst.generation)
    }

    /// 快照读。代际不匹配（旧 ref 指向已复用的 id）读到 Null——ABA 防护。
    pub fn read(&self, inst: InstanceId, field: FieldId) -> Value {
        self.slot(inst)
            .and_then(|s| s.fields.get(field.0 as usize).cloned())
            .unwrap_or(Value::Null)
    }

    pub fn alive(&self, inst: InstanceId) -> bool {
        matches!(self.read(inst, FIELD_ALIVE), Value::Bool(true))
    }

    pub fn alive_instances(&self, ty: EntityTypeId) -> Vec<InstanceId> {
        self.slots[ty.0 as usize]
            .iter()
            .enumerate()
            .filter_map(|(id, s)| {
                let s = s.as_ref()?;
                let inst = InstanceId { ty, id: id as u32, generation: s.generation };
                self.alive(inst).then_some(inst)
            })
            .collect()
    }

    fn set(&mut self, inst: InstanceId, field: FieldId, v: Value) {
        let ok = self.slots[inst.ty.0 as usize]
            .get_mut(inst.id as usize)
            .and_then(|o| o.as_mut())
            .filter(|s| s.generation == inst.generation);
        if let Some(s) = ok {
            s.fields[field.0 as usize] = v;
        }
    }

    fn alloc(&mut self, ty: EntityTypeId) -> InstanceId {
        let defaults: Vec<Value> =
            self.types[ty.0 as usize].fields.iter().map(|f| f.default.clone()).collect();
        let (id, generation) = self.free[ty.0 as usize]
            .pop()
            .unwrap_or((self.slots[ty.0 as usize].len() as u32, 0));
        let slot = Slot { generation, fields: defaults };
        let v = &mut self.slots[ty.0 as usize];
        if (id as usize) < v.len() {
            v[id as usize] = Some(slot);
        } else {
            v.push(Some(slot));
        }
        InstanceId { ty, id, generation }
    }

    /// 释放：id 归还复用，代际号 +1 防 ABA（§6.3）。
    fn release(&mut self, inst: InstanceId) {
        if self.slot(inst).is_some() {
            self.slots[inst.ty.0 as usize][inst.id as usize] = None;
            self.free[inst.ty.0 as usize].push((inst.id, inst.generation + 1));
        }
    }

    fn is_ref_field(&self, ty: EntityTypeId, field: FieldId) -> bool {
        self.types[ty.0 as usize].fields[field.0 as usize].is_ref
    }
}

impl SnapshotRead for Store {
    fn read(&self, inst: InstanceId, field: FieldId) -> Value {
        Store::read(self, inst, field)
    }
}

/// scope 注册期编译产物：合取组的析取原子（(a|b) & c → [[a,b],[c]]）。
/// 组数 >1 时路由用每帧位码闩实现合取（§4「& 合取」行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Atom {
    Own(FieldId),
    Inst { ref_field: FieldId, field: FieldId },
    Type(EntityTypeId, FieldId),
}

pub(crate) struct RegisteredCalc {
    pub name: String,
    /// 挂在哪个 entity 类型下。
    pub ty: EntityTypeId,
    pub pred: Predicate,
    pub n_groups: u32,
    /// 注册期识别的等值快路（§5 编译）：条件含合取项 `new.path = self` 时，
    /// type scope 无需全实例扇出，直接按 ref 点查订阅者——对应 §4「等值条件→值桶」。
    pub self_eq_path: Option<Vec<String>>,
    pub declared_writes: HashSet<FieldId>,
    pub f: CalcFn,
}

#[derive(Default)]
pub(crate) struct Indexes {
    /// (writer 类型, 字段) → own 订阅（订阅者 = writer 自己）。O(1) 哈希链（§4）。
    pub own: HashMap<(EntityTypeId, FieldId), Vec<(CalcId, u32)>>,
    /// (被盯类型, 字段) → type 订阅（订阅者 = 该 calc 类型的全体实例）。
    pub type_: HashMap<(EntityTypeId, FieldId), Vec<(CalcId, u32)>>,
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
    /// fold 增量状态（§3.4）：runtime 维护，per (谓词, 订阅者实例)。
    fold_state: HashMap<(CalcId, InstanceId), route::FoldAcc>,
    /// 帧 N-1 提交的写集，本帧路由。唯一触发源（§0）。
    pending: Vec<WriteRec>,
    /// calculation 请求的创建，帧边界生效。
    spawn_queue: Vec<(EntityTypeId, Vec<(FieldId, Value)>)>,
    clock: clock::Clock,
    frame: u64,
}

impl Runtime {
    pub fn new() -> Self {
        let mut rt = Runtime {
            store: Store { types: vec![], slots: vec![], free: vec![] },
            calcs: vec![],
            field_owner: HashMap::new(),
            idx: Indexes::default(),
            fold_state: HashMap::new(),
            pending: vec![],
            spawn_queue: vec![],
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
        rt.clock = clock::Clock::new(ty, rt.store.alive_instances(ty)[0], rt.field(ty, "frame"), rt.field(ty, "alarm"));
        rt
    }

    // ---- 注册期 ----

    /// 注册 entity 类型。`_alive` 自动注入为 0 号字段。
    /// singleton 类型立即创建实例 0（注册生效于帧边界的严格化留作 TODO，§5）。
    pub fn register_entity_type(
        &mut self,
        name: &str,
        mut fields: Vec<FieldDef>,
        singleton: bool,
    ) -> EntityTypeId {
        let ty = EntityTypeId(self.store.types.len() as u32);
        fields.insert(0, FieldDef::new("_alive", Value::Bool(false)));
        self.store.types.push(EntityType { name: name.to_string(), fields, singleton });
        self.store.slots.push(vec![]);
        self.store.free.push(vec![]);
        if singleton {
            self.spawn(ty, vec![]);
        }
        ty
    }

    /// 按名查字段 id。
    pub fn field(&self, ty: EntityTypeId, name: &str) -> FieldId {
        let t = &self.store.types[ty.0 as usize];
        let i = t
            .fields
            .iter()
            .position(|f| f.name == name)
            .unwrap_or_else(|| panic!("类型 {} 无字段 {name}", t.name));
        FieldId(i as u32)
    }

    pub fn clock(&self) -> &clock::Clock {
        &self.clock
    }

    /// 定时语义（§6.2）：到点 runtime 写 `Clock.alarm = payload`，订阅者 each 触发。
    pub fn set_alarm(&mut self, at_frame: u64, payload: Value) {
        self.clock.set_alarm(at_frame, payload);
    }

    /// 注册 calculation 及其唯一前置 predicate（单谓词制，§1.4）。
    ///
    /// `declared_writes`：该 calc 的静态写集（D1 检查对象）。`_alive` 豁免
    /// （自决通道；归属是否也应唯一列为开放问题）。
    ///
    /// 注册期编译流水线（§5）：本函数完成 scope 展平/索引挂接、D1 检查、
    /// inst-ref 校验、等值快路识别。TODO：条件归一化、等价谓词合并、
    /// 常量阈值聚簇进共享索引、包含关系消解。
    pub fn register_calculation(
        &mut self,
        name: &str,
        ty: EntityTypeId,
        pred: Predicate,
        declared_writes: &[FieldId],
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
                    self.store.types[ty.0 as usize].name,
                    self.store.types[ty.0 as usize].fields[w.0 as usize].name,
                    self.calcs[prev.0 as usize].name
                ));
            }
        }
        // scope 展平为合取组
        let groups = flatten_scope(&pred.scope)?;
        if groups.len() > 1 && !matches!(pred.delivery, Delivery::Each(_)) {
            return Err("合取 scope 暂仅支持 each 交付（batch/fold 下的合取语义未定，§8）".into());
        }
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
                                self.store.types[ty.0 as usize].fields[ref_field.0 as usize].name
                            ));
                        }
                        self.idx.inst.entry((ty, ref_field, field)).or_default().push((id, gi as u32));
                    }
                    Atom::Type(watched_ty, field) => {
                        check_field(&self.store, watched_ty, field)?;
                        self.idx.type_.entry((watched_ty, field)).or_default().push((id, gi as u32));
                    }
                }
            }
        }
        let self_eq_path = scan_self_eq(&pred.cond);
        self.calcs.push(RegisteredCalc {
            name: name.to_string(),
            ty,
            n_groups: groups.len() as u32,
            self_eq_path,
            declared_writes: declared_writes.iter().copied().collect(),
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
            // 直接走下一帧写集 + 立即结算与帧边界一致：此处简单立即写入并结算
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
        // 阶段一：路由（按 cell 分片可并行，TODO）
        let triggers = route::route(&self.store, &self.idx, &self.calcs, &mut self.fold_state, &w);
        // 阶段二：执行（按实例可并行：D1 + 写局部性保证无竞争，TODO）
        let exec_buf = self.execute(triggers);
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

    fn execute(&mut self, triggers: Vec<Trigger>) -> Vec<WriteRec> {
        let mut folded: Vec<WriteRec> = vec![];
        let mut by_cell: HashMap<CellAddr, usize> = HashMap::new();
        for t in &triggers {
            if !self.store.alive(t.subscriber) {
                continue;
            }
            let rc = &self.calcs[t.calc.0 as usize];
            let mut ctx = Ctx {
                snapshot: &self.store,
                self_id: t.subscriber,
                writes: vec![],
                spawns: vec![],
            };
            (rc.f)(&mut ctx, &t.input);
            let writes = ctx.writes;
            self.spawn_queue.extend(ctx.spawns);
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
                        if folded[i].new != v {
                            // D3 推论一：多次运行写同字段不同值，最终值不确定
                            eprintln!(
                                "[PCE 警告] 帧 {}：{} 多次运行对同字段写入不同值，折叠顺序未定义（§2）",
                                self.frame, rc.name
                            );
                        }
                        folded[i].new = v;
                    }
                    None => {
                        by_cell.insert(key, folded.len());
                        let old = self.store.read(t.subscriber, field);
                        folded.push(WriteRec { inst: t.subscriber, field, old, new: v });
                    }
                }
            }
        }
        folded
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

fn check_field(store: &Store, ty: EntityTypeId, field: FieldId) -> Result<(), String> {
    let t = &store.types[ty.0 as usize];
    if (field.0 as usize) < t.fields.len() {
        Ok(())
    } else {
        Err(format!("类型 {} 无字段 id {}", t.name, field.0))
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

/// 识别条件中的合取项 `new.path = self`（或对称），作为 type scope 的等值快路。
fn scan_self_eq(c: &Cond) -> Option<Vec<String>> {
    match c {
        Cond::Cmp(l, crate::predicate::CmpOp::Eq, r) => match (l, r) {
            (Expr::Val(ValRef::New(p)), Expr::Val(ValRef::SelfRef))
            | (Expr::Val(ValRef::SelfRef), Expr::Val(ValRef::New(p))) => Some(p.clone()),
            _ => None,
        },
        Cond::And(a, b) => scan_self_eq(a).or_else(|| scan_self_eq(b)),
        _ => None,
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
/// （本实现 inst 路由经反向表查询，删表即解除）；id 归还复用。
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
    let nfields = store.types[dead.ty.0 as usize].fields.len();
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
