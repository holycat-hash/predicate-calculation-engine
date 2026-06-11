//! 23 双人交易 / 对称原子交换（docs/23-symmetric-trade.md）的可运行验证：
//! 会话实体批内仲裁、报价代次使陈旧确认失效（同帧换价+确认也关死）、
//! 双侧 escrow 逐帧守恒、单值判决同帧原子互换、取消/死亡全额退回。

use std::collections::BTreeMap;

use pce::predicate::{inst, new_path, own, type_scope};
use pce::{
    CmpOp, Cond, Delivery, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, Scope, ValRef, Value,
};

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

fn scope_or(scopes: Vec<Scope>) -> Scope {
    scopes.into_iter().reduce(|a, b| Scope::Or(Box::new(a), Box::new(b))).unwrap()
}

/// 把 src（物品计数表）并入 dst。
fn merge_into(dst: &mut BTreeMap<String, Value>, src: &Value) {
    if let Value::Map(m) = src {
        for (k, v) in m {
            let cur = as_i64(dst.get(k).unwrap_or(&Value::Int(0)));
            dst.insert(k.clone(), Value::Int(cur + as_i64(v)));
        }
    }
}

/// 从 dst 扣除 src；不足则一件不动返回 false（check 与 act 同一次运行）。
fn take_from(dst: &mut BTreeMap<String, Value>, src: &Value) -> bool {
    let Value::Map(m) = src else { return false };
    if m.iter().any(|(k, v)| as_i64(dst.get(k).unwrap_or(&Value::Int(0))) < as_i64(v)) {
        return false;
    }
    for (k, v) in m {
        let cur = as_i64(dst.get(k).unwrap_or(&Value::Int(0)));
        dst.insert(k.clone(), Value::Int(cur - as_i64(v)));
    }
    true
}

#[derive(Clone, Copy)]
struct W {
    player_ty: EntityTypeId,
    trade_ty: EntityTypeId,
    p_cmd: FieldId,
    p_items: FieldId,
    p_escrow: FieldId,
    p_pending: FieldId,
    t_a: FieldId,
    t_b: FieldId,
    t_gen: FieldId,
    t_state: FieldId,
    t_verdict: FieldId,
}

