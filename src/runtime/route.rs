//! 阶段一：路由。持帧 N-1 写集 W：索引查找 → 条件判定 → 填充 each 触发 /
//! batch 缓冲 / 更新 fold（§2）。成本不变量 O(|W|·log + |F|)（§4）。
//!
//! ## 索引绑定（§4 成本表的实现）
//! - own / inst scope → 订阅链哈希：O(1) + 触发数
//! - type scope + 常量等值（含 `became`、`in {…}`）→ 值桶：O(1) + k
//! - type scope + 常量阈值 / `crossed` → 共享排序阈值表：O(log s + k)
//!   （白送优化「路由 SIMD」的数据结构前提：阈值表向量扫描）
//! - type scope + `new.path = self` → ref 点查（值桶的退化形）
//! - 活阈值（条件引用 own 字段）→ 逐订阅者点查：O(该 cell 订阅者数)
//!   ——§4 诚实退化条款
//!
//! ## 等价合并（§5 / 白送优化「kernel fusion」）
//! 同 (cell, 条件) 的常量阈值/等值天然共享同一张索引（一次二分服务全部订阅）；
//! 扫描退化路径上，订阅者无关（sub-independent）的条件按结构等价分类，
//! 每写只求值一次，结果扇出给各 delivery。

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::calculation::{CalcId, Input};
use crate::entity::{EntityTypeId, InstanceId};
use crate::predicate::{CmpOp, Cond, Delivery, Dir, Expr, FoldOp, Proj, ValRef};
use crate::value::Value;

use super::{Determinism, Indexes, RegisteredCalc, Store, Trigger, WriteRec};

// ---- fold 增量状态（§3.4）----

/// sum/count 可逆（±delta，O(1)/写）；min/max 非可逆，用多重集
/// （BTreeMap<全序位键, 计数>）维护，O(log n)/写——成员值回升时正确收缩。
/// 「堆 vs 懒重算」按写读比自适应属 B 层，遥测入口见 [`super::Profile`]。
#[derive(Debug, Clone)]
pub(crate) enum FoldAcc {
    Num(f64),
    Set(BTreeMap<u64, u32>),
}

/// f64 → 可全序比较的位键（单调映射，NaN 排两端）。
fn f2k(f: f64) -> u64 {
    let b = f.to_bits();
    if b >> 63 == 1 { !b } else { b ^ (1 << 63) }
}

fn k2f(k: u64) -> f64 {
    let b = if k >> 63 == 1 { k ^ (1 << 63) } else { !k };
    f64::from_bits(b)
}

pub(crate) fn fold_init(op: FoldOp) -> FoldAcc {
    match op {
        FoldOp::Sum | FoldOp::Count => FoldAcc::Num(0.0),
        FoldOp::Min | FoldOp::Max => FoldAcc::Set(BTreeMap::new()),
    }
}

fn apply_fold(op: FoldOp, st: &mut FoldAcc, w: &WriteRec) {
    let new = w.new.as_f64();
    let old = w.old.as_f64();
    match (op, st) {
        // ±delta：O(1)/写（§4）
        (FoldOp::Sum, FoldAcc::Num(acc)) => {
            *acc += new.unwrap_or(0.0) - old.unwrap_or(0.0)
        }
        (FoldOp::Count, FoldAcc::Num(acc)) => *acc += 1.0,
        (FoldOp::Min | FoldOp::Max, FoldAcc::Set(ms)) => {
            // 撤销该 cell 的上一笔贡献（old 即上一帧提交值），再记入新值。
            // 成员死亡退出的撤销属 §8 开放问题三 / B 层（懒重算时机）。
            if let Some(o) = old {
                if let Some(c) = ms.get_mut(&f2k(o)) {
                    *c -= 1;
                    if *c == 0 {
                        ms.remove(&f2k(o));
                    }
                }
            }
            if let Some(n) = new {
                *ms.entry(f2k(n)).or_insert(0) += 1;
            }
        }
        _ => unreachable!("fold 状态与算子不匹配"),
    }
}

fn fold_value(op: FoldOp, st: &FoldAcc) -> Value {
    match (op, st) {
        (FoldOp::Count, FoldAcc::Num(acc)) => Value::Int(*acc as i64),
        (FoldOp::Sum, FoldAcc::Num(acc)) => Value::Float(*acc),
        (FoldOp::Min, FoldAcc::Set(ms)) => {
            ms.keys().next().map_or(Value::Null, |&k| Value::Float(k2f(k)))
        }
        (FoldOp::Max, FoldAcc::Set(ms)) => {
            ms.keys().next_back().map_or(Value::Null, |&k| Value::Float(k2f(k)))
        }
        _ => unreachable!(),
    }
}

