//! 21 链接生命与伤害转移（docs/21-linked-life.md）的可运行验证：
//! 链式守恒分账、互保镜面环的整数衰减终止、保镖死亡时在途转发的收尸回收。

use std::collections::BTreeMap;

use pce::predicate::{new_path, own, type_scope};
use pce::{
    CmpOp, Cond, Delivery, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, Scope, Value, ValRef,
};

fn target_is_self() -> Cond {
    Cond::Cmp(new_path(&["target"]), CmpOp::Eq, Expr::Val(ValRef::SelfRef))
}

fn scope_or(scopes: Vec<Scope>) -> Scope {
    scopes
        .into_iter()
        .reduce(|a, b| Scope::Or(Box::new(a), Box::new(b)))
        .unwrap()
}

fn path(v: &Value, key: &str) -> Value {
    v.get_path(&[key.to_string()])
}

fn as_i64(v: &Value) -> i64 {
    v.as_f64().unwrap_or(0.0) as i64
}

fn as_str(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        _ => String::new(),
    }
}

fn map_of(v: &Value) -> BTreeMap<String, Value> {
    match v {
        Value::Map(m) => m.clone(),
        _ => BTreeMap::new(),
    }
}

#[derive(Clone, Copy)]
struct W {
    unit_ty: EntityTypeId,
    hp: FieldId,
    guard: FieldId,
    hit_in: FieldId,
    pending: FieldId,
}

fn setup() -> (Runtime, W) {
    let mut rt = Runtime::new();
    let unit_ty = rt.register_entity_type(
        "Unit",
        vec![
            FieldDef::new("hp", Value::Int(100)),
            FieldDef::reference("guard"),
            FieldDef::new("ratio", Value::Float(0.5)),
            FieldDef::new("hit_in", Value::Null),
            FieldDef::new("fwd_out", Value::Null),
            FieldDef::new("ack_out", Value::Null),
            FieldDef::new("pending", Value::Map(BTreeMap::new())),
            FieldDef::new("fwd_seq", Value::Int(0)),
            FieldDef::new("reclaim_op", Value::Null),
        ],
        false,
    );
    let w = W {
        unit_ty,
        hp: rt.field(unit_ty, "hp"),
        guard: rt.field(unit_ty, "guard"),
        hit_in: rt.field(unit_ty, "hit_in"),
        pending: rt.field(unit_ty, "pending"),
    };
    let ratio_f = rt.field(unit_ty, "ratio");
    let fwd_out = rt.field(unit_ty, "fwd_out");
    let ack_out = rt.field(unit_ty, "ack_out");
    let fwd_seq = rt.field(unit_ty, "fwd_seq");
    let reclaim_op = rt.field(unit_ty, "reclaim_op");

    // 1 唯一结算：直击/转发/回执/回收四路同形合流，批内全序 ack → reclaim → 伤害
    let (hp_f, guard_f, pending_f) = (w.hp, w.guard, w.pending);
    rt.register_calculation(
        "settle",
        unit_ty,
        Predicate::new(
            scope_or(vec![
                own(w.hit_in),
                type_scope(unit_ty, fwd_out),
                type_scope(unit_ty, ack_out),
                own(reclaim_op),
            ]),
            target_is_self(),
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[hp_f, fwd_out, ack_out, pending_f, fwd_seq],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let mut ops: Vec<Value> = rows.iter().map(|r| r[0].clone()).collect();
            let rank = |v: &Value| match as_str(&path(v, "kind")).as_str() {
                "ack" => 0,
                "reclaim" => 1,
                _ => 2,
            };
            ops.sort_by_key(|v| (rank(v), as_str(&path(v, "salt"))));
            let mut hp = as_i64(&ctx.read_own(hp_f));
            let mut pending = map_of(&ctx.read_own(pending_f));
            let mut seq = as_i64(&ctx.read_own(fwd_seq));
            let mut dmg = 0i64;
            let mut acks: Vec<(Value, String)> = vec![];
            for op in ops {
                match as_str(&path(&op, "kind")).as_str() {
                    // 回执：保镖已结算，删在途台账
                    "ack" => {
                        pending.remove(&as_str(&path(&op, "salt")));
                    }
                    // 收尸回收：在途转发全额落回自己（台账已删的部分幂等不重扣）
                    "reclaim" => {
                        let in_flight: i64 = pending.values().map(as_i64).sum();
                        hp -= in_flight;
                        pending.clear();
                    }
                    "hit" => dmg += as_i64(&path(&op, "amount")),
                    "fwd" => {
                        dmg += as_i64(&path(&op, "amount"));
                        acks.push((path(&op, "source"), as_str(&path(&op, "salt"))));
                    }
                    _ => {}
                }
            }
            if dmg > 0 {
                // 净算后分账：f = ⌊D×ratio⌋，keep = D − f（余数留本地，守恒）
                let mut keep = dmg;
                let guard = ctx.read_own(guard_f);
                if let Value::Ref(_) = guard {
                    let ratio = ctx.read_own(ratio_f).as_f64().unwrap_or(0.0);
                    let f = ((dmg as f64) * ratio).floor() as i64;
                    if f > 0 {
                        let salt = format!("u{}-{}", ctx.self_id().id, seq);
                        seq += 1;
                        ctx.write(
                            fwd_out,
                            Value::map([
                                ("target", guard.clone()),
                                ("kind", Value::str("fwd")),
                                ("amount", Value::Int(f)),
                                ("salt", Value::Str(salt.clone())),
                                ("source", Value::Ref(ctx.self_id())),
                            ]),
                        );
                        pending.insert(salt, Value::Int(f));
                        keep = dmg - f;
                    }
                }
                hp -= keep;
            }
            for (source, salt) in acks {
                ctx.write(
                    ack_out,
                    Value::map([
                        ("target", source),
                        ("kind", Value::str("ack")),
                        ("salt", Value::Str(salt)),
                    ]),
                );
            }
            ctx.write(hp_f, Value::Int(hp));
            ctx.write(pending_f, Value::Map(pending));
            ctx.write(fwd_seq, Value::Int(seq));
        }),
    )
    .unwrap();

    // 2 保镖收尸：§6.3 把 guard ref 写 null → 在途与未来转发全部回头
    rt.register_calculation(
        "reclaim_probe",
        unit_ty,
        Predicate::new(own(w.guard), Cond::Became(Value::Null), Delivery::Each(vec![])),
        &[reclaim_op],
        Box::new(move |ctx, _| {
            ctx.write(
                reclaim_op,
                Value::map([
                    ("target", Value::Ref(ctx.self_id())),
                    ("kind", Value::str("reclaim")),
                ]),
            );
        }),
    )
    .unwrap();

    (rt, w)
}

