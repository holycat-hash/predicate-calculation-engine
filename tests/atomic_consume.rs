//! 19 原子消耗与双花（docs/19-atomic-consume.md）的可运行验证：
//! 单写者批内仲裁关双花、escrow 帧间 saga 逐帧守恒、reject 退款、
//! 商店死亡经 ref 收尸触发回滚。

use std::collections::BTreeMap;

use pce::predicate::{lit, new_path, own, type_scope};
use pce::{
    CmpOp, Cond, Delivery, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, Scope, Value, ValRef,
};

const PRICE: i64 = 3;

fn target_is_self() -> Cond {
    Cond::Cmp(new_path(&["target"]), CmpOp::Eq, Expr::Val(ValRef::SelfRef))
}

fn and(a: Cond, b: Cond) -> Cond {
    Cond::And(Box::new(a), Box::new(b))
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
    player_ty: EntityTypeId,
    shop_ty: EntityTypeId,
    door_ty: EntityTypeId,
    p_balance: FieldId,
    p_escrow: FieldId,
    p_spend_log: FieldId,
    p_buy_req: FieldId,
    p_pending_shop: FieldId,
    p_items: FieldId,
    s_stock: FieldId,
    d_demand: FieldId,
}

fn setup() -> (Runtime, W) {
    let mut rt = Runtime::new();
    let player_ty = rt.register_entity_type(
        "Player",
        vec![
            FieldDef::new("balance", Value::Int(10)),
            FieldDef::new("escrow", Value::Map(BTreeMap::new())),
            FieldDef::new("reserve_out", Value::Null),
            FieldDef::reference("pending_shop"),
            FieldDef::new("spend_log", Value::Map(BTreeMap::new())),
            FieldDef::new("buy_req", Value::Null),
            FieldDef::new("reclaim_op", Value::Null),
            FieldDef::new("items", Value::Int(0)),
        ],
        false,
    );
    let shop_ty = rt.register_entity_type(
        "Shop",
        vec![
            FieldDef::new("stock", Value::Int(0)),
            FieldDef::new("decide_out", Value::Null),
        ],
        false,
    );
    let door_ty =
        rt.register_entity_type("Door", vec![FieldDef::new("demand", Value::Null)], false);

    let w = W {
        player_ty,
        shop_ty,
        door_ty,
        p_balance: rt.field(player_ty, "balance"),
        p_escrow: rt.field(player_ty, "escrow"),
        p_spend_log: rt.field(player_ty, "spend_log"),
        p_buy_req: rt.field(player_ty, "buy_req"),
        p_pending_shop: rt.field(player_ty, "pending_shop"),
        p_items: rt.field(player_ty, "items"),
        s_stock: rt.field(shop_ty, "stock"),
        d_demand: rt.field(door_ty, "demand"),
    };
    let p_reserve_out = rt.field(player_ty, "reserve_out");
    let p_reclaim_op = rt.field(player_ty, "reclaim_op");
    let s_decide_out = rt.field(shop_ty, "decide_out");

    // 1 钱包：一切动钱的 op 同形合流到唯一写者，批内全序仲裁
    let (balance_f, escrow_f, log_f, pending_f) =
        (w.p_balance, w.p_escrow, w.p_spend_log, w.p_pending_shop);
    rt.register_calculation(
        "wallet",
        player_ty,
        Predicate::new(
            scope_or(vec![
                own(w.p_buy_req),
                type_scope(door_ty, w.d_demand),
                type_scope(shop_ty, s_decide_out),
                own(p_reclaim_op),
            ]),
            target_is_self(),
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[balance_f, escrow_f, p_reserve_out, pending_f, log_f],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let mut ops: Vec<Value> = rows.iter().map(|r| r[0].clone()).collect();
            // 批内全序（手法 10）：先结清旧约（grant/reject），再收尸退款
            // （reclaim），后花新钱（spend/buy）；同级按 req/salt 决胜
            let rank = |v: &Value| match as_str(&path(v, "kind")).as_str() {
                "grant" | "reject" => 0,
                "reclaim" => 1,
                _ => 2,
            };
            ops.sort_by_key(|v| {
                (rank(v), format!("{}{}", as_str(&path(v, "req")), as_str(&path(v, "salt"))))
            });
            let mut balance = as_i64(&ctx.read_own(balance_f));
            let mut escrow = map_of(&ctx.read_own(escrow_f));
            let mut log = map_of(&ctx.read_own(log_f));
            for op in ops {
                match as_str(&path(&op, "kind")).as_str() {
                    "grant" => {
                        // 钱真正花掉：删台账即结清；台账无此 req 则幂等无操作
                        escrow.remove(&as_str(&path(&op, "req")));
                        ctx.write(pending_f, Value::Null);
                    }
                    "reject" => {
                        if let Some(amt) = escrow.remove(&as_str(&path(&op, "req"))) {
                            balance += as_i64(&amt);
                        }
                        ctx.write(pending_f, Value::Null);
                    }
                    "reclaim" => {
                        for (_, amt) in std::mem::take(&mut escrow) {
                            balance += as_i64(&amt);
                        }
                    }
                    "spend" => {
                        let cost = as_i64(&path(&op, "cost"));
                        let salt = as_str(&path(&op, "salt"));
                        if balance >= cost {
                            balance -= cost;
                            log.insert(salt, Value::str("ok"));
                        } else {
                            log.insert(salt, Value::str("no"));
                        }
                    }
                    "buy" => {
                        let price = as_i64(&path(&op, "price"));
                        let req = as_str(&path(&op, "req"));
                        let shop = path(&op, "shop");
                        if balance >= price {
                            balance -= price;
                            escrow.insert(req.clone(), Value::Int(price));
                            ctx.write(
                                p_reserve_out,
                                Value::map([
                                    ("shop", shop.clone()),
                                    ("buyer", Value::Ref(ctx.self_id())),
                                    ("req", Value::Str(req)),
                                    ("price", Value::Int(price)),
                                ]),
                            );
                            ctx.write(pending_f, shop);
                        } else {
                            log.insert(req, Value::str("no"));
                        }
                    }
                    _ => {}
                }
            }
            ctx.write(balance_f, Value::Int(balance));
            ctx.write(escrow_f, Value::Map(escrow));
            ctx.write(log_f, Value::Map(log));
        }),
    )
    .unwrap();

    // 2 商店：库存 check 与 act 同处一次 batch 运行，无帧缝
    let stock_f = w.s_stock;
    rt.register_calculation(
        "shop_decide",
        shop_ty,
        Predicate::new(
            type_scope(player_ty, p_reserve_out),
            Cond::Cmp(new_path(&["shop"]), CmpOp::Eq, Expr::Val(ValRef::SelfRef)),
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[stock_f, s_decide_out],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let mut reserves: Vec<Value> = rows.iter().map(|r| r[0].clone()).collect();
            reserves.sort_by_key(|v| as_str(&path(v, "req")));
            let mut stock = as_i64(&ctx.read_own(stock_f));
            for r in reserves {
                let mut decide = vec![
                    ("target".to_string(), path(&r, "buyer")),
                    ("req".to_string(), path(&r, "req")),
                    ("price".to_string(), path(&r, "price")),
                ];
                if stock > 0 {
                    stock -= 1;
                    decide.push(("kind".to_string(), Value::str("grant")));
                    decide.push(("item".to_string(), Value::str("potion")));
                } else {
                    decide.push(("kind".to_string(), Value::str("reject")));
                }
                ctx.write(s_decide_out, Value::Map(decide.into_iter().collect()));
            }
            ctx.write(stock_f, Value::Int(stock));
        }),
    )
    .unwrap();

    // 3 入库：只认 grant，过滤在谓词层
    let items_f = w.p_items;
    rt.register_calculation(
        "stash",
        player_ty,
        Predicate::new(
            type_scope(shop_ty, s_decide_out),
            and(
                target_is_self(),
                Cond::Cmp(new_path(&["kind"]), CmpOp::Eq, lit(Value::str("grant"))),
            ),
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[items_f],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let items = as_i64(&ctx.read_own(items_f));
            ctx.write(items_f, Value::Int(items + rows.len() as i64));
        }),
    )
    .unwrap();

    // 4 收尸触发回滚：§6.3 死亡结算把 pending_shop 写成 null
    rt.register_calculation(
        "reclaim_probe",
        player_ty,
        Predicate::new(
            own(w.p_pending_shop),
            Cond::Became(Value::Null),
            Delivery::Each(vec![]),
        ),
        &[p_reclaim_op],
        Box::new(move |ctx, _| {
            ctx.write(
                p_reclaim_op,
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

/// 守恒式：balance + Σescrow + price×items 在 saga 的每一帧边界都不变。
fn conserved(rt: &Runtime, player: InstanceId, w: W) -> i64 {
    let escrow: i64 = map_of(&rt.read(player, w.p_escrow)).values().map(as_i64).sum();
    as_i64(&rt.read(player, w.p_balance)) + escrow + PRICE * as_i64(&rt.read(player, w.p_items))
}

fn buy_req(player: InstanceId, shop: InstanceId, req: &str) -> Value {
    Value::map([
        ("target", Value::Ref(player)),
        ("kind", Value::str("buy")),
        ("shop", Value::Ref(shop)),
        ("price", Value::Int(PRICE)),
        ("req", Value::str(req)),
    ])
}

#[test]
fn same_frame_double_spend_grants_exactly_one() {
    let (mut rt, w) = setup();
    let player = rt.spawn(w.player_ty, vec![(w.p_balance, Value::Int(5))]);
    let d1 = rt.spawn(w.door_ty, vec![]);
    let d2 = rt.spawn(w.door_ty, vec![]);

    let demand = |salt: &str| {
        Value::map([
            ("target", Value::Ref(player)),
            ("kind", Value::str("spend")),
            ("cost", Value::Int(3)),
            ("salt", Value::str(salt)),
        ])
    };
    // 同帧两扇门抢一笔钱：check 与 act 同处钱包一次 batch 运行，无帧缝
    rt.debug_write(d1, w.d_demand, demand("a"));
    rt.debug_write(d2, w.d_demand, demand("b"));
    rt.step();

    assert_eq!(as_i64(&rt.read(player, w.p_balance)), 2);
    let log = map_of(&rt.read(player, w.p_spend_log));
    assert_eq!(log.get("a"), Some(&Value::str("ok")));
    assert_eq!(log.get("b"), Some(&Value::str("no")));
}

#[test]
fn purchase_saga_conserves_value_every_frame() {
    let (mut rt, w) = setup();
    let player = rt.spawn(w.player_ty, vec![]);
    let shop = rt.spawn(w.shop_ty, vec![(w.s_stock, Value::Int(1))]);

    rt.debug_write(player, w.p_buy_req, buy_req(player, shop, "r1"));

    // 帧 1：预留——balance → escrow，钱不离开自己实例
    rt.step();
    assert_eq!(as_i64(&rt.read(player, w.p_balance)), 7);
    assert_eq!(map_of(&rt.read(player, w.p_escrow)).len(), 1);
    assert_eq!(conserved(&rt, player, w), 10);

    // 帧 2：商店判定——stock 扣减与 grant 同帧原子提交
    rt.step();
    assert_eq!(as_i64(&rt.read(shop, w.s_stock)), 0);
    assert_eq!(conserved(&rt, player, w), 10);

    // 帧 3：玩家结清——escrow 删账（钱花定），货入库
    rt.step();
    assert_eq!(map_of(&rt.read(player, w.p_escrow)).len(), 0);
    assert_eq!(as_i64(&rt.read(player, w.p_items)), 1);
    assert_eq!(conserved(&rt, player, w), 10);

    // 帧 4–5：正常结清写 pending=null 也触发 reclaim——空台账幂等无操作
    rt.step();
    rt.step();
    assert_eq!(as_i64(&rt.read(player, w.p_balance)), 7);
    assert_eq!(as_i64(&rt.read(player, w.p_items)), 1);
    assert_eq!(conserved(&rt, player, w), 10);
}

#[test]
fn out_of_stock_reject_refunds_escrow() {
    let (mut rt, w) = setup();
    let player = rt.spawn(w.player_ty, vec![]);
    let shop = rt.spawn(w.shop_ty, vec![(w.s_stock, Value::Int(0))]);

    rt.debug_write(player, w.p_buy_req, buy_req(player, shop, "r1"));
    for _ in 0..5 {
        rt.step();
        assert_eq!(conserved(&rt, player, w), 10);
    }
    assert_eq!(as_i64(&rt.read(player, w.p_balance)), 10);
    assert_eq!(map_of(&rt.read(player, w.p_escrow)).len(), 0);
    assert_eq!(as_i64(&rt.read(player, w.p_items)), 0);
}

#[test]
fn shop_death_after_reserve_rolls_back_via_ref_null() {
    let (mut rt, w) = setup();
    let player = rt.spawn(w.player_ty, vec![]);
    let shop = rt.spawn(w.shop_ty, vec![(w.s_stock, Value::Int(5))]);

    rt.debug_write(player, w.p_buy_req, buy_req(player, shop, "r1"));
    rt.step(); // 预留提交：escrow 3，pending_shop = shop
    assert_eq!(as_i64(&rt.read(player, w.p_balance)), 7);

    // 商店死亡：在途 reserve 路由落空（死订阅者被跳过），
    // runtime 把 pending_shop 写 null（§6.3）→ became(null) 触发回滚
    rt.destroy(shop);
    for _ in 0..3 {
        rt.step();
        assert_eq!(conserved(&rt, player, w), 10);
    }
    assert_eq!(as_i64(&rt.read(player, w.p_balance)), 10);
    assert_eq!(map_of(&rt.read(player, w.p_escrow)).len(), 0);
    assert_eq!(as_i64(&rt.read(player, w.p_items)), 0);
    assert_eq!(rt.read(player, w.p_pending_shop), Value::Null);
}