// ---- type scope 索引（§4 成本表）----

/// 可哈希的常量键：值桶只接受这些类型（Float 因判等语义不进桶，走扫描）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum VKey {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    Ref(EntityTypeId, u32, u32),
}

fn vkey(v: &Value) -> Option<VKey> {
    match v {
        Value::Null => Some(VKey::Null),
        Value::Bool(b) => Some(VKey::Bool(*b)),
        Value::Int(i) => Some(VKey::Int(*i)),
        Value::Str(s) => Some(VKey::Str(s.clone())),
        Value::Ref(r) => Some(VKey::Ref(r.ty, r.id, r.generation)),
        _ => None,
    }
}

/// 写入值的探桶键：数值统一落 Int 键（val_eq 语义下 Float(3.0)=Int(3)，
/// 探桶不得有假阴性）。
fn probe_key(v: &Value) -> Option<VKey> {
    match v {
        Value::Float(f) if f.fract() == 0.0 && f.is_finite() => Some(VKey::Int(*f as i64)),
        _ => vkey(v),
    }
}

#[derive(Debug, Clone)]
struct ThEntry {
    t: f64,
    /// 边界取等（Le/Ge）。
    eq_ok: bool,
    calc: CalcId,
    group: u32,
}

/// (被盯类型, 字段) 的 type scope 订阅索引。
/// 各容器是「无假阴性的前滤」：命中后仍求值完整条件（O(1)）。
#[derive(Default)]
pub(crate) struct TypeIndex {
    /// 等值快路：条件含合取项 `new.path = self`——按 ref 直接点查订阅者。
    self_eq: Vec<(Vec<String>, CalcId, u32)>,
    /// 值桶：(投影路径, 常量) → 订阅。O(1) + k。
    buckets: HashMap<(Vec<String>, VKey), Vec<(CalcId, u32)>>,
    /// 桶里出现过的去重路径（每写逐路径探一次桶）。
    bucket_paths: Vec<Vec<String>>,
    /// 共享排序阈值表：fires when new < t（或 ≤）。后缀范围查询 O(log s + k)。
    lt: Vec<ThEntry>,
    /// fires when new > t（或 ≥）。前缀范围查询。
    gt: Vec<ThEntry>,
    /// crossed(t, ↓)：old ≥ t ∧ new < t ⇔ t ∈ (new, old]。
    cross_down: Vec<ThEntry>,
    /// crossed(t, ↑)：old ≤ t ∧ new > t ⇔ t ∈ [old, new)。
    cross_up: Vec<ThEntry>,
    /// 退化路径：活阈值 / 复合条件——逐订阅者（§4 诚实退化条款）。
    scan: Vec<(CalcId, u32)>,
}

fn sorted_insert(v: &mut Vec<ThEntry>, e: ThEntry) {
    let i = v.partition_point(|x| x.t < e.t);
    v.insert(i, e);
}