fn hit(target: InstanceId, amount: i64, salt: &str) -> Value {
    Value::map([
        ("target", Value::Ref(target)),
        ("kind", Value::str("hit")),
        ("amount", Value::Int(amount)),
        ("salt", Value::str(salt)),
    ])
}

fn hp_of(rt: &Runtime, u: InstanceId, w: W) -> i64 {
    as_i64(&rt.read(u, w.hp))
}

fn pending_len(rt: &Runtime, u: InstanceId, w: W) -> usize {
    map_of(&rt.read(u, w.pending)).len()
}

#[test]
fn guard_chain_splits_with_remainder_conservation() {
    let (mut rt, w) = setup();
    let a = rt.spawn(w.unit_ty, vec![]);
    let b = rt.spawn(w.unit_ty, vec![]);
    let c = rt.spawn(w.unit_ty, vec![]);
    rt.debug_write(a, w.guard, Value::Ref(b));
    rt.debug_write(b, w.guard, Value::Ref(c));

    rt.debug_write(a, w.hit_in, hit(a, 100, "h1"));
    for _ in 0..10 {
        rt.step();
    }

    // A 留 50 转 50；B 留 25 转 25；C 无保镖全收 25——逐跳 keep + f = D
    assert_eq!(hp_of(&rt, a, w), 50);
    assert_eq!(hp_of(&rt, b, w), 75);
    assert_eq!(hp_of(&rt, c, w), 75);
    let total_loss = (100 - hp_of(&rt, a, w)) + (100 - hp_of(&rt, b, w)) + (100 - hp_of(&rt, c, w));
    assert_eq!(total_loss, 100);
    // ack 已清空全部在途台账
    assert_eq!(pending_len(&rt, a, w), 0);
    assert_eq!(pending_len(&rt, b, w), 0);
}

#[test]
fn mutual_guard_mirror_ring_terminates_and_conserves() {
    let (mut rt, w) = setup();
    let a = rt.spawn(w.unit_ty, vec![]);
    let b = rt.spawn(w.unit_ty, vec![]);
    rt.debug_write(a, w.guard, Value::Ref(b));
    rt.debug_write(b, w.guard, Value::Ref(a));

    rt.debug_write(a, w.hit_in, hit(a, 100, "h1"));
    for _ in 0..30 {
        rt.step();
    }
    let (hp_a, hp_b) = (hp_of(&rt, a, w), hp_of(&rt, b, w));

    // 终止：整数衰减 ⌊D×0.5⌋ 严格递减，链在有限帧停（再推进若干帧不变）
    for _ in 0..5 {
        rt.step();
    }
    assert_eq!((hp_of(&rt, a, w), hp_of(&rt, b, w)), (hp_a, hp_b));

    // 守恒：100 = 50+13+3+1（A）+ 25+6+2+1（B），余数逐跳留存
    assert_eq!(hp_a, 33);
    assert_eq!(hp_b, 67);
    assert_eq!((100 - hp_a) + (100 - hp_b), 100);
    assert_eq!(pending_len(&rt, a, w), 0);
    assert_eq!(pending_len(&rt, b, w), 0);
}

#[test]
fn guard_death_reclaims_in_flight_forward() {
    let (mut rt, w) = setup();
    let a = rt.spawn(w.unit_ty, vec![]);
    let b = rt.spawn(w.unit_ty, vec![]);
    rt.debug_write(a, w.guard, Value::Ref(b));

    rt.debug_write(a, w.hit_in, hit(a, 100, "h1"));
    rt.step(); // A 结算：留 50，转 50 在途，台账 {50}
    assert_eq!(hp_of(&rt, a, w), 50);
    assert_eq!(pending_len(&rt, a, w), 1);

    // 保镖死于转发送达前：路由跳过死订阅者，fwd 蒸发——
    // 但台账还在，guard 被 runtime 写 null（§6.3）→ reclaim 全额兜回
    rt.destroy(b);
    for _ in 0..3 {
        rt.step();
    }
    assert_eq!(hp_of(&rt, a, w), 0); // 50 自留 + 50 回落，伤害不蒸发
    assert_eq!(pending_len(&rt, a, w), 0);
    assert_eq!(rt.read(a, w.guard), Value::Null);
}
