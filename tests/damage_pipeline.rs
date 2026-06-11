//! Runnable checks for docs/10-damage-pipeline.md.

use pce::predicate::{new_path, type_scope};
use pce::{
    CmpOp, Cond, Delivery, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, Scope, ValRef, Value,
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

fn as_bool(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

#[derive(Clone, Copy)]
struct F {
    unit_ty: EntityTypeId,
    hp: FieldId,
    shield: FieldId,
    attack_out: FieldId,
}

fn setup() -> (Runtime, F) {
    let mut rt = Runtime::new();
    let unit_ty = rt.register_entity_type(
        "Unit",
        vec![
            FieldDef::new("hp", Value::Int(100)),
            FieldDef::new("shield", Value::Int(0)),
            FieldDef::new("armor_pct", Value::Int(0)),
            FieldDef::new("vuln_pct", Value::Int(0)),
            FieldDef::new("mit_pct", Value::Int(0)),
            FieldDef::new("attack_out", Value::Null),
            FieldDef::new("reflect_out", Value::Null),
        ],
        false,
    );
    let f = F {
        unit_ty,
        hp: rt.field(unit_ty, "hp"),
        shield: rt.field(unit_ty, "shield"),
        attack_out: rt.field(unit_ty, "attack_out"),
    };
    let armor = rt.field(unit_ty, "armor_pct");
    let vuln = rt.field(unit_ty, "vuln_pct");
    let mit = rt.field(unit_ty, "mit_pct");
    let reflect_out = rt.field(unit_ty, "reflect_out");

    let (hp_f, shield_f) = (f.hp, f.shield);
    rt.register_calculation(
        "settle",
        unit_ty,
        Predicate::new(
            scope_or(vec![
                type_scope(unit_ty, f.attack_out),
                type_scope(unit_ty, reflect_out),
            ]),
            target_is_self(),
            Delivery::Batch(vec![Proj::New(vec![]), Proj::WriterId]),
        ),
        &[hp_f, shield_f, reflect_out],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let mut blockable = 0;
            let mut true_damage = 0;
            let mut reflected_from: Option<Value> = None;
            let mut reflect_basis = 0;

            for row in rows {
                let hit = &row[0];
                let mut eff = as_i64(&path(hit, "raw"));
                eff = eff * (100 - as_i64(&ctx.read_own(armor))) / 100;
                eff = eff * (100 + as_i64(&ctx.read_own(vuln)) - as_i64(&ctx.read_own(mit))) / 100;

                if as_bool(&path(hit, "true")) {
                    true_damage += eff;
                } else {
                    blockable += eff;
                }
                if !as_bool(&path(hit, "reflected")) {
                    reflected_from.get_or_insert_with(|| row[1].clone());
                    reflect_basis += eff;
                }
            }

            let shield = as_i64(&ctx.read_own(shield_f));
            let absorbed = shield.min(blockable);
            let overflow = blockable - absorbed;
            ctx.write(shield_f, Value::Int(shield - absorbed));
            ctx.write(
                hp_f,
                Value::Int(as_i64(&ctx.read_own(hp_f)) - overflow - true_damage),
            );

            let reflected = reflect_basis / 10;
            if reflected > 0 {
                if let Some(target) = reflected_from {
                    ctx.write(
                        reflect_out,
                        Value::map([
                            ("target", target),
                            ("raw", Value::Int(reflected)),
                            ("true", Value::Bool(false)),
                            ("reflected", Value::Bool(true)),
                            ("salt", Value::str("reflect")),
                        ]),
                    );
                }
            }
        }),
    )
    .unwrap();

    (rt, f)
}

fn hit(target: InstanceId, raw: i64, true_damage: bool, reflected: bool, salt: &str) -> Value {
    Value::map([
        ("target", Value::Ref(target)),
        ("raw", Value::Int(raw)),
        ("true", Value::Bool(true_damage)),
        ("reflected", Value::Bool(reflected)),
        ("salt", Value::str(salt)),
    ])
}

fn hp(rt: &Runtime, unit: InstanceId, f: F) -> i64 {
    as_i64(&rt.read(unit, f.hp))
}

fn shield(rt: &Runtime, unit: InstanceId, f: F) -> i64 {
    as_i64(&rt.read(unit, f.shield))
}

#[test]
fn batch_settle_absorbs_shield_once_true_damage_pierces_and_reflect_stops() {
    let (mut rt, f) = setup();
    let attacker = rt.spawn(f.unit_ty, vec![]);
    let defender = rt.spawn(f.unit_ty, vec![(f.shield, Value::Int(10))]);

    rt.debug_write(attacker, f.attack_out, hit(defender, 8, false, false, "a"));
    rt.debug_write(attacker, f.attack_out, hit(defender, 7, false, false, "b"));
    rt.debug_write(attacker, f.attack_out, hit(defender, 5, true, false, "c"));
    rt.step();

    assert_eq!(shield(&rt, defender, f), 0);
    assert_eq!(hp(&rt, defender, f), 90);
    assert_eq!(hp(&rt, attacker, f), 100);

    rt.step();
    assert_eq!(hp(&rt, attacker, f), 98);

    rt.step();
    assert_eq!(hp(&rt, defender, f), 90);
    assert_eq!(hp(&rt, attacker, f), 98);
}