impl TypeIndex {
    /// 注册期编译（§5）：把条件的某个合取项绑定到最优索引类。
    /// 取顶层合取链（`and` / `and not` 的左支）逐项试绑，绑不上落 scan。
    pub(crate) fn insert(&mut self, cond: &Cond, calc: CalcId, group: u32) {
        let mut conj = vec![];
        collect_conjuncts(cond, &mut conj);
        for c in &conj {
            match c {
                // new.path = self → ref 点查
                Cond::Cmp(Expr::Val(ValRef::New(p)), CmpOp::Eq, Expr::Val(ValRef::SelfRef))
                | Cond::Cmp(Expr::Val(ValRef::SelfRef), CmpOp::Eq, Expr::Val(ValRef::New(p))) => {
                    self.self_eq.push((p.clone(), calc, group));
                    return;
                }
                // new.path = 常量 → 值桶
                Cond::Cmp(Expr::Val(ValRef::New(p)), CmpOp::Eq, Expr::Val(ValRef::Const(v)))
                | Cond::Cmp(Expr::Val(ValRef::Const(v)), CmpOp::Eq, Expr::Val(ValRef::New(p))) => {
                    if let Some(k) = vkey(v) {
                        self.add_bucket(p.clone(), k, calc, group);
                        return;
                    }
                }
                Cond::Became(v) => {
                    if let Some(k) = vkey(v) {
                        self.add_bucket(vec![], k, calc, group);
                        return;
                    }
                }
                Cond::InSet(vs) => {
                    if let Some(ks) = vs.iter().map(vkey).collect::<Option<Vec<_>>>() {
                        for k in ks {
                            self.add_bucket(vec![], k, calc, group);
                        }
                        return;
                    }
                }
                // new ⋛ 常量 → 共享排序阈值表
                Cond::Cmp(Expr::Val(ValRef::New(p)), op, Expr::Val(ValRef::Const(v)))
                    if p.is_empty() =>
                {
                    if let Some(t) = v.as_f64() {
                        if self.add_threshold(*op, t, calc, group) {
                            return;
                        }
                    }
                }
                Cond::Cmp(Expr::Val(ValRef::Const(v)), op, Expr::Val(ValRef::New(p)))
                    if p.is_empty() =>
                {
                    if let (Some(t), Some(m)) = (v.as_f64(), mirror(*op)) {
                        if self.add_threshold(m, t, calc, group) {
                            return;
                        }
                    }
                }
                // crossed(常量, dir) → 区间查询
                Cond::Crossed(Expr::Val(ValRef::Const(v)), dir) => {
                    if let Some(t) = v.as_f64() {
                        let e = ThEntry { t, eq_ok: false, calc, group };
                        match dir {
                            Dir::Down => sorted_insert(&mut self.cross_down, e),
                            Dir::Up => sorted_insert(&mut self.cross_up, e),
                        }
                        return;
                    }
                }
                _ => {}
            }
        }
        self.scan.push((calc, group));
    }

    fn add_bucket(&mut self, path: Vec<String>, k: VKey, calc: CalcId, group: u32) {
        if !self.bucket_paths.contains(&path) {
            self.bucket_paths.push(path.clone());
        }
        self.buckets.entry((path, k)).or_default().push((calc, group));
    }

    fn add_threshold(&mut self, op: CmpOp, t: f64, calc: CalcId, group: u32) -> bool {
        match op {
            CmpOp::Lt => sorted_insert(&mut self.lt, ThEntry { t, eq_ok: false, calc, group }),
            CmpOp::Le => sorted_insert(&mut self.lt, ThEntry { t, eq_ok: true, calc, group }),
            CmpOp::Gt => sorted_insert(&mut self.gt, ThEntry { t, eq_ok: false, calc, group }),
            CmpOp::Ge => sorted_insert(&mut self.gt, ThEntry { t, eq_ok: true, calc, group }),
            _ => return false,
        }
        true
    }

    /// 路由一条写：把命中的 (calc, group, 候选订阅者) 交给 `hit`。
    /// 候选订阅者 None = 「该 calc 类型的全体存活实例」（由调用方扇出）。
    fn probe(
        &self,
        w: &WriteRec,
        calcs: &[RegisteredCalc],
        mut hit: impl FnMut(CalcId, u32, Option<InstanceId>),
    ) {
        for (path, c, g) in &self.self_eq {
            if let Some(target) = w.new.get_path(path).as_ref_id() {
                if target.ty == calcs[c.0 as usize].ty {
                    hit(*c, *g, Some(target));
                }
            }
        }
        for path in &self.bucket_paths {
            let v = w.new.get_path(path);
            if let Some(k) = probe_key(&v) {
                if let Some(subs) = self.buckets.get(&(path.clone(), k)) {
                    for &(c, g) in subs {
                        hit(c, g, None);
                    }
                }
            }
        }
        let (new, old) = (w.new.as_f64(), w.old.as_f64());
        if let Some(n) = new {
            // lt 表：fires when n < t（或 ≤）——后缀 [partition_point ..)
            let i = self.lt.partition_point(|e| e.t < n);
            for e in &self.lt[i..] {
                if e.t > n || e.eq_ok {
                    hit(e.calc, e.group, None);
                }
            }
            // gt 表：fires when n > t（或 ≥）——前缀 [.. partition_point)
            let j = self.gt.partition_point(|e| e.t <= n);
            for e in &self.gt[..j] {
                if e.t < n || e.eq_ok {
                    hit(e.calc, e.group, None);
                }
            }
        }
        if let (Some(n), Some(o)) = (new, old) {
            if n < o {
                // ↓：t ∈ (n, o]
                let a = self.cross_down.partition_point(|e| e.t <= n);
                let b = self.cross_down.partition_point(|e| e.t <= o);
                for e in &self.cross_down[a..b] {
                    hit(e.calc, e.group, None);
                }
            } else if n > o {
                // ↑：t ∈ [o, n)
                let a = self.cross_up.partition_point(|e| e.t < o);
                let b = self.cross_up.partition_point(|e| e.t < n);
                for e in &self.cross_up[a..b] {
                    hit(e.calc, e.group, None);
                }
            }
        }
        for &(c, g) in &self.scan {
            hit(c, g, None);
        }
    }
}