fn setup() -> (Runtime, W) {
    let mut rt = Runtime::new();
    let player_ty = rt.register_entity_type(
        "Player",
        vec![
            FieldDef::new("cmd", Value::Null),
            FieldDef::new("items", Value::Map(BTreeMap::new())),
            FieldDef::new("escrow", Value::Map(BTreeMap::new())),
            FieldDef::reference("pending_trade"),
            FieldDef::new("trade_out", Value::Null),
            FieldDef::new("reclaim_op", Value::Null),
            FieldDef::new("applied_gen", Value::Int(0)),
        ],
        false,
    );
    let trade_ty = rt.register_entity_type(
        "Trade",
        vec![
            FieldDef::reference("a"),
            FieldDef::reference("b"),
            FieldDef::new("offer_a", Value::Map(BTreeMap::new())),
            FieldDef::new("offer_b", Value::Map(BTreeMap::new())),
            FieldDef::new("gen", Value::Int(0)),
            FieldDef::new("confirm_a", Value::Null),
            FieldDef::new("confirm_b", Value::Null),
            FieldDef::new("state", Value::str("open")),
            FieldDef::new("verdict", Value::Null),
        ],
        false,
    );
    let w = W {
        player_ty,
        trade_ty,
        p_cmd: rt.field(player_ty, "cmd"),
        p_items: rt.field(player_ty, "items"),
        p_escrow: rt.field(player_ty, "escrow"),
        p_pending: rt.field(player_ty, "pending_trade"),
        t_a: rt.field(trade_ty, "a"),
        t_b: rt.field(trade_ty, "b"),
        t_gen: rt.field(trade_ty, "gen"),
        t_state: rt.field(trade_ty, "state"),
        t_verdict: rt.field(trade_ty, "verdict"),
    };
    let t_offer_a = rt.field(trade_ty, "offer_a");
    let t_offer_b = rt.field(trade_ty, "offer_b");
    let t_conf_a = rt.field(trade_ty, "confirm_a");
    let t_conf_b = rt.field(trade_ty, "confirm_b");
    let p_trade_out = rt.field(player_ty, "trade_out");
    let p_reclaim = rt.field(player_ty, "reclaim_op");
    let p_applied = rt.field(player_ty, "applied_gen");

    // 1 仲裁者：报价 / 确认 / 取消 / 死亡（ref 收尸 null）合流到会话单写者；
    //   批内全序：报价 < 取消 < 确认 → 同帧换价必使陈旧确认作废
    let (f_a, f_b, f_gen, f_state, f_verdict) = (w.t_a, w.t_b, w.t_gen, w.t_state, w.t_verdict);
    rt.register_calculation(
        "broker",
        trade_ty,
        Predicate::new(
            scope_or(vec![type_scope(player_ty, p_trade_out), own(f_a), own(f_b)]),
            Cond::Or(
                Box::new(Cond::Cmp(new_path(&["trade"]), CmpOp::Eq, Expr::Val(ValRef::SelfRef))),
                Box::new(Cond::Became(Value::Null)),
            ),
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[t_offer_a, t_offer_b, f_gen, t_conf_a, t_conf_b, f_state, f_verdict],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            if as_str(&ctx.read_own(f_state)) != "open" {
                return; // done 后短路一切后续 op：双判决不可能
            }
            let a = ctx.read_own(f_a);
            let b = ctx.read_own(f_b);
            let mut cur_gen = as_i64(&ctx.read_own(f_gen));
            let mut offer_a = ctx.read_own(t_offer_a);
            let mut offer_b = ctx.read_own(t_offer_b);
            let mut conf_a = ctx.read_own(t_conf_a);
            let mut conf_b = ctx.read_own(t_conf_b);

            // 死亡信号（runtime 把 a/b ref 写 null，§6.3）→ 立即流拍
            if rows.iter().any(|r| r[0] == Value::Null) {
                ctx.write(
                    f_verdict,
                    Value::map([("kind", Value::str("abort")), ("gen", Value::Int(cur_gen))]),
                );
                ctx.write(f_state, Value::str("done"));
                return;
            }
            // 批内全序（手法 10）：报价先于确认；同级按 seq 决胜（D3 合规）
            let rank = |v: &Value| match as_str(&path(v, "kind")).as_str() {
                "offer" => 0,
                "cancel" => 1,
                _ => 2,
            };
            let mut ops: Vec<Value> = rows.iter().map(|r| r[0].clone()).collect();
            ops.sort_by_key(|v| (rank(v), as_i64(&path(v, "seq"))));
            let mut aborted = false;
            for op in ops {
                let who = path(&op, "who");
                match as_str(&path(&op, "kind")).as_str() {
                    "offer" => {
                        if who == a {
                            offer_a = path(&op, "items");
                        } else if who == b {
                            offer_b = path(&op, "items");
                        } else {
                            continue;
                        }
                        cur_gen += 1; // 代次失效：任何改价清空双方确认
                        conf_a = Value::Null;
                        conf_b = Value::Null;
                    }
                    "cancel" => {
                        aborted = true;
                        break;
                    }
                    "confirm" => {
                        // 确认携带它基于的代次：与当前代次相符才算数
                        if cur_gen > 0 && as_i64(&path(&op, "gen")) == cur_gen {
                            if who == a {
                                conf_a = Value::Int(cur_gen);
                            } else if who == b {
                                conf_b = Value::Int(cur_gen);
                            }
                        }
                    }
                    _ => {}
                }
            }
            if aborted {
                ctx.write(
                    f_verdict,
                    Value::map([("kind", Value::str("abort")), ("gen", Value::Int(cur_gen))]),
                );
                ctx.write(f_state, Value::str("done"));
            } else if cur_gen > 0 && conf_a == Value::Int(cur_gen) && conf_b == Value::Int(cur_gen) {
                // 单值判决：两个方向的支付在同一个值快照里，双方同帧各取半边
                ctx.write(
                    f_verdict,
                    Value::map([
                        ("kind", Value::str("commit")),
                        ("gen", Value::Int(cur_gen)),
                        ("a", a.clone()),
                        ("b", b.clone()),
                        ("to_a", offer_b.clone()),
                        ("to_b", offer_a.clone()),
                    ]),
                );
                ctx.write(f_state, Value::str("done"));
            }
            ctx.write(t_offer_a, offer_a);
            ctx.write(t_offer_b, offer_b);
            ctx.write(f_gen, Value::Int(cur_gen));
            ctx.write(t_conf_a, conf_a);
            ctx.write(t_conf_b, conf_b);
        }),
    )
    .unwrap();

    // 2 钱包：命令、判决（经自己持有的 ref 精确盯会话）、收尸退款合流到唯一写者
    let (f_cmd, f_items, f_escrow, f_pending) = (w.p_cmd, w.p_items, w.p_escrow, w.p_pending);
    rt.register_calculation(
        "wallet",
        player_ty,
        Predicate::new(
            scope_or(vec![own(f_cmd), inst(f_pending, f_verdict), own(p_reclaim)]),
            Cond::True,
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[f_items, f_escrow, f_pending, p_applied, p_trade_out],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let mut items = map_of(&ctx.read_own(f_items));
            let mut escrow = map_of(&ctx.read_own(f_escrow));
            let mut applied = as_i64(&ctx.read_own(p_applied));
            let mut pending = ctx.read_own(f_pending);
            let mut out: Option<Value> = None;

            // 批内全序：判决 / 退款先于新命令
            let rank = |v: &Value| match as_str(&path(v, "kind")).as_str() {
                "commit" | "abort" => 0,
                "reclaim" => 1,
                _ => 2,
            };
            let mut ops: Vec<Value> = rows.iter().map(|r| r[0].clone()).collect();
            ops.sort_by_key(|v| (rank(v), as_i64(&path(v, "seq"))));
            for op in ops {
                match as_str(&path(&op, "kind")).as_str() {
                    "commit" => {
                        let g = as_i64(&path(&op, "gen"));
                        if g > applied {
                            applied = g; // 幂等：同一判决至多应用一次
                            let me_a = path(&op, "a") == Value::Ref(ctx.self_id());
                            let recv = if me_a { path(&op, "to_a") } else { path(&op, "to_b") };
                            escrow.clear(); // 我押的货已易主（对方经同一判决收取）
                            merge_into(&mut items, &recv);
                            pending = Value::Null;
                        }
                    }
                    "abort" => {
                        let refund = Value::Map(std::mem::take(&mut escrow));
                        merge_into(&mut items, &refund);
                        pending = Value::Null;
                    }
                    "reclaim" => {
                        // 收尸退款：空 escrow 幂等无操作（19 同款假阳性吸收）
                        let refund = Value::Map(std::mem::take(&mut escrow));
                        merge_into(&mut items, &refund);
                    }
                    "offer" => {
                        // 改价先退旧押，再验足新押（check 与 act 同一次运行，无帧缝）
                        let refund = Value::Map(std::mem::take(&mut escrow));
                        merge_into(&mut items, &refund);
                        let want = path(&op, "items");
                        if take_from(&mut items, &want) {
                            escrow = map_of(&want);
                            pending = path(&op, "trade");
                            out = Some(Value::map([
                                ("kind", Value::str("offer")),
                                ("trade", path(&op, "trade")),
                                ("who", Value::Ref(ctx.self_id())),
                                ("items", want.clone()),
                                ("seq", path(&op, "seq")),
                            ]));
                        }
                    }
                    "confirm" => {
                        out = Some(Value::map([
                            ("kind", Value::str("confirm")),
                            ("trade", path(&op, "trade")),
                            ("who", Value::Ref(ctx.self_id())),
                            ("gen", path(&op, "gen")),
                            ("seq", path(&op, "seq")),
                        ]));
                    }
                    "cancel" => {
                        out = Some(Value::map([
                            ("kind", Value::str("cancel")),
                            ("trade", path(&op, "trade")),
                            ("who", Value::Ref(ctx.self_id())),
                            ("seq", path(&op, "seq")),
                        ]));
                    }
                    _ => {}
                }
            }
            ctx.write(f_items, Value::Map(items));
            ctx.write(f_escrow, Value::Map(escrow));
            ctx.write(p_applied, Value::Int(applied));
            ctx.write(f_pending, pending);
            if let Some(o) = out {
                ctx.write(p_trade_out, o);
            }
        }),
    )
    .unwrap();

    // 3 收尸探针：会话死亡 → runtime 写 pending_trade = null → 退款（19 同款）
    rt.register_calculation(
        "reclaim_probe",
        player_ty,
        Predicate::new(own(w.p_pending), Cond::Became(Value::Null), Delivery::Each(vec![])),
        &[p_reclaim],
        Box::new(move |ctx, _| {
            ctx.write(p_reclaim, Value::map([("kind", Value::str("reclaim"))]));
        }),
    )
    .unwrap();

    (rt, w)
}

