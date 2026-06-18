//! 03 帧内聚合（docs/03-frame-aggregation.md）的可运行验证：
//! 同帧多攻击/治疗必须 batch 后一次读改写，不能让 each 多次基于同一快照覆盖 hp。

use pce::predicate::{new_path, type_scope};
use pce::{
    CmpOp, Cond, Delivery, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, Scope, ValRef, Value,
};

fn as_i64(v: &Value) -> i64 {
    v.as_f64().unwrap_or(0.0) as i64
}

#[derive(Clone, Copy)]
struct W {
    unit_ty: EntityTypeId,
    attacker_ty: EntityTypeId,
    healer_ty: EntityTypeId,
    hp: FieldId,
    attack_out: FieldId,
    heal_out: FieldId,
}

fn setup() -> (Runtime, W) {
    let mut rt = Runtime::new();
    let unit_ty =
        rt.register_entity_type("Unit", vec![FieldDef::new("hp", Value::Int(100))], false);
    let attacker_ty = rt.register_entity_type(
        "Attacker",
        vec![FieldDef::new("attack_out", Value::Null)],
        false,
    );
    let healer_ty = rt.register_entity_type(
        "Healer",
        vec![FieldDef::new("heal_out", Value::Null)],
        false,
    );
    let w = W {
        unit_ty,
        attacker_ty,
        healer_ty,
        hp: rt.field(unit_ty, "hp"),
        attack_out: rt.field(attacker_ty, "attack_out"),
        heal_out: rt.field(healer_ty, "heal_out"),
    };

    let hp_f = w.hp;
    rt.register_calculation(
        "settle_hp",
        unit_ty,
        Predicate::new(
            Scope::Or(
                Box::new(type_scope(attacker_ty, w.attack_out)),
                Box::new(type_scope(healer_ty, w.heal_out)),
            ),
            Cond::Cmp(new_path(&["target"]), CmpOp::Eq, Expr::Val(ValRef::SelfRef)),
            Delivery::Batch(vec![Proj::New(vec!["dmg".to_string()])]),
        ),
        &[hp_f],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let net: i64 = rows.iter().map(|r| as_i64(&r[0])).sum();
            let hp = as_i64(&ctx.read_own(hp_f));
            ctx.write(hp_f, Value::Int((hp - net).max(0)));
        }),
    )
    .unwrap();

    (rt, w)
}

fn packet(target: InstanceId, dmg: i64) -> Value {
    Value::map([("target", Value::Ref(target)), ("dmg", Value::Int(dmg))])
}

#[test]
fn same_frame_damage_and_healing_are_summed_once_per_target() {
    let (mut rt, w) = setup();
    let u1 = rt.spawn(w.unit_ty, vec![(w.hp, Value::Int(100))]);
    let u2 = rt.spawn(w.unit_ty, vec![(w.hp, Value::Int(50))]);
    let attackers: Vec<InstanceId> = (0..6).map(|_| rt.spawn(w.attacker_ty, vec![])).collect();
    let healers: Vec<InstanceId> = (0..3).map(|_| rt.spawn(w.healer_ty, vec![])).collect();

    for (attacker, dmg) in attackers[..5].iter().zip([10, 5, 8, 4, 6]) {
        rt.debug_write(*attacker, w.attack_out, packet(u1, dmg));
    }
    rt.debug_write(healers[0], w.heal_out, packet(u1, -3));
    rt.debug_write(healers[1], w.heal_out, packet(u1, -7));

    rt.debug_write(attackers[5], w.attack_out, packet(u2, 20));
    rt.debug_write(healers[2], w.heal_out, packet(u2, -5));

    rt.step();

    assert_eq!(rt.read(u1, w.hp), Value::Int(77));
    assert_eq!(rt.read(u2, w.hp), Value::Int(35));
}