fn mirror(op: CmpOp) -> Option<CmpOp> {
    Some(match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
        _ => return None,
    })
}

fn collect_conjuncts<'a>(c: &'a Cond, out: &mut Vec<&'a Cond>) {
    match c {
        Cond::And(a, b) => {
            collect_conjuncts(a, out);
            collect_conjuncts(b, out);
        }
        // and not 的左支必须成立，是合法前滤来源
        Cond::AndNot(a, _) => collect_conjuncts(a, out),
        _ => out.push(c),
    }
}

/// 条件是否不引用订阅者（own 字段 / self）——是则每写只需求值一次（等价合并）。
pub(crate) fn cond_sub_independent(c: &Cond) -> bool {
    fn expr_ok(e: &Expr) -> bool {
        match e {
            Expr::Val(v) => !matches!(v, ValRef::Own(_) | ValRef::SelfRef),
            Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) | Expr::Div(a, b) => {
                expr_ok(a) && expr_ok(b)
            }
        }
    }
    match c {
        Cond::True | Cond::InRange(..) | Cond::InSet(_) | Cond::Changed | Cond::Became(_) => true,
        Cond::Cmp(a, _, b) => expr_ok(a) && expr_ok(b),
        Cond::Crossed(t, _) => expr_ok(t),
        Cond::And(a, b) | Cond::Or(a, b) | Cond::AndNot(a, b) => {
            cond_sub_independent(a) && cond_sub_independent(b)
        }
    }
}

// ---- 帧 scratch（白送优化「帧 arena 分配」的工程形：缓冲跨帧复用）----

/// 路由期一切临时结构帧生命周期：clear 不还容量，下一帧零分配复用。
#[derive(Default)]
pub(crate) struct Scratch {
    /// batch 私有帧缓冲。行携带规范序键（C4 Canonical 时排序，Free 时忽略）。
    batch_buf: HashMap<(CalcId, InstanceId), Vec<(BatchKey, Vec<Value>)>>,
    batch_order: Vec<(CalcId, InstanceId)>,
    /// `&` 合取闩（§4）：每 (谓词, 订阅者) 每帧位码；最高位 = 本帧已触发。
    latch: HashMap<(CalcId, InstanceId), u32>,
    fold_hits: Vec<(CalcId, InstanceId)>,
    fold_seen: HashSet<(CalcId, InstanceId)>,
    /// 等价合并 memo：sub-independent 条件按等价类每写求值一次。
    cond_memo: Vec<(u32, bool)>,
}

type BatchKey = (u32, u32, u32, u32);

impl Scratch {
    fn clear(&mut self) {
        self.batch_buf.clear();
        self.batch_order.clear();
        self.latch.clear();
        self.fold_hits.clear();
        self.fold_seen.clear();
        self.cond_memo.clear();
    }
}

struct EvalCtx<'a> {
    store: &'a Store,
    /// 订阅者实例（own 字段点查、self 常量的对象）。
    sub: InstanceId,
    new: &'a Value,
    old: &'a Value,
}

