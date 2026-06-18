//! 原始 API 形态说明（教学参考，不参与库编译或导出）。
//!
//! 把 §7 的谓词 DSL 直译成 Rust：字段按名引用、注册期统一解析校验；
//! 表达式用运算符重载（`own_("hp_max") * 0.3`）；档位（C 层）是 builder
//! 上的方法。纯库 crate 只暴露底层 AST（`predicate`）与 runtime 注册 API；
//! 本文件仅作为“原始 API 形态”说明书，帮助教学与讨论，不纳入 `src/`。
//!
//! ```no_run
//! use pce::prelude::*;
//!
//! let mut rt = Runtime::new();
//! let unit = rt.entity("Unit")
//!     .field("hp", 100)
//!     .field("hp_max", 100)
//!     .field("state", "idle")
//!     .done();
//!
//! // §7 示例 1：血量跌穿 30% —— 边沿触发，不会每帧重复
//! let f_state = rt.field(unit, "state");
//! rt.calc("flee", unit)
//!     .on(own("hp"))
//!     .when(crossed_down(own_("hp_max") * 0.3))
//!     .each([newv(), oldv()])
//!     .writes(["state"])
//!     .body(move |ctx, _| ctx.write(f_state, "fleeing"))
//!     .unwrap();
//! ```

use std::ops::{Add, BitAnd, BitOr, Div, Mul, Sub};

use crate::calculation::{CalcId, Ctx, Input};
use crate::entity::{EntityTypeId, FieldDef, FieldId};
use crate::predicate::{CmpOp, Cond, Delivery, Dir, Expr, FoldOp, Predicate, Proj, Scope, ValRef};
use crate::runtime::{CalcOptions, Residency, RowPolicy, Runtime, Tier};
use crate::value::Value;

// ---- scope（§3.2）----

/// 名字形式的 scope，注册时解析为 [`Scope`]。
#[derive(Debug, Clone)]
pub enum S {
    /// 自己实例的某个 cell。
    Own(String),
    /// 经由自己持有的 ref 字段，盯一个特定实例的 cell。
    Via(String, String),
    /// 某 entity 类型的全体实例（通配 id）。
    All(EntityTypeId, String),
    /// `type(Clock, frame)` 的糖——配合恒真条件即经典 ECS system，
    /// 注册期识别后走稠密遍历快路（白送优化）。
    EveryFrame,
    /// 并：任一来源有写入即触发。
    Or(Box<S>, Box<S>),
    /// 合取：同帧都有写入才触发。
    And(Box<S>, Box<S>),
}

/// 自己实例的某个 cell。
pub fn own(field: &str) -> S {
    S::Own(field.into())
}

/// 经由自己持有的 ref 字段 `ref_field`，盯目标实例的 `field`。
pub fn via(ref_field: &str, field: &str) -> S {
    S::Via(ref_field.into(), field.into())
}

/// 某 entity 类型全体实例的 `field`（通配 id）。
pub fn all(ty: EntityTypeId, field: &str) -> S {
    S::All(ty, field.into())
}

/// 每帧触发（订阅 `Clock.frame`——显式轮询，代价可见、自付，§6.2）。
/// 恒真条件 + each 时即 ECS 快路。
pub fn every_frame() -> S {
    S::EveryFrame
}

impl BitOr for S {
    type Output = S;
    fn bitor(self, rhs: S) -> S {
        S::Or(Box::new(self), Box::new(rhs))
    }
}

impl BitAnd for S {
    type Output = S;
    fn bitand(self, rhs: S) -> S {
        S::And(Box::new(self), Box::new(rhs))
    }
}

// ---- 表达式 / 投影（§3.3、§3.4）----

/// 条件操作数 / 投影项。注册时按上下文解析为 [`Expr`] 或 [`Proj`]。
#[derive(Debug, Clone)]
pub struct E(Ei);

#[derive(Debug, Clone)]
enum Ei {
    New(Vec<String>),
    Old(Vec<String>),
    OwnF(String),
    Const(Value),
    SelfRef,
    /// 仅投影侧合法：writer 实例 id 作为 ref 值交付。
    Writer,
    Add(Box<Ei>, Box<Ei>),
    Sub(Box<Ei>, Box<Ei>),
    Mul(Box<Ei>, Box<Ei>),
    Div(Box<Ei>, Box<Ei>),
}

fn path(p: &str) -> Vec<String> {
    if p.is_empty() { vec![] } else { p.split('.').map(str::to_string).collect() }
}

/// 本次写入值（整体）。
pub fn newv() -> E {
    E(Ei::New(vec![]))
}