// ---- 测试辅助 ----

fn one(name: &str) -> Value {
    Value::map([(name, Value::Int(1))])
}

fn gold(n: i64) -> Value {
    Value::map([("gold", Value::Int(n))])
}

fn cmd_offer(trade: InstanceId, items: Value, seq: i64) -> Value {
    Value::map([
        ("kind", Value::str("offer")),
        ("trade", Value::Ref(trade)),
        ("items", items),
        ("seq", Value::Int(seq)),
    ])
}

fn cmd_confirm(trade: InstanceId, based_gen: i64, seq: i64) -> Value {
    Value::map([
        ("kind", Value::str("confirm")),
        ("trade", Value::Ref(trade)),
        ("gen", Value::Int(based_gen)),
        ("seq", Value::Int(seq)),
    ])
}

fn cmd_cancel(trade: InstanceId, seq: i64) -> Value {
    Value::map([
        ("kind", Value::str("cancel")),
        ("trade", Value::Ref(trade)),
        ("seq", Value::Int(seq)),
    ])
}

fn spawn_pair(rt: &mut Runtime, w: &W, a_items: Value, b_items: Value) -> (InstanceId, InstanceId, InstanceId) {
    let a = rt.spawn(w.player_ty, vec![(w.p_items, a_items)]);
    let b = rt.spawn(w.player_ty, vec![(w.p_items, b_items)]);
    let t = rt.spawn(w.trade_ty, vec![(w.t_a, Value::Ref(a)), (w.t_b, Value::Ref(b))]);
    (a, b, t)
}

