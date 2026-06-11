//! Executable checks for docs/06-event-materialization.md.

use pce::entity::FIELD_ALIVE;
use pce::predicate::{new_path, own, type_scope};
use pce::{
    CmpOp, Cond, Delivery, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, ValRef, Value,
};

#[derive(Clone, Copy)]
struct W {
    unit_ty: EntityTypeId,
    matcher_ty: EntityTypeId,
    u_notice_count: FieldId,
    u_last_trade: FieldId,
    m_pair_out: FieldId,
}

fn as_i64(v: &Value) -> i64 {
    v.as_f64().unwrap_or(0.0) as i64
}

fn target_is_self(path: &[&str]) -> Cond {
    Cond::Cmp(new_path(path), CmpOp::Eq, Expr::Val(ValRef::SelfRef))
}

fn or(a: Cond, b: Cond) -> Cond {
    Cond::Or(Box::new(a), Box::new(b))
}

fn trade(a: InstanceId, b: InstanceId, price: i64) -> Value {
    Value::map([
        ("a", Value::Ref(a)),
        ("b", Value::Ref(b)),
        ("price", Value::Int(price)),
    ])
}

fn setup() -> (Runtime, W) {
    let mut rt = Runtime::new();
    let unit_ty = rt.register_entity_type(
        "Unit",
        vec![
            FieldDef::new("notice_count", Value::Int(0)),
            FieldDef::reference("last_trade"),
        ],
        false,
    );
    let matcher_ty = rt.register_entity_type(
        "Matcher",
        vec![FieldDef::new("pair_out", Value::Null)],
        false,
    );
    let trade_ty = rt.register_entity_type(
        "Trade",
        vec![
            FieldDef::new("members", Value::Null),
            FieldDef::new("ttl_seen", Value::Bool(false)),
        ],
        false,
    );

    let w = W {
        unit_ty,
        matcher_ty,
        u_notice_count: rt.field(unit_ty, "notice_count"),
        u_last_trade: rt.field(unit_ty, "last_trade"),
        m_pair_out: rt.field(matcher_ty, "pair_out"),
    };
    let trade_members = rt.field(trade_ty, "members");
    let trade_ttl_seen = rt.field(trade_ty, "ttl_seen");

    rt.register_calculation(
        "match_spawn",
        matcher_ty,
        Predicate::new(
            own(w.m_pair_out),
            Cond::True,
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            for row in rows {
                ctx.spawn(trade_ty, vec![(trade_members, row[0].clone())]);
            }
        }),
    )
    .unwrap();

    let notice_count = w.u_notice_count;
    let last_trade = w.u_last_trade;
    rt.register_calculation(
        "on_trade",
        unit_ty,
        Predicate::new(
            type_scope(trade_ty, trade_members),
            or(target_is_self(&["a"]), target_is_self(&["b"])),
            Delivery::Each(vec![Proj::New(vec![]), Proj::WriterId]),
        ),
        &[notice_count, last_trade],
        Box::new(move |ctx, input| {
            let Input::Each(row) = input else { return };
            let count = as_i64(&ctx.read_own(notice_count));
            ctx.write(notice_count, Value::Int(count + 1));
            ctx.write(last_trade, row[1].clone());
        }),
    )
    .unwrap();

    rt.register_calculation(
        "trade_mark_seen",
        trade_ty,
        Predicate::new(
            own(FIELD_ALIVE),
            Cond::Became(Value::Bool(true)),
            Delivery::Each(vec![]),
        ),
        &[trade_ttl_seen],
        Box::new(move |ctx, _| ctx.write(trade_ttl_seen, Value::Bool(true))),
    )
    .unwrap();
    rt.register_calculation(
        "trade_reap",
        trade_ty,
        Predicate::new(
            own(trade_ttl_seen),
            Cond::Became(Value::Bool(true)),
            Delivery::Each(vec![]),
        ),
        &[],
        Box::new(|ctx, _| ctx.destroy_self()),
    )
    .unwrap();

    (rt, w)
}

#[test]
fn spawned_trade_events_do_not_fold_and_reach_both_members() {
    let (mut rt, w) = setup();
    let matcher = rt.spawn(w.matcher_ty, vec![]);
    let a = rt.spawn(w.unit_ty, vec![]);
    let b = rt.spawn(w.unit_ty, vec![]);
    let c = rt.spawn(w.unit_ty, vec![]);
    let d = rt.spawn(w.unit_ty, vec![]);

    rt.debug_write(matcher, w.m_pair_out, trade(a, b, 7));
    rt.debug_write(matcher, w.m_pair_out, trade(c, d, 11));

    rt.step();
    for unit in [a, b, c, d] {
        assert_eq!(as_i64(&rt.read(unit, w.u_notice_count)), 0);
        assert_eq!(rt.read(unit, w.u_last_trade), Value::Null);
    }

    rt.step();
    let ab_trade = rt.read(a, w.u_last_trade);
    let cd_trade = rt.read(c, w.u_last_trade);
    assert!(matches!(ab_trade, Value::Ref(_)));
    assert!(matches!(cd_trade, Value::Ref(_)));
    assert_ne!(ab_trade, cd_trade);
    assert_eq!(rt.read(b, w.u_last_trade), ab_trade);
    assert_eq!(rt.read(d, w.u_last_trade), cd_trade);
    for unit in [a, b, c, d] {
        assert_eq!(as_i64(&rt.read(unit, w.u_notice_count)), 1);
    }

    rt.step();
    for unit in [a, b, c, d] {
        assert_eq!(rt.read(unit, w.u_last_trade), Value::Null);
        assert_eq!(as_i64(&rt.read(unit, w.u_notice_count)), 1);
    }
}