pub(crate) fn route(
    store: &Store,
    idx: &Indexes,
    calcs: &[RegisteredCalc],
    fold_state: &mut HashMap<(CalcId, InstanceId), FoldAcc>,
    scratch: &mut Scratch,
    writes: &[WriteRec],
    determinism: Determinism,
) -> Vec<Trigger> {
    scratch.clear();
    let mut triggers: Vec<Trigger> = vec![];

    for w in writes {
        scratch.cond_memo.clear();
        // 候选收集：(calc, group, 已定订阅者 / None=按类型扇出)
        let mut hits: Vec<(CalcId, u32, Option<InstanceId>)> = vec![];
        // own：订阅者 = writer 自己。哈希链 O(1)+触发数
        if let Some(subs) = idx.own.get(&(w.inst.ty, w.field)) {
            for &(c, g) in subs {
                hits.push((c, g, Some(w.inst)));
            }
        }
        // type：值桶 / 阈值表 / self_eq 点查 / 扫描退化
        if let Some(ti) = idx.type_.get(&(w.inst.ty, w.field)) {
            ti.probe(w, calcs, |c, g, sub| hits.push((c, g, sub)));
        }
        // inst：经 ref 反向表点查持有者，再查 (持有者类型, ref 字段, 被盯字段) 索引
        if let Some(holders) = idx.ref_reverse.get(&w.inst) {
            for &(holder, rf) in holders {
                if let Some(subs) = idx.inst.get(&(holder.ty, rf, w.field)) {
                    for &(c, g) in subs {
                        hits.push((c, g, Some(holder)));
                    }
                }
            }
        }

        for (c, group, sub) in hits {
            let rc = &calcs[c.0 as usize];
            match sub {
                Some(sub) => {
                    if !store.alive(sub) {
                        continue;
                    }
                    let ectx = EvalCtx { store, sub, new: &w.new, old: &w.old };
                    if eval_cond(&rc.pred.cond, &ectx) {
                        deliver(rc, c, group, sub, w, store, fold_state, scratch, &mut triggers);
                    }
                }
                None => {
                    // 类型扇出。sub-independent 条件：等价类内每写求值一次（§5 等价合并）
                    if rc.cond_indep {
                        let v = match scratch
                            .cond_memo
                            .iter()
                            .find(|(cls, _)| *cls == rc.cond_class)
                        {
                            Some(&(_, v)) => v,
                            None => {
                                let ectx =
                                    EvalCtx { store, sub: w.inst, new: &w.new, old: &w.old };
                                let v = eval_cond(&rc.pred.cond, &ectx);
                                scratch.cond_memo.push((rc.cond_class, v));
                                v
                            }
                        };
                        if !v {
                            continue;
                        }
                        store.for_each_alive(rc.ty, |sub| {
                            deliver(rc, c, group, sub, w, store, fold_state, scratch, &mut triggers);
                        });
                    } else {
                        // 活阈值：逐订阅者点查（§4 诚实退化条款）
                        store.for_each_alive(rc.ty, |sub| {
                            let ectx = EvalCtx { store, sub, new: &w.new, old: &w.old };
                            if eval_cond(&rc.pred.cond, &ectx) {
                                deliver(
                                    rc, c, group, sub, w, store, fold_state, scratch,
                                    &mut triggers,
                                );
                            }
                        });
                    }
                }
            }
        }
    }

    // batch：整帧聚为一批，一次交付；顺序未定义（D3）。
    // C4 Canonical 档：买回确定性——按 (writer, field) 规范序排序后交付。
    for key in scratch.batch_order.drain(..) {
        let mut rows = scratch.batch_buf.remove(&key).unwrap();
        if determinism == Determinism::Canonical {
            rows.sort_by_key(|(k, _)| *k);
        }
        triggers.push(Trigger {
            calc: key.0,
            subscriber: key.1,
            input: Input::Batch(rows.into_iter().map(|(_, r)| r).collect()),
        });
    }
    // fold：本帧有更新的聚合交付一次
    for &(c, sub) in &scratch.fold_hits {
        let Delivery::Fold(op) = calcs[c.0 as usize].pred.delivery else { unreachable!() };
        let v = fold_value(op, &fold_state[&(c, sub)]);
        triggers.push(Trigger { calc: c, subscriber: sub, input: Input::Fold(v) });
    }
    triggers
}

/// 条件已判真：按 delivery 落触发 / batch 缓冲 / fold 更新（含合取闩）。
#[allow(clippy::too_many_arguments)]
fn deliver(
    rc: &RegisteredCalc,
    c: CalcId,
    group: u32,
    sub: InstanceId,
    w: &WriteRec,
    store: &Store,
    fold_state: &mut HashMap<(CalcId, InstanceId), FoldAcc>,
    scratch: &mut Scratch,
    triggers: &mut Vec<Trigger>,
) {
    // 合取闩（§4「& 合取」行）：组数 >1 时须同帧集齐全部组才触发（一次/帧）。
    // 诚实条款：`&` 要求帧完整写集，是路由内部唯一的流水化屏障。
    if rc.n_groups > 1 {
        let bits = scratch.latch.entry((c, sub)).or_insert(0);
        *bits |= 1 << group;
        let full = (1u32 << rc.n_groups) - 1;
        let fired = 1u32 << 31;
        if *bits & full != full || *bits & fired != 0 {
            return;
        }
        *bits |= fired;
    }
    match &rc.pred.delivery {
        Delivery::Each(projs) => {
            triggers.push(Trigger {
                calc: c,
                subscriber: sub,
                input: Input::Each(project(projs, w, sub, store)),
            });
        }
        Delivery::Batch(projs) => {
            let key = (c, sub);
            let buf = scratch.batch_buf.entry(key).or_insert_with(|| {
                scratch.batch_order.push(key);
                vec![]
            });
            let okey = (w.inst.ty.0, w.inst.id, w.inst.generation, w.field.0);
            buf.push((okey, project(projs, w, sub, store)));
        }
        Delivery::Fold(op) => {
            let st = fold_state.entry((c, sub)).or_insert_with(|| fold_init(*op));
            apply_fold(*op, st, w);
            if scratch.fold_seen.insert((c, sub)) {
                scratch.fold_hits.push((c, sub));
            }
        }
    }
}