/// 本次写入值的字段路径（结构化 cell，如 `new_("target")`、`new_("a.b")`）。
pub fn new_(p: &str) -> E {
    E(Ei::New(path(p)))
}

/// 该 cell 上一帧值（双缓冲免费）。
pub fn oldv() -> E {
    E(Ei::Old(vec![]))
}

/// 上一帧值的字段路径。
pub fn old_(p: &str) -> E {
    E(Ei::Old(path(p)))
}

/// 订阅者自己实例的字段（活阈值；代价见 §4 诚实退化条款）。
pub fn own_(field: &str) -> E {
    E(Ei::OwnF(field.into()))
}

/// 自身实例引用（常量；典型用法 `new_("target").eq(self_())`）。
pub fn self_() -> E {
    E(Ei::SelfRef)
}

/// writer 的实例 id（仅投影侧：`each([writer(), new_("dmg")])`）。
pub fn writer() -> E {
    E(Ei::Writer)
}

/// 常量。多数场合可省略——字面量自动转换（`own_("hp") * 0.3`、`.eq(5)`）。
pub fn val(v: impl Into<Value>) -> E {
    E(Ei::Const(v.into()))
}

macro_rules! e_from {
    ($($t:ty),*) => {$(
        impl From<$t> for E {
            fn from(v: $t) -> E { E(Ei::Const(v.into())) }
        }
    )*};
}
e_from!(bool, i32, i64, f64, &str, String, Value, crate::entity::InstanceId);

macro_rules! e_op {
    ($tr:ident, $m:ident, $v:ident) => {
        impl<T: Into<E>> $tr<T> for E {
            type Output = E;
            fn $m(self, rhs: T) -> E {
                E(Ei::$v(Box::new(self.0), Box::new(rhs.into().0)))
            }
        }
    };
}
e_op!(Add, add, Add);
e_op!(Sub, sub, Sub);
e_op!(Mul, mul, Mul);
e_op!(Div, div, Div);

// ---- 条件（§3.3）----

/// 名字形式的条件，注册时解析为 [`Cond`]。
#[derive(Debug, Clone)]
pub struct C(Ci);

#[derive(Debug, Clone)]
enum Ci {
    True,
    Cmp(E, CmpOp, E),
    InRange(f64, f64),
    InSet(Vec<Value>),
    Changed,
    Became(Value),
    Crossed(E, Dir),
    And(Box<Ci>, Box<Ci>),
    Or(Box<Ci>, Box<Ci>),
    AndNot(Box<Ci>, Box<Ci>),
}

impl E {
    pub fn eq(self, rhs: impl Into<E>) -> C {
        C(Ci::Cmp(self, CmpOp::Eq, rhs.into()))
    }
    pub fn ne(self, rhs: impl Into<E>) -> C {
        C(Ci::Cmp(self, CmpOp::Ne, rhs.into()))
    }
    pub fn lt(self, rhs: impl Into<E>) -> C {
        C(Ci::Cmp(self, CmpOp::Lt, rhs.into()))
    }
    pub fn le(self, rhs: impl Into<E>) -> C {
        C(Ci::Cmp(self, CmpOp::Le, rhs.into()))
    }
    pub fn gt(self, rhs: impl Into<E>) -> C {
        C(Ci::Cmp(self, CmpOp::Gt, rhs.into()))
    }
    pub fn ge(self, rhs: impl Into<E>) -> C {
        C(Ci::Cmp(self, CmpOp::Ge, rhs.into()))
    }
}

/// new ≠ old（与 D2 配合：写即事件，「值真的变了」显式问）。
pub fn changed() -> C {
    C(Ci::Changed)
}

/// old ≠ v ∧ new = v（典型：`became(())` 收尸 ref 置 null、`became(true)` 感知出生）。
pub fn became(v: impl Into<Value>) -> C {
    C(Ci::Became(v.into()))
}

/// 边沿下穿：old ≥ t ∧ new < t。阈值可为活表达式（`own_("hp_max") * 0.3`）。
pub fn crossed_down(t: impl Into<E>) -> C {
    C(Ci::Crossed(t.into(), Dir::Down))
}

/// 边沿上穿：old ≤ t ∧ new > t。
pub fn crossed_up(t: impl Into<E>) -> C {
    C(Ci::Crossed(t.into(), Dir::Up))
}

/// new ∈ [a, b]（闭区间，数值）。
pub fn in_range(a: f64, b: f64) -> C {
    C(Ci::InRange(a, b))
}

/// new ∈ {…}（等值集合 → 值桶索引）。
pub fn one_of<V: Into<Value>>(vs: impl IntoIterator<Item = V>) -> C {
    C(Ci::InSet(vs.into_iter().map(Into::into).collect()))
}

