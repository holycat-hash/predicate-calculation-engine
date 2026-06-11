//! Executable checks for docs/15-summon-attribution.md:
//! attacks carry a flattened root-owner snapshot, so a later tame/transfer does
//! not rewrite in-flight attribution.

use pce::predicate::{new_path, type_scope};
use pce::{
    CmpOp, Cond, Delivery, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, ValRef, Value,
};

fn target_is_self() -> Cond {
    Cond::Cmp(new_path(&["target"]), CmpOp::Eq, Expr::Val(ValRef::SelfRef))
}

fn beneficiary_is_self() -> Cond {
    Cond::Cmp(
        new_path(&["beneficiary"]),
        CmpOp::Eq,
        Expr::Val(ValRef::SelfRef),
    )
}

fn path(v: &Value, key: &str) -> Value {
    v.get_path(&[key.to_string()])
}

fn as_i64(v: &Value) -> i64 {
    v.as_f64().unwrap_or(0.0) as i64
}

#[derive(Clone, Copy)]
struct F {
    unit_ty: EntityTypeId,
    owner: FieldId,
    root_owner: FieldId,
    tame_out: FieldId,
    attack_out: FieldId,
    hp: FieldId,
    xp: FieldId,
}

fn setup() -> (Runtime, F) {
    let mut rt = Runtime::new();
    let unit_ty = rt.register_entity_type(
        "Unit",
        vec![
            FieldDef::reference("owner"),
            FieldDef::reference("root_owner"),
            FieldDef::new("tame_out", Value::Null),
            FieldDef::new("attack_out", Value::Null),
            FieldDef::new("hp", Value::Int(100)),
            FieldDef::new("xp", Value::Int(0)),
        ],
        false,
    );
    let credit_ty = rt.register_entity_type(
        "KillCredit",
        vec![FieldDef::new("grant", Value::Null)],
        false,
    );
    let grant_f = rt.field(credit_ty, "grant");
    let f = F {
        unit_ty,
        owner: rt.field(unit_ty, "owner"),
        root_owner: rt.field(unit_ty, "root_owner"),
        tame_out: rt.field(unit_ty, "tame_out"),
        attack_out: rt.field(unit_ty, "attack_out"),
        hp: rt.field(unit_ty, "hp"),
        xp: rt.field(unit_ty, "xp"),
    };

    rt.register_calculation(
        "lineage",
        unit_ty,
        Predicate::new(
            type_scope(unit_ty, f.tame_out),
            target_is_self(),
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[f.owner, f.root_owner],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let Some(op) = rows
                .iter()
                .map(|r| &r[0])
                .max_by_key(|v| as_i64(&path(v, "seq")))
            else {
                return;
            };
            ctx.write(f.owner, path(op, "owner"));
            ctx.write(f.root_owner, path(op, "root"));
        }),
    )
    .unwrap();

    rt.register_calculation(
        "settle",
        unit_ty,
        Predicate::new(
            type_scope(unit_ty, f.attack_out),
            target_is_self(),
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[f.hp, pce::entity::FIELD_ALIVE],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let mut hp = as_i64(&ctx.read_own(f.hp));
            let mut best: Option<Value> = None;
            for row in rows {
                let hit = &row[0];
                hp -= as_i64(&path(hit, "dmg"));
                if best.as_ref().is_none_or(|cur| {
                    (as_i64(&path(hit, "dmg")), as_i64(&path(hit, "salt")))
                        > (as_i64(&path(cur, "dmg")), as_i64(&path(cur, "salt")))
                }) {
                    best = Some(hit.clone());
                }
            }
            if hp <= 0 {
                let killer_root = best
                    .map(|v| path(&v, "attacker_root"))
                    .unwrap_or(Value::Null);
                ctx.spawn(
                    credit_ty,
                    vec![(
                        grant_f,
                        Value::map([
                            ("beneficiary", killer_root),
                            ("kind", Value::str("xp")),
                            ("amount", Value::Int(1)),
                        ]),
                    )],
                );
                ctx.destroy_self();
            }
            ctx.write(f.hp, Value::Int(hp));
        }),
    )
    .unwrap();

    rt.register_calculation(
        "credit",
        unit_ty,
        Predicate::new(
            type_scope(credit_ty, grant_f),
            beneficiary_is_self(),
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[f.xp],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let gained: i64 = rows.iter().map(|r| as_i64(&path(&r[0], "amount"))).sum();
            ctx.write(f.xp, Value::Int(as_i64(&ctx.read_own(f.xp)) + gained));
        }),
    )
    .unwrap();

    (rt, f)
}

fn tame(target: InstanceId, owner: InstanceId, root: InstanceId, seq: i64) -> Value {
    Value::map([
        ("target", Value::Ref(target)),
        ("owner", Value::Ref(owner)),
        ("root", Value::Ref(root)),
        ("seq", Value::Int(seq)),
    ])
}

fn attack(target: InstanceId, attacker_root: Value, dmg: i64, salt: i64) -> Value {
    Value::map([
        ("target", Value::Ref(target)),
        ("attacker_root", attacker_root),
        ("dmg", Value::Int(dmg)),
        ("salt", Value::Int(salt)),
    ])
}

#[test]
fn in_flight_attack_keeps_old_root_after_tame_transfer() {
    let (mut rt, f) = setup();
    let old_owner = rt.spawn(f.unit_ty, vec![]);
    let new_owner = rt.spawn(f.unit_ty, vec![]);
    let pet = rt.spawn(
        f.unit_ty,
        vec![
            (f.owner, Value::Ref(old_owner)),
            (f.root_owner, Value::Ref(old_owner)),
        ],
    );
    let victim = rt.spawn(f.unit_ty, vec![(f.hp, Value::Int(50))]);

    let frozen_root = rt.read(pet, f.root_owner);
    rt.debug_write(victim, f.attack_out, attack(victim, frozen_root, 100, 1));
    rt.debug_write(new_owner, f.tame_out, tame(pet, new_owner, new_owner, 2));

    rt.step();
    assert_eq!(rt.read(pet, f.root_owner), Value::Ref(new_owner));

    rt.step();
    assert_eq!(rt.read(old_owner, f.xp), Value::Int(1));
    assert_eq!(rt.read(new_owner, f.xp), Value::Int(0));
}