/// 某种物品在双方 items + escrow 中的总量（死者读到 Null 计 0）。
fn total(rt: &Runtime, players: &[InstanceId], w: &W, item: &str) -> i64 {
    players
        .iter()
        .map(|p| {
            as_i64(&path(&rt.read(*p, w.p_items), item))
                + as_i64(&path(&rt.read(*p, w.p_escrow), item))
        })
        .sum()
}

/// 推进 n 帧，每个帧边界检查守恒式。
fn step_conserved(rt: &mut Runtime, players: &[InstanceId], w: &W, n: usize, expect: &[(&str, i64)]) {
    for _ in 0..n {
        rt.step();
        for (item, amount) in expect {
            assert_eq!(total(rt, players, w, item), *amount, "{item} 总量在帧边界破坏守恒");
        }
    }
}

fn item_count(rt: &Runtime, p: InstanceId, w: &W, item: &str) -> i64 {
    as_i64(&path(&rt.read(p, w.p_items), item))
}

#[test]
fn double_confirm_swaps_atomically_with_conservation() {
    let (mut rt, w) = setup();
    let (a, b, t) = spawn_pair(&mut rt, &w, one("sword"), gold(100));
    let goods = [("sword", 1), ("gold", 100)];

    rt.debug_write(a, w.p_cmd, cmd_offer(t, one("sword"), 1));
    rt.debug_write(b, w.p_cmd, cmd_offer(t, gold(100), 2));
    step_conserved(&mut rt, &[a, b], &w, 2, &goods); // 双侧押入 + 会话记账
    assert_eq!(as_i64(&rt.read(t, w.t_gen)), 2);

    rt.debug_write(a, w.p_cmd, cmd_confirm(t, 2, 3));
    rt.debug_write(b, w.p_cmd, cmd_confirm(t, 2, 4));
    step_conserved(&mut rt, &[a, b], &w, 3, &goods); // 确认 → 判决 → 双侧同帧应用
    assert_eq!(as_str(&rt.read(t, w.t_state)), "done");
    assert_eq!(item_count(&rt, a, &w, "gold"), 100);
    assert_eq!(item_count(&rt, b, &w, "sword"), 1);
    assert_eq!(map_of(&rt.read(a, w.p_escrow)).len(), 0);
    assert_eq!(map_of(&rt.read(b, w.p_escrow)).len(), 0);

    // 判决后重放陈旧确认：done 短路，无第二次判决，无复制
    rt.debug_write(a, w.p_cmd, cmd_confirm(t, 2, 5));
    step_conserved(&mut rt, &[a, b], &w, 3, &goods);
    assert_eq!(item_count(&rt, a, &w, "gold"), 100);
    assert_eq!(item_count(&rt, b, &w, "sword"), 1);
}