impl BitAnd for C {
    type Output = C;
    fn bitand(self, rhs: C) -> C {
        C(Ci::And(Box::new(self.0), Box::new(rhs.0)))
    }
}

impl BitOr for C {
    type Output = C;
    fn bitor(self, rhs: C) -> C {
        C(Ci::Or(Box::new(self.0), Box::new(rhs.0)))
    }
}

impl C {
    /// 否定仅作守卫（§3.3）：仍需正触发源，稀疏性不破。
    pub fn and_not(self, rhs: C) -> C {
        C(Ci::AndNot(Box::new(self.0), Box::new(rhs.0)))
    }
}

// ---- entity builder ----

/// `rt.entity("Unit").field("hp", 100).done()`。
pub struct EntityBuilder<'a> {
    rt: &'a mut Runtime,
    name: String,
    fields: Vec<FieldDef>,
    singleton: bool,
    policy: RowPolicy,
}

impl<'a> EntityBuilder<'a> {
    pub fn field(mut self, name: &str, default: impl Into<Value>) -> Self {
        self.fields.push(FieldDef { name: name.into(), is_ref: false, default: default.into() });
        self
    }

    /// ref 类型字段：runtime 维护反向表，目标销毁时被结算写成 null（§6.3）。
    pub fn ref_field(mut self, name: &str) -> Self {
        self.fields.push(FieldDef::reference(name));
        self
    }

    /// 单实例类型（全局状态的唯一表达，§1.2）。注册即创建实例 0。
    pub fn singleton(mut self) -> Self {
        self.singleton = true;
        self
    }

    /// C6 行身份策略：压缩行（行恒稠密，死亡 swap-remove 重映射）。
    /// 默认稳定行（死亡留洞）。按 churn 特征选，可看 [`Runtime::profile`] 遥测。
    pub fn compact(mut self) -> Self {
        self.policy = RowPolicy::Compact;
        self
    }

    pub fn done(self) -> EntityTypeId {
        self.rt.register_entity_type_with(&self.name, self.fields, self.singleton, self.policy)
    }
}

// ---- calculation builder ----

enum Dv {
    Each(Vec<E>),
    Batch(Vec<E>),
    Fold(FoldOp),
}

/// `rt.calc("flee", unit).on(...).when(...).each([...]).writes([...]).body(...)`。
///
/// 单谓词制（§1.4）：一个 calculation 恰有一个前置 predicate；
/// `when` 缺省为恒真（写即触发，D2），交付缺省为 `each([])`。
pub struct CalcBuilder<'a> {
    rt: &'a mut Runtime,
    name: String,
    ty: EntityTypeId,
    scope: Option<S>,
    cond: C,
    delivery: Dv,
    writes: Vec<String>,
    reads: Option<Vec<String>>,
    tier: Tier,
    residency: Residency,
}

impl<'a> CalcBuilder<'a> {
    /// 嗅探范围（必填）。
    pub fn on(mut self, scope: S) -> Self {
        self.scope = Some(scope);
        self
    }

    /// 值条件（缺省恒真：写即触发，D2）。
    pub fn when(mut self, cond: C) -> Self {
        self.cond = cond;
        self
    }

    /// 每条命中触发一次。投影项见 [`newv`]/[`oldv`]/[`writer`]/[`own_`]。
    /// D3 推论一：一帧内可能运行多次；禁止用 each 做读-改-写累加。
    pub fn each<const N: usize>(mut self, projs: [E; N]) -> Self {
        self.delivery = Dv::Each(projs.into());
        self
    }

    /// 整帧聚为一批，一次交付；顺序未定义（D3，Canonical 档买回，C4）。
    pub fn batch<const N: usize>(mut self, projs: [E; N]) -> Self {
        self.delivery = Dv::Batch(projs.into());
        self
    }

    /// 增量聚合（本层最重要的下推形式）：±delta，O(1)/写。
    pub fn fold_sum(mut self) -> Self {
        self.delivery = Dv::Fold(FoldOp::Sum);
        self
    }

    pub fn fold_count(mut self) -> Self {
        self.delivery = Dv::Fold(FoldOp::Count);
        self
    }

    pub fn fold_min(mut self) -> Self {
        self.delivery = Dv::Fold(FoldOp::Min);
        self
    }

    pub fn fold_max(mut self) -> Self {
        self.delivery = Dv::Fold(FoldOp::Max);
        self
    }

