//! 阶段一：路由。持帧 N-1 写集 W：索引查找 → 条件判定 → 填充 each 触发 /
//! batch 缓冲 / 更新 fold（§2）。
//!
//! 成本目标 O(|W|·log + |F|)（§4）。当前脚手架与目标的差距标注在各分支。

use std::collections::HashMap;

use crate::calculation::{CalcId, Input};
use crate::entity::InstanceId;
use crate::predicate::{CmpOp, Cond, Delivery, Dir, Expr, FoldOp, Proj, ValRef};
use crate::value::Value;

use super::{Indexes, RegisteredCalc, Store, Trigger, WriteRec};

/// fold 增量状态（§3.4）。sum/count 可逆（±delta）；min/max 非可逆，
/// 此处为运行 min/max 的简化（成员退出后的懒重算 TODO，§8）。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FoldAcc {
    pub acc: f64,
    pub seen: bool,
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
    writes: &[WriteRec],
) -> Vec<Trigger> {
    let mut triggers: Vec<Trigger> = vec![];
    let mut batch_buf: HashMap<(CalcId, InstanceId), Vec<Vec<Value>>> = HashMap::new();
    let mut batch_order: Vec<(CalcId, InstanceId)> = vec![];
    // 合取闩（§4「& 合取」行）：每 (谓词, 订阅者) 每帧位码；最高位 = 本帧已触发
    let mut latch: HashMap<(CalcId, InstanceId), u32> = HashMap::new();
    let mut fold_hits: Vec<(CalcId, InstanceId)> = vec![];

    for w in writes {
        // 候选收集：三类 scope 各走各的索引
        let mut hits: Vec<(CalcId, u32, InstanceId)> = vec![];
        // own：订阅者 = writer 自己。哈希链 O(1)+触发数
        if let Some(subs) = idx.own.get(&(w.inst.ty, w.field)) {
            for &(c, g) in subs {
                hits.push((c, g, w.inst));
            }
        }
        // type：订阅者 = 该 calc 类型的全体实例
        if let Some(subs) = idx.type_.get(&(w.inst.ty, w.field)) {
            for &(c, g) in subs {
                let rc = &calcs[c.0 as usize];
                if let Some(path) = &rc.self_eq_path {
                    // 注册期识别的等值快路（§4 值桶的退化形：按 ref 点查）
                    if let Some(target) = w.new.get_path(path).as_ref_id() {
                        if target.ty == rc.ty {
                            hits.push((c, g, target));
                        }
                    }
                } else {
                    // TODO §4：常量阈值 → 共享排序阈值表；等值 → 值桶。
                    // 现为线性扇出，违反成本红线，仅脚手架阶段允许。
                    for sub in store.alive_instances(rc.ty) {
                        hits.push((c, g, sub));
                    }
                }
            }
        }
        // inst：经 ref 反向表点查持有者，再查 (持有者类型, ref 字段, 被盯字段) 索引
        if let Some(holders) = idx.ref_reverse.get(&w.inst) {
            for &(holder, rf) in holders {
                if let Some(subs) = idx.inst.get(&(holder.ty, rf, w.field)) {
                    for &(c, g) in subs {
                        hits.push((c, g, holder));
                    }
                }
            }
        }

        for (c, group, sub) in hits {
            if !store.alive(sub) {
                continue;
            }
            let rc = &calcs[c.0 as usize];
            let ectx = EvalCtx { store, sub, new: &w.new, old: &w.old };
            if !eval_cond(&rc.pred.cond, &ectx) {
                continue;
            }
            // 合取闩：组数 >1 时须同帧集齐全部组才触发（每组触发一次/帧）
            if rc.n_groups > 1 {
                let bits = latch.entry((c, sub)).or_insert(0);
                *bits |= 1 << group;
                let full = (1u32 << rc.n_groups) - 1;
                let fired = 1u32 << 31;
                if *bits & full != full || *bits & fired != 0 {
                    continue;
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
                    let buf = batch_buf.entry(key).or_insert_with(|| {
                        batch_order.push(key);
                        vec![]
                    });
                    buf.push(project(projs, w, sub, store));
                }
                Delivery::Fold(op) => {
                    let st = fold_state.entry((c, sub)).or_default();
                    apply_fold(*op, st, w);
                    if !fold_hits.contains(&(c, sub)) {
                        fold_hits.push((c, sub));
                    }
                }
            }
        }
    }

    // batch：整帧聚为一批，一次交付；顺序未定义（D3）——此处的插入序无任何承诺
    for key in batch_order {
        let rows = batch_buf.remove(&key).unwrap();
        triggers.push(Trigger { calc: key.0, subscriber: key.1, input: Input::Batch(rows) });
    }
    // fold：本帧有更新的聚合交付一次
    for (c, sub) in fold_hits {
        let st = fold_state[&(c, sub)];
        let v = match calcs[c.0 as usize].pred.delivery {
            Delivery::Fold(FoldOp::Count) => Value::Int(st.acc as i64),
            _ => Value::Float(st.acc),
        };
        triggers.push(Trigger { calc: c, subscriber: sub, input: Input::Fold(v) });
    }
    triggers
}

fn apply_fold(op: FoldOp, st: &mut FoldAcc, w: &WriteRec) {
    let new = w.new.as_f64().unwrap_or(0.0);
    let old = w.old.as_f64().unwrap_or(0.0);
    match op {
        // ±delta：O(1)/写（§4）
        FoldOp::Sum => st.acc += new - old,
        FoldOp::Count => st.acc += 1.0,
        FoldOp::Min => st.acc = if st.seen { st.acc.min(new) } else { new },
        FoldOp::Max => st.acc = if st.seen { st.acc.max(new) } else { new },
    }
    st.seen = true;
}

/// 投影（§3.4）：交付一律是值快照，不是引用——引用不跨实体，值经由谓词通道流动。
fn project(projs: &[Proj], w: &WriteRec, sub: InstanceId, store: &Store) -> Vec<Value> {
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
