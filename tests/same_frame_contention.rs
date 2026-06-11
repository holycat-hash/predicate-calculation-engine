//! 02 同帧多方抢唯一资源（docs/02-same-frame-contention.md）的可运行验证：
//! 资源实例单写者批内仲裁，同帧申请用业务全序键决胜，申请方经 inst ref 收到同一个结果。

use pce::predicate::{inst, lit, new_path, own_field, type_scope};
use pce::{
    CmpOp, Cond, Delivery, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, ValRef, Value,
};

fn as_i64(v: &Value) -> i64 {
    v.as_f64().unwrap_or(0.0) as i64
}

#[derive(Clone, Copy)]
struct W {
    unit_ty: EntityTypeId,
    item_ty: EntityTypeId,
    claim: FieldId,
    want: FieldId,
    result: FieldId,
    owner: FieldId,
}

fn setup() -> (Runtime, W) {
    let mut rt = Runtime::new();
    let unit_ty = rt.register_entity_type(
        "Unit",
        vec![
            FieldDef::new("claim", Value::Null),
            FieldDef::reference("want"),
            FieldDef::new("result", Value::str("pending")),
        ],
        false,
    );
    let item_ty =
        rt.register_entity_type("Item", vec![FieldDef::reference("owner")], false);
    let w = W {
        unit_ty,
        item_ty,
        claim: rt.field(unit_ty, "claim"),
        want: rt.field(unit_ty, "want"),
        result: rt.field(unit_ty, "result"),
        owner: rt.field(item_ty, "owner"),
    };

    let owner_f = w.owner;
    rt.register_calculation(
        "grant",
        item_ty,
        Predicate::new(
            type_scope(unit_ty, w.claim),
            Cond::And(
                Box::new(Cond::Cmp(new_path(&["item"]), CmpOp::Eq, Expr::Val(ValRef::SelfRef))),
                Box::new(Cond::Cmp(own_field(owner_f), CmpOp::Eq, lit(Value::Null))),
            ),
            Delivery::Batch(vec![
                Proj::WriterId,
                Proj::New(vec!["prio".to_string()]),
                Proj::New(vec!["salt".to_string()]),
            ]),
        ),
        &[owner_f],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let mut best: Option<(i64, i64, Value)> = None;
            for row in rows {
                let key = (as_i64(&row[1]), as_i64(&row[2]));
                if best.as_ref().map_or(true, |(p, s, _)| key > (*p, *s)) {
                    best = Some((key.0, key.1, row[0].clone()));
                }
            }
            if let Some((_, _, winner)) = best {
                ctx.write(owner_f, winner);
            }
        }),
    )
    .unwrap();

    let (result_f, want_f) = (w.result, w.want);
    rt.register_calculation(
        "claim_result",
        unit_ty,
        Predicate::new(
            inst(want_f, owner_f),
            Cond::True,
            Delivery::Each(vec![Proj::New(vec![])]),
        ),
        &[result_f, want_f],
        Box::new(move |ctx, input| {
            let Input::Each(row) = input else { return };
            let won = row[0] == Value::Ref(ctx.self_id());
            ctx.write(result_f, Value::str(if won { "won" } else { "lost" }));
            ctx.write(want_f, Value::Null);
        }),
    )
    .unwrap();

    (rt, w)
}

fn claim(rt: &mut Runtime, unit: InstanceId, item: InstanceId, w: W, prio: i64, salt: i64) {
    rt.debug_write(unit, w.want, Value::Ref(item));
    rt.debug_write(
        unit,
        w.claim,
        Value::map([
            ("item", Value::Ref(item)),
            ("prio", Value::Int(prio)),
            ("salt", Value::Int(salt)),
        ]),
    );
}

#[test]
fn same_frame_claims_choose_one_winner_and_broadcast_result() {
    let (mut rt, w) = setup();
    let item = rt.spawn(w.item_ty, vec![]);
    let low = rt.spawn(w.unit_ty, vec![]);
    let tied_low_salt = rt.spawn(w.unit_ty, vec![]);
    let winner = rt.spawn(w.unit_ty, vec![]);

    claim(&mut rt, low, item, w, 1, 99);
    claim(&mut rt, tied_low_salt, item, w, 2, 1);
    claim(&mut rt, winner, item, w, 2, 5);

    rt.step();
    assert_eq!(rt.read(item, w.owner), Value::Ref(winner));
    assert_eq!(rt.read(winner, w.result), Value::str("pending"));

    rt.step();
    assert_eq!(rt.read(low, w.result), Value::str("lost"));
    assert_eq!(rt.read(tied_low_salt, w.result), Value::str("lost"));
    assert_eq!(rt.read(winner, w.result), Value::str("won"));
    assert_eq!(rt.read(low, w.want), Value::Null);
    assert_eq!(rt.read(tied_low_salt, w.want), Value::Null);
    assert_eq!(rt.read(winner, w.want), Value::Null);

    claim(&mut rt, low, item, w, 99, 99);
    rt.step();
    assert_eq!(rt.read(item, w.owner), Value::Ref(winner));
}