    /// 静态写集声明（D1 检查对象；必填，注册期归属检查）。
    pub fn writes<const N: usize>(mut self, fields: [&str; N]) -> Self {
        self.writes = fields.iter().map(|s| s.to_string()).collect();
        self
    }

    /// C2 读集声明：换热冷分离与预取精度；非 Silent 档下越界读会被检测。
    pub fn reads<const N: usize>(mut self, fields: [&str; N]) -> Self {
        self.reads = Some(fields.iter().map(|s| s.to_string()).collect());
        self
    }

    /// C1 执行档位（`Tier::Kernel`：受限子集，禁动态分配，可进向量化调度）。
    pub fn tier(mut self, t: Tier) -> Self {
        self.tier = t;
        self
    }

    /// C3 驻留 pin（无静态正解；配合 profile + 滞回）。
    pub fn residency(mut self, r: Residency) -> Self {
        self.residency = r;
        self
    }

    /// 提交注册（注册期编译 §5：解析、校验、索引绑定、D1 检查一次完成）。
    pub fn body(
        self,
        f: impl Fn(&mut Ctx, &Input) + Send + Sync + 'static,
    ) -> Result<CalcId, String> {
        let ty = self.ty;
        let scope = self
            .scope
            .ok_or_else(|| format!("calculation {}：缺 scope（.on(...)）", self.name))?;
        let scope = resolve_scope(self.rt, ty, &scope)?;
        let cond = resolve_cond(self.rt, ty, &self.cond.0)?;
        let delivery = match &self.delivery {
            Dv::Each(ps) => Delivery::Each(resolve_projs(self.rt, ty, ps)?),
            Dv::Batch(ps) => Delivery::Batch(resolve_projs(self.rt, ty, ps)?),
            Dv::Fold(op) => Delivery::Fold(*op),
        };
        let writes = self
            .writes
            .iter()
            .map(|n| self.rt.try_field(ty, n))
            .collect::<Result<Vec<_>, _>>()?;
        let reads = match &self.reads {
            Some(ns) => {
                Some(ns.iter().map(|n| self.rt.try_field(ty, n)).collect::<Result<Vec<_>, _>>()?)
            }
            None => None,
        };
        let opts = CalcOptions { reads, tier: self.tier, residency: self.residency };
        self.rt.register_calculation_opt(
            &self.name,
            ty,
            Predicate::new(scope, cond, delivery),
            &writes,
            opts,
            Box::new(f),
        )
    }
}

impl Runtime {
    /// 声明 entity 类型（builder）。
    pub fn entity(&mut self, name: &str) -> EntityBuilder<'_> {
        EntityBuilder {
            rt: self,
            name: name.into(),
            fields: vec![],
            singleton: false,
            policy: RowPolicy::default(),
        }
    }

    /// 声明 calculation 及其前置 predicate（builder）。
    pub fn calc(&mut self, name: &str, ty: EntityTypeId) -> CalcBuilder<'_> {
        CalcBuilder {
            rt: self,
            name: name.into(),
            ty,
            scope: None,
            cond: C(Ci::True),
            delivery: Dv::Each(vec![]),
            writes: vec![],
            reads: None,
            tier: Tier::default(),
            residency: Residency::default(),
        }
    }
}

// ---- 注册期解析 ----

fn resolve_scope(rt: &Runtime, ty: EntityTypeId, s: &S) -> Result<Scope, String> {
    Ok(match s {
        S::Own(f) => Scope::Own(rt.try_field(ty, f)?),
        S::Via(rf, f) => {
            let ref_field = rt.try_field(ty, rf)?;
            // 被盯字段属于 ref 目标类型,注册期未知目标类型——按字段名跨类型解析
            // 不可行；inst scope 的字段 id 语义是「目标实例的字段」。约定：
            // 名字在持有者类型上解析仅当同名；通用场景用 via_id。
            Scope::Inst { ref_field, field: resolve_target_field(rt, ty, rf, f)? }
        }
        S::All(wty, f) => Scope::Type(*wty, rt.try_field(*wty, f)?),
        S::EveryFrame => {
            let c = rt.clock();
            Scope::Type(c.ty, c.f_frame)
        }
        S::Or(a, b) => Scope::Or(
            Box::new(resolve_scope(rt, ty, a)?),
            Box::new(resolve_scope(rt, ty, b)?),
        ),
        S::And(a, b) => Scope::And(
            Box::new(resolve_scope(rt, ty, a)?),
            Box::new(resolve_scope(rt, ty, b)?),
        ),
    })
}

