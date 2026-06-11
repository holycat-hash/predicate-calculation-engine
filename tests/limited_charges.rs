//! Runnable checks for docs/12-limited-charges.md.

use pce::predicate::{lit, new_path, own, type_scope};
use pce::{
    CmpOp, Cond, Delivery, Dir, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId,
    Predicate, Proj, Runtime, ValRef, Value,
};

fn target_is_self() -> Cond {
    Cond::Cmp(new_path(&["target"]), CmpOp::Eq, Expr::Val(ValRef::SelfRef))
}

fn path(v: &Value, key: &str) -> Value {
    v.get_path(&[key.to_string()])
}

fn as_i64(v: &Value) -> i64 {
    v.as_f64().unwrap_or(0.0) as i64
}

#[derive(Clone, Copy)]
struct F {
    attacker_ty: EntityTypeId,
    unit_ty: EntityTypeId,
    collector_ty: EntityTypeId,
    attack_out: FieldId,
    hp: FieldId,
    charge: FieldId,
    hit_count: FieldId,
    empower_next: FieldId,
    ward_fx: FieldId,
    rattle_count: FieldId,
}

fn setup() -> (Runtime, F) {
    let mut rt = Runtime::new();
    let attacker_ty = rt.register_entity_type(
        "Attacker",
        vec![FieldDef::new("attack_out", Value::Null)],
        false,
    );
    let unit_ty = rt.register_entity_type(
        "Unit",
        vec![
            FieldDef::new("hp", Value::Int(10)),
            FieldDef::new("charge", Value::Int(1)),
            FieldDef::new("hit_count", Value::Int(0)),
            FieldDef::new("empower_next", Value::Bool(false)),
            FieldDef::new("ward_fx", Value::Int(0)),
        ],
        false,
    );
    let rattle_ty =
        rt.register_entity_type("Rattle", vec![FieldDef::new("payload", Value::Null)], false);
    let collector_ty = rt.register_entity_type(
        "Collector",
        vec![FieldDef::new("rattle_count", Value::Int(0))],
        false,
    );

    let f = F {
        attacker_ty,
        unit_ty,
        collector_ty,
        attack_out: rt.field(attacker_ty, "attack_out"),
        hp: rt.field(unit_ty, "hp"),
        charge: rt.field(unit_ty, "charge"),
        hit_count: rt.field(unit_ty, "hit_count"),
        empower_next: rt.field(unit_ty, "empower_next"),
        ward_fx: rt.field(unit_ty, "ward_fx"),
        rattle_count: rt.field(collector_ty, "rattle_count"),
    };
    let rattle_payload = rt.field(rattle_ty, "payload");

    let (hp_f, charge_f, hit_count_f, empower_f, ward_f) =
        (f.hp, f.charge, f.hit_count, f.empower_next, f.ward_fx);
    rt.register_calculation(
        "settle",
        unit_ty,
        Predicate::new(
            type_scope(attacker_ty, f.attack_out),
            target_is_self(),
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[hp_f, charge_f, hit_count_f, empower_f, ward_f],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let total: i64 = rows.iter().map(|r| as_i64(&path(&r[0], "dmg"))).sum();
            let old_hits = as_i64(&ctx.read_own(hit_count_f));
            let new_hits = old_hits + rows.len() as i64;
            let mut hp = as_i64(&ctx.read_own(hp_f));
            let mut charge = as_i64(&ctx.read_own(charge_f));
            let mut ward = as_i64(&ctx.read_own(ward_f));

            if hp - total <= 0 && charge > 0 {
                charge -= 1;
                hp = 1;
                ward += 1;
            } else {
                hp -= total;
            }

            let already_empowered = matches!(ctx.read_own(empower_f), Value::Bool(true));
            let crosses_fifth = old_hits / 5 < new_hits / 5;
            ctx.write(hp_f, Value::Int(hp));
            ctx.write(charge_f, Value::Int(charge));
            ctx.write(hit_count_f, Value::Int(new_hits));
            ctx.write(empower_f, Value::Bool(already_empowered || crosses_fifth));
            ctx.write(ward_f, Value::Int(ward));
        }),
    )
    .unwrap();

    rt.register_calculation(
        "deathrattle",
        unit_ty,
        Predicate::new(
            own(f.hp),
            Cond::Crossed(lit(Value::Int(0)), Dir::Down),
            Delivery::Each(vec![]),
        ),
        &[],
        Box::new(move |ctx, _| {
            ctx.spawn(
                rattle_ty,
                vec![(
                    rattle_payload,
                    Value::map([
                        ("source", Value::Ref(ctx.self_id())),
                        ("kind", Value::str("rattle")),
                    ]),
                )],
            );
            ctx.destroy_self();
        }),
    )
    .unwrap();

    let count_f = f.rattle_count;
    rt.register_calculation(
        "collect_rattle",
        collector_ty,
        Predicate::new(
            type_scope(rattle_ty, rattle_payload),
            Cond::True,
            Delivery::Batch(vec![]),
        ),
        &[count_f],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            ctx.write(
                count_f,
                Value::Int(as_i64(&ctx.read_own(count_f)) + rows.len() as i64),
            );
        }),
    )
    .unwrap();

    (rt, f)
}

fn hit(target: InstanceId, dmg: i64, salt: &str) -> Value {
    Value::map([
        ("target", Value::Ref(target)),
        ("dmg", Value::Int(dmg)),
        ("salt", Value::str(salt)),
    ])
}

#[test]
fn batch_charges_prevent_same_frame_overconsume_and_deathrattle_once() {
    let (mut rt, f) = setup();
    let attacker = rt.spawn(f.attacker_ty, vec![]);
    let unit = rt.spawn(f.unit_ty, vec![]);
    let collector = rt.spawn(f.collector_ty, vec![]);

    for i in 0..5 {
        rt.debug_write(attacker, f.attack_out, hit(unit, 4, &format!("h{i}")));
    }
    rt.step();

    assert_eq!(as_i64(&rt.read(unit, f.hp)), 1);
    assert_eq!(as_i64(&rt.read(unit, f.charge)), 0);
    assert_eq!(as_i64(&rt.read(unit, f.hit_count)), 5);
    assert_eq!(rt.read(unit, f.empower_next), Value::Bool(true));
    assert_eq!(as_i64(&rt.read(unit, f.ward_fx)), 1);

    rt.debug_write(attacker, f.attack_out, hit(unit, 2, "lethal"));
    rt.step();
    assert_eq!(as_i64(&rt.read(unit, f.hp)), -1);

    rt.step();
    rt.step();
    assert_eq!(as_i64(&rt.read(collector, f.rattle_count)), 1);

    for _ in 0..3 {
        rt.step();
    }
    assert_eq!(as_i64(&rt.read(collector, f.rattle_count)), 1);
}
