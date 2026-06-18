//! predicate 层：封闭代数（§3）。
//!
//! 谓词是注册期定型的声明式结构（数据 / AST），不是任意函数；
//! 图灵完备性全部留给 calculation。形状为三段式 `(scope, condition, delivery)`：
//! - scope：静态部分，注册期已知、可索引，决定「谁该被叫醒」
//! - condition：动态部分，逐写 O(1) 判定，决定「值得醒吗」
//! - delivery：下推的投影与势，决定「递过去什么、递几次」
//!
//! 词汇表准入标准（§3.5）：一个原语能进入本层，当且仅当存在注册期可建的索引，
//! 使其判定摊销在 §4 成本预算内。凡绑不上索引的表达力，要么物化为 entity，
//! 要么留在 calculation。**禁止在此追加新变体而不给出索引绑定。**

use crate::entity::{EntityTypeId, FieldId};
use crate::value::Value;

/// 嗅探范围（§3.2）。
#[derive(Debug, Clone)]
pub enum Scope {
    /// 自己实例的某个 cell。
    Own(FieldId),
    /// 经由自己持有的 ref 字段，盯一个特定实例的 cell。
    /// ref 必须来自自己实例的 ref 类型字段（注册期校验）。
    Inst { ref_field: FieldId, field: FieldId },
    /// 某 entity 类型的全体实例（通配 id）。
    Type(EntityTypeId, FieldId),
    /// 并：任一来源有写入即触发。
    Or(Box<Scope>, Box<Scope>),
    /// 合取：同帧都有写入才触发（runtime 用每帧位码/计数闩实现，§4）。
    And(Box<Scope>, Box<Scope>),
}

/// 条件可引用的信息是封闭集（§3.3）：new、old、own 行字段、常量（含 self）。
/// **不允许引用其他实例的行**——那是 join，需要时物化为 entity（§6.1）。
#[derive(Debug, Clone)]
pub enum ValRef {
    /// 本次写入值；结构化 cell 允许字段路径（如 `new.target` → New(["target"])）。
    New(Vec<String>),
    /// 该 cell 上一帧值（双缓冲免费）。
    Old(Vec<String>),
    /// 订阅者自己实例的字段（一次点查；使阈值变「活」，代价见 §4 诚实退化条款）。
    Own(FieldId),
    Const(Value),
    /// 自身实例引用（常量；典型用法 `new.target = self`）。
    SelfRef,
}

/// 比较操作数允许常量与 own 字段的标量四则运算（如 `0.3 * own.hp_max`，§3.3）。
#[derive(Debug, Clone)]
pub enum Expr {
    Val(ValRef),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    Div(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
}

/// 值条件（§3.3）。每个变体在 §4 表中有对应索引绑定。
#[derive(Debug, Clone)]
pub enum Cond {
    /// 无条件（写即触发，D2）。
    True,
    Cmp(Expr, CmpOp, Expr),
    /// new in [a, b]（闭区间，数值）。
    InRange(f64, f64),
    /// new in {…}（等值集合 → 值桶索引）。
    InSet(Vec<Value>),
    /// new ≠ old。与 D2 配合：写即事件，「值真的变了」要显式问。
    Changed,
    /// old ≠ v ∧ new = v。
    Became(Value),
    /// 边沿穿越：Down: old ≥ t ∧ new < t；Up 对称。阈值可为活表达式。
    Crossed(Expr, Dir),
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
    /// 否定仅作守卫（§3.3）：「没有写入」不是事件；守卫式仍需正触发源，稀疏性不破。
    AndNot(Box<Cond>, Box<Cond>),
}

/// project 可投影的项（§3.4）。交付一律是值快照，不是引用。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Proj {
    New(Vec<String>),
    Old(Vec<String>),
    /// writer 的实例 id（作为 ref 值交付）。
    WriterId,
    /// 订阅者 own 行字段。
    Own(FieldId),
    /// 投影侧标量四则（§3.4）：与条件侧同一封闭集（new/old/own/const，含 self），
    /// 复用条件 Expr 的预编译器求值。不引用其他实例 ⇒ 非 join、不动成本模型；
    /// 投影发生在命中之后，O(表达式大小)/交付行。
    Expr(Expr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldOp {
    /// ±delta 增量维护，O(1)/写。
    Sum,
    Count,
    /// 堆或懒重算，O(log n)/写（非可逆先例，§8）。
    Min,
    Max,
}

/// 投影与势（§3.4）。
#[derive(Debug, Clone)]
pub enum Delivery {
    /// 每条命中触发一次 calculation。
    /// D3 推论一：一帧内可能运行多次；禁止用 each 做读-改-写累加。
    Each(Vec<Proj>),
    /// 整帧聚为一批，一次交付；顺序未定义（D3），消费逻辑必须顺序无关。
    Batch(Vec<Proj>),
    /// runtime 增量维护，仅交付聚合值（增量视图维护，本层最重要的下推形式）。
    Fold(FoldOp),
}

/// 谓词：挂在 calculation 之前。单谓词制（§1.4）：一个 calculation 恰有一个前置谓词。
#[derive(Debug, Clone)]
pub struct Predicate {
    pub scope: Scope,
    pub cond: Cond,
    pub delivery: Delivery,
}

impl Predicate {
    pub fn new(scope: Scope, cond: Cond, delivery: Delivery) -> Self {
        Predicate {
            scope,
            cond,
            delivery,
        }
    }
}

// ---- 构造便捷函数（保持声明式书写接近 §7 示例 DSL）----

pub fn own(field: FieldId) -> Scope {
    Scope::Own(field)
}

pub fn inst(ref_field: FieldId, field: FieldId) -> Scope {
    Scope::Inst { ref_field, field }
}

pub fn type_scope(ty: EntityTypeId, field: FieldId) -> Scope {
    Scope::Type(ty, field)
}

pub fn new_val() -> Expr {
    Expr::Val(ValRef::New(vec![]))
}

pub fn new_path(path: &[&str]) -> Expr {
    Expr::Val(ValRef::New(path.iter().map(|s| s.to_string()).collect()))
}

pub fn own_field(f: FieldId) -> Expr {
    Expr::Val(ValRef::Own(f))
}

pub fn lit(v: Value) -> Expr {
    Expr::Val(ValRef::Const(v))
}