/// `via(ref_field, field)` 的被盯字段名解析：ref 可指向任意类型，
/// 注册期无法静态确定目标类型——按「全体类型中该名字段 id 一致」的约定解析，
/// 否则要求用底层 API 显式给 FieldId。
fn resolve_target_field(
    rt: &Runtime,
    ty: EntityTypeId,
    rf: &str,
    f: &str,
) -> Result<FieldId, String> {
    let mut found: Option<FieldId> = None;
    for t in 0..rt.type_count() {
        if let Ok(id) = rt.try_field(EntityTypeId(t as u32), f) {
            match found {
                None => found = Some(id),
                Some(prev) if prev == id => {}
                Some(_) => {
                    return Err(format!(
                        "via({rf}, {f})：字段 {f} 在不同类型上 id 不一致，无法按名解析；\
                         请用底层 API（Scope::Inst）显式指定 FieldId"
                    ));
                }
            }
        }
    }
    let _ = ty;
    found.ok_or_else(|| format!("via({rf}, {f})：没有任何类型有字段 {f}"))
}

fn resolve_expr(rt: &Runtime, ty: EntityTypeId, e: &Ei) -> Result<Expr, String> {
    Ok(match e {
        Ei::New(p) => Expr::Val(ValRef::New(p.clone())),
        Ei::Old(p) => Expr::Val(ValRef::Old(p.clone())),
        Ei::OwnF(f) => Expr::Val(ValRef::Own(rt.try_field(ty, f)?)),
        Ei::Const(v) => Expr::Val(ValRef::Const(v.clone())),
        Ei::SelfRef => Expr::Val(ValRef::SelfRef),
        Ei::Writer => return Err("writer() 仅可用于投影侧（each/batch）".into()),
        Ei::Add(a, b) => Expr::Add(
            Box::new(resolve_expr(rt, ty, a)?),
            Box::new(resolve_expr(rt, ty, b)?),
        ),
        Ei::Sub(a, b) => Expr::Sub(
            Box::new(resolve_expr(rt, ty, a)?),
            Box::new(resolve_expr(rt, ty, b)?),
        ),
        Ei::Mul(a, b) => Expr::Mul(
            Box::new(resolve_expr(rt, ty, a)?),
            Box::new(resolve_expr(rt, ty, b)?),
        ),
        Ei::Div(a, b) => Expr::Div(
            Box::new(resolve_expr(rt, ty, a)?),
            Box::new(resolve_expr(rt, ty, b)?),
        ),
    })
}

fn resolve_cond(rt: &Runtime, ty: EntityTypeId, c: &Ci) -> Result<Cond, String> {
    Ok(match c {
        Ci::True => Cond::True,
        Ci::Cmp(a, op, b) => Cond::Cmp(
            resolve_expr(rt, ty, &a.0)?,
            *op,
            resolve_expr(rt, ty, &b.0)?,
        ),
        Ci::InRange(a, b) => Cond::InRange(*a, *b),
        Ci::InSet(vs) => Cond::InSet(vs.clone()),
        Ci::Changed => Cond::Changed,
        Ci::Became(v) => Cond::Became(v.clone()),
        Ci::Crossed(t, d) => Cond::Crossed(resolve_expr(rt, ty, &t.0)?, *d),
        Ci::And(a, b) => Cond::And(
            Box::new(resolve_cond(rt, ty, a)?),
            Box::new(resolve_cond(rt, ty, b)?),
        ),
        Ci::Or(a, b) => Cond::Or(
            Box::new(resolve_cond(rt, ty, a)?),
            Box::new(resolve_cond(rt, ty, b)?),
        ),
        Ci::AndNot(a, b) => Cond::AndNot(
            Box::new(resolve_cond(rt, ty, a)?),
            Box::new(resolve_cond(rt, ty, b)?),
        ),
    })
}

/// 投影解析（§3.4 可投影集：new/old 路径、writer id、own 字段）。
/// 注：核心库已开放投影侧四则（OQ2，见 route.rs `CompiledProj::Expr`）；
/// 此教学 shim 未跟进，仅解析 new/old/writer/own。
fn resolve_projs(rt: &Runtime, ty: EntityTypeId, ps: &[E]) -> Result<Vec<Proj>, String> {
    ps.iter()
        .map(|e| {
            Ok(match &e.0 {
                Ei::New(p) => Proj::New(p.clone()),
                Ei::Old(p) => Proj::Old(p.clone()),
                Ei::Writer => Proj::WriterId,
                Ei::OwnF(f) => Proj::Own(rt.try_field(ty, f)?),
                other => {
                    return Err(format!(
                        "投影仅允许 new/old/writer/own 字段（§3.4），不允许 {other:?}"
                    ))
                }
            })
        })
        .collect()
}