#[test]
fn offer_swap_invalidates_stale_confirm_even_same_frame() {
    let (mut rt, w) = setup();
    let a_items = Value::map([("sword", Value::Int(1)), ("rock", Value::Int(1))]);
    let (a, b, t) = spawn_pair(&mut rt, &w, a_items, gold(100));
    let goods = [("sword", 1), ("rock", 1), ("gold", 100)];

    rt.debug_write(a, w.p_cmd, cmd_offer(t, one("sword"), 1));
    rt.debug_write(b, w.p_cmd, cmd_offer(t, gold(100), 2));
    step_conserved(&mut rt, &[a, b], &w, 2, &goods);
    assert_eq!(as_i64(&rt.read(t, w.t_gen)), 2);

    // 经典诈骗：B 基于 gen 2（看到 sword）确认，A 同帧把报价换成 rock
    rt.debug_write(b, w.p_cmd, cmd_confirm(t, 2, 3));
    rt.debug_write(a, w.p_cmd, cmd_offer(t, one("rock"), 4));
    step_conserved(&mut rt, &[a, b], &w, 4, &goods);

    // 批内全序：报价先行 → gen = 3，B 的 gen 2 确认作废；绝不按旧报价成交
    assert_eq!(as_str(&rt.read(t, w.t_state)), "open");
    assert_eq!(rt.read(t, w.t_verdict), Value::Null);
    assert_eq!(as_i64(&rt.read(t, w.t_gen)), 3);
    assert_eq!(item_count(&rt, a, &w, "sword"), 1); // sword 退押回手
    assert_eq!(as_i64(&path(&rt.read(a, w.p_escrow), "rock")), 1);
    assert_eq!(item_count(&rt, b, &w, "gold") + as_i64(&path(&rt.read(b, w.p_escrow), "gold")), 100);

    // 基于新代次重新双确认：按 B 实际看到的 rock 成交，机制不死锁
    rt.debug_write(b, w.p_cmd, cmd_confirm(t, 3, 5));
    rt.debug_write(a, w.p_cmd, cmd_confirm(t, 3, 6));
    step_conserved(&mut rt, &[a, b], &w, 3, &goods);
    assert_eq!(as_str(&rt.read(t, w.t_state)), "done");
    assert_eq!(item_count(&rt, a, &w, "gold"), 100);
    assert_eq!(item_count(&rt, a, &w, "sword"), 1);
    assert_eq!(item_count(&rt, b, &w, "rock"), 1);
    assert_eq!(item_count(&rt, b, &w, "gold"), 0);
}

#[test]
fn cancel_refunds_both_escrows_in_full() {
    let (mut rt, w) = setup();
    let (a, b, t) = spawn_pair(&mut rt, &w, one("sword"), gold(100));
    let goods = [("sword", 1), ("gold", 100)];

    rt.debug_write(a, w.p_cmd, cmd_offer(t, one("sword"), 1));
    rt.debug_write(b, w.p_cmd, cmd_offer(t, gold(100), 2));
    step_conserved(&mut rt, &[a, b], &w, 2, &goods);

    rt.debug_write(a, w.p_cmd, cmd_cancel(t, 3));
    step_conserved(&mut rt, &[a, b], &w, 5, &goods); // 取消 → abort → 双侧退款 → 探针幂等
    assert_eq!(as_str(&rt.read(t, w.t_state)), "done");
    assert_eq!(item_count(&rt, a, &w, "sword"), 1);
    assert_eq!(item_count(&rt, b, &w, "gold"), 100);
    assert_eq!(map_of(&rt.read(a, w.p_escrow)).len(), 0);
    assert_eq!(map_of(&rt.read(b, w.p_escrow)).len(), 0);
}

#[test]
fn player_death_mid_trade_refunds_survivor_via_ref_corpse() {
    let (mut rt, w) = setup();
    let (a, b, t) = spawn_pair(&mut rt, &w, one("sword"), gold(100));

    rt.debug_write(a, w.p_cmd, cmd_offer(t, one("sword"), 1));
    rt.debug_write(b, w.p_cmd, cmd_offer(t, gold(100), 2));
    step_conserved(&mut rt, &[a, b], &w, 2, &[("sword", 1), ("gold", 100)]);
    rt.debug_write(b, w.p_cmd, cmd_confirm(t, 2, 3));
    step_conserved(&mut rt, &[a, b], &w, 2, &[("sword", 1), ("gold", 100)]);

    // A 下线 / 死亡：runtime 把 Trade.a 写 null（§6.3）→ 会话流拍 → 幸存者退款
    rt.destroy(a);
    step_conserved(&mut rt, &[a, b], &w, 5, &[("gold", 100)]); // 金币分文不差
    assert_eq!(as_str(&rt.read(t, w.t_state)), "done");
    assert_eq!(item_count(&rt, b, &w, "gold"), 100);
    assert_eq!(map_of(&rt.read(b, w.p_escrow)).len(), 0);
    assert_eq!(rt.read(b, w.p_pending), Value::Null);
}