/// 投影（§3.4）：交付一律是值快照，不是引用——引用不跨实体，值经由谓词通道流动。
pub(crate) fn project(projs: &[Proj], w: &WriteRec, sub: InstanceId, store: &Store) -> Vec<Value> {
    projs
        .iter()
        .map(|p| match p {
            Proj::New(path) => w.new.get_path(path),
            Proj::Old(path) => w.old.get_path(path),
            Proj::WriterId => Value::Ref(w.inst),
            Proj::Own(f) => store.read(sub, *f),
        })
        .collect()
}

// ---- 条件求值（§3.3 封闭集：new / old / own 字段 / 常量含 self）----

fn eval_val(v: &ValRef, c: &EvalCtx) -> Value {
    match v {
        ValRef::New(path) => c.new.get_path(path),
        ValRef::Old(path) => c.old.get_path(path),
        ValRef::Own(f) => c.store.read(c.sub, *f),
        ValRef::Const(v) => v.clone(),
        ValRef::SelfRef => Value::Ref(c.sub),
    }
}

fn eval_expr(e: &Expr, c: &EvalCtx) -> Value {
    match e {
        Expr::Val(v) => eval_val(v, c),
        Expr::Add(a, b) => arith(a, b, c, |x, y| x + y),
        Expr::Sub(a, b) => arith(a, b, c, |x, y| x - y),
        Expr::Mul(a, b) => arith(a, b, c, |x, y| x * y),
        Expr::Div(a, b) => arith(a, b, c, |x, y| x / y),
    }
}

fn arith(a: &Expr, b: &Expr, c: &EvalCtx, f: fn(f64, f64) -> f64) -> Value {
    match (eval_expr(a, c).as_f64(), eval_expr(b, c).as_f64()) {
        (Some(x), Some(y)) => Value::Float(f(x, y)),
        _ => Value::Null,
    }
}

/// 数值感知判等：Int(3) 与 Float(3.0) 相等；其余按结构判等。
fn val_eq(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

fn eval_cond(cond: &Cond, c: &EvalCtx) -> bool {
    match cond {
        Cond::True => true,
        Cond::Cmp(l, op, r) => {
            let (lv, rv) = (eval_expr(l, c), eval_expr(r, c));
            match op {
                CmpOp::Eq => val_eq(&lv, &rv),
                CmpOp::Ne => !val_eq(&lv, &rv),
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
        Cond::InRange(a, b) => c.new.as_f64().is_some_and(|v| v >= *a && v <= *b),
        Cond::InSet(vs) => vs.iter().any(|v| val_eq(v, c.new)),
        // D2 写即事件；「值真的变了」显式问
        Cond::Changed => !val_eq(c.new, c.old),
        Cond::Became(v) => val_eq(c.new, v) && !val_eq(c.old, v),
        // 边沿穿越（双缓冲旧值免费，O(1)，§4）
        Cond::Crossed(t, dir) => {
            let (Some(t), Some(old), Some(new)) =
                (eval_expr(t, c).as_f64(), c.old.as_f64(), c.new.as_f64())
            else {
                return false;
            };
            match dir {
                Dir::Down => old >= t && new < t,
                Dir::Up => old <= t && new > t,
            }
        }
        Cond::And(a, b) => eval_cond(a, c) && eval_cond(b, c),
        Cond::Or(a, b) => eval_cond(a, c) || eval_cond(b, c),
        // 否定仅作守卫（§3.3）：仍需正触发源，稀疏性不破
        Cond::AndNot(a, b) => eval_cond(a, c) && !eval_cond(b, c),
    }
}
