//! Runnable checks for docs/11-immunity-invincibility.md.

use pce::predicate::{lit, new_path, own_field, type_scope};
use pce::{
    CmpOp, Cond, Delivery, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, ValRef, Value,
};

const IFRAMES: i64 = 30;

fn target_is_self() -> Cond {
    Cond::Cmp(new_path(&["target"]), CmpOp::Eq, Expr::Val(ValRef::SelfRef))
}

fn and(a: Cond, b: Cond) -> Cond {
    Cond::And(Box::new(a), Box::new(b))
}

fn or(a: Cond, b: Cond) -> Cond {
    Cond::Or(Box::new(a), Box::new(b))
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
    attack_out: FieldId,
    hp: FieldId,
    immune_until: FieldId,
    immune_all: FieldId,
    blocked_fx: FieldId,
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
            FieldDef::new("hp", Value::Int(100)),
            FieldDef::new("immune_until", Value::Int(0)),
            FieldDef::new("immune_all", Value::Bool(false)),
            FieldDef::new("blocked_fx", Value::Int(0)),
        ],
        false,
    );
    let f = F {
        attacker_ty,
        unit_ty,
        attack_out: rt.field(attacker_ty, "attack_out"),
        hp: rt.field(unit_ty, "hp"),
        immune_until: rt.field(unit_ty, "immune_until"),
        immune_all: rt.field(unit_ty, "immune_all"),
        blocked_fx: rt.field(unit_ty, "blocked_fx"),
    };

    let accept_guard = and(
        and(
            target_is_self(),
            Cond::Cmp(new_path(&["frame"]), CmpOp::Ge, own_field(f.immune_until)),
        ),
        and(
            Cond::Cmp(own_field(f.immune_all), CmpOp::Ne, lit(Value::Bool(true))),
            Cond::Cmp(new_path(&["kind"]), CmpOp::Ne, lit(Value::str("poison"))),
        ),
    );
    let (hp_f, until_f) = (f.hp, f.immune_until);
    rt.register_calculation(
        "settle",
        unit_ty,
        Predicate::new(
            type_scope(attacker_ty, f.attack_out),
            accept_guard,
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[hp_f, until_f],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let damage: i64 = rows.iter().map(|r| as_i64(&path(&r[0], "dmg"))).sum();
            let frame = rows
                .iter()
                .map(|r| as_i64(&path(&r[0], "frame")))
                .max()
                .unwrap_or(0);
            ctx.write(hp_f, Value::Int(as_i64(&ctx.read_own(hp_f)) - damage));
            ctx.write(until_f, Value::Int(frame + IFRAMES));
        }),
    )
    .unwrap();

    let blocked_guard = and(
        target_is_self(),
        or(
            or(
                Cond::Cmp(new_path(&["frame"]), CmpOp::Lt, own_field(f.immune_until)),
                Cond::Cmp(own_field(f.immune_all), CmpOp::Eq, lit(Value::Bool(true))),
            ),
            Cond::Cmp(new_path(&["kind"]), CmpOp::Eq, lit(Value::str("poison"))),
        ),
    );
    let blocked_f = f.blocked_fx;
    rt.register_calculation(
        "immune_fx",
        unit_ty,
        Predicate::new(
            type_scope(attacker_ty, f.attack_out),
            blocked_guard,
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[blocked_f],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            ctx.write(
                blocked_f,
                Value::Int(as_i64(&ctx.read_own(blocked_f)) + rows.len() as i64),
            );
        }),
    )
    .unwrap();

    (rt, f)
}

fn attack(target: InstanceId, dmg: i64, frame: i64, kind: &str, salt: &str) -> Value {
    Value::map([
        ("target", Value::Ref(target)),
        ("dmg", Value::Int(dmg)),
        ("frame", Value::Int(frame)),
        ("kind", Value::str(kind)),
        ("salt", Value::str(salt)),
    ])
}

#[test]
fn complementary_guards_settle_hits_or_emit_immunity_fx() {
    let (mut rt, f) = setup();
    let attacker = rt.spawn(f.attacker_ty, vec![]);
    let unit = rt.spawn(f.unit_ty, vec![]);
    let frame = rt.frame() as i64;

    rt.debug_write(
        attacker,
        f.attack_out,
        attack(unit, 10, frame, "slash", "a"),
    );
    rt.debug_write(
        attacker,
        f.attack_out,
        attack(unit, 15, frame, "slash", "b"),
    );
    rt.step();
    assert_eq!(as_i64(&rt.read(unit, f.hp)), 75);
    assert_eq!(as_i64(&rt.read(unit, f.immune_until)), frame + IFRAMES);
    assert_eq!(as_i64(&rt.read(unit, f.blocked_fx)), 0);

    rt.debug_write(
        attacker,
        f.attack_out,
        attack(unit, 99, frame + 1, "slash", "c"),
    );
    rt.step();
    assert_eq!(as_i64(&rt.read(unit, f.hp)), 75);
    assert_eq!(as_i64(&rt.read(unit, f.blocked_fx)), 1);

    rt.debug_write(
        attacker,
        f.attack_out,
        attack(unit, 99, frame + 100, "poison", "d"),
    );
    rt.step();
    assert_eq!(as_i64(&rt.read(unit, f.hp)), 75);
    assert_eq!(as_i64(&rt.read(unit, f.blocked_fx)), 2);

    rt.debug_write(unit, f.immune_all, Value::Bool(true));
    rt.debug_write(
        attacker,
        f.attack_out,
        attack(unit, 99, frame + 100, "slash", "e"),
    );
    rt.step();
    assert_eq!(as_i64(&rt.read(unit, f.hp)), 75);
    assert_eq!(as_i64(&rt.read(unit, f.blocked_fx)), 3);
}
