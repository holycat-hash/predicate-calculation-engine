//! Executable checks for docs/20-projectile.md:
//! projectile hits are settled in one batch by geometry time, not delivery
//! order; duplicate targets are ignored and flattened credit survives death.

use std::collections::BTreeMap;

use pce::predicate::{new_path, type_scope};
use pce::{
    CmpOp, Cond, Delivery, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, ValRef, Value,
};

fn proj_is_self() -> Cond {
    Cond::Cmp(new_path(&["proj"]), CmpOp::Eq, Expr::Val(ValRef::SelfRef))
}

fn target_is_self() -> Cond {
    Cond::Cmp(new_path(&["target"]), CmpOp::Eq, Expr::Val(ValRef::SelfRef))
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

fn ref_key(v: &Value) -> String {
    match v {
        Value::Ref(inst) => format!("{}:{}", inst.ty.0, inst.id),
        _ => "null".to_string(),
    }
}

#[derive(Clone, Copy)]
struct F {
    unit_ty: EntityTypeId,
    projectile_ty: EntityTypeId,
    cand_ty: EntityTypeId,
    target_ty: EntityTypeId,
    hit_ty: EntityTypeId,
    pierce_left: FieldId,
    hit_set: FieldId,
    cred: FieldId,
    cand_hit: FieldId,
    hit: FieldId,
    hit_count: FieldId,
    last_owner: FieldId,
}

fn setup() -> (Runtime, F) {
    let mut rt = Runtime::new();
    let unit_ty = rt.register_entity_type("Unit", vec![FieldDef::new("name", Value::Null)], false);
    let projectile_ty = rt.register_entity_type(
        "Projectile",
        vec![
            FieldDef::new("pierce_left", Value::Int(3)),
            FieldDef::new("hit_set", Value::Map(BTreeMap::new())),
            FieldDef::new("cred", Value::Null),
        ],
        false,
    );
    let cand_ty =
        rt.register_entity_type("HitCand", vec![FieldDef::new("hit", Value::Null)], false);
    let target_ty = rt.register_entity_type(
        "Target",
        vec![
            FieldDef::new("hit_count", Value::Int(0)),
            FieldDef::new("last_owner", Value::Null),
        ],
        false,
    );
    let hit_ty = rt.register_entity_type("Hit", vec![FieldDef::new("hit", Value::Null)], false);
    let f = F {
        unit_ty,
        projectile_ty,
        cand_ty,
        target_ty,
        hit_ty,
        pierce_left: rt.field(projectile_ty, "pierce_left"),
        hit_set: rt.field(projectile_ty, "hit_set"),
        cred: rt.field(projectile_ty, "cred"),
        cand_hit: rt.field(cand_ty, "hit"),
        hit: rt.field(hit_ty, "hit"),
        hit_count: rt.field(target_ty, "hit_count"),
        last_owner: rt.field(target_ty, "last_owner"),
    };

    rt.register_calculation(
        "projectile_settle",
        projectile_ty,
        Predicate::new(
            type_scope(cand_ty, f.cand_hit),
            proj_is_self(),
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[f.pierce_left, f.hit_set, pce::entity::FIELD_ALIVE],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let mut hits: Vec<Value> = rows.iter().map(|r| r[0].clone()).collect();
            hits.sort_by_key(|v| (as_i64(&path(v, "t")), as_str(&path(v, "salt"))));

            let mut left = as_i64(&ctx.read_own(f.pierce_left));
            let mut seen = map_of(&ctx.read_own(f.hit_set));
            let cred = ctx.read_own(f.cred);
            for hit in hits {
                if left <= 0 {
                    break;
                }
                let target = path(&hit, "target");
                let key = ref_key(&target);
                if seen.contains_key(&key) {
                    continue;
                }
                seen.insert(key, Value::Bool(true));
                left -= 1;
                ctx.spawn(
                    f.hit_ty,
                    vec![(
                        f.hit,
                        Value::map([
                            ("target", target),
                            ("cred", cred.clone()),
                            ("t", path(&hit, "t")),
                            ("salt", path(&hit, "salt")),
                        ]),
                    )],
                );
            }
            ctx.write(f.pierce_left, Value::Int(left));
            ctx.write(f.hit_set, Value::Map(seen));
            if left <= 0 {
                ctx.destroy_self();
            }
        }),
    )
    .unwrap();

    rt.register_calculation(
        "target_apply",
        target_ty,
        Predicate::new(
            type_scope(hit_ty, f.hit),
            target_is_self(),
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[f.hit_count, f.last_owner],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let count = as_i64(&ctx.read_own(f.hit_count)) + rows.len() as i64;
            let owner = rows
                .iter()
                .map(|r| path(&path(&r[0], "cred"), "owner"))
                .max_by_key(as_str)
                .unwrap_or(Value::Null);
            ctx.write(f.hit_count, Value::Int(count));
            ctx.write(f.last_owner, owner);
        }),
    )
    .unwrap();

    (rt, f)
}

fn cand(proj: InstanceId, target: InstanceId, t: i64, salt: &str) -> Value {
    Value::map([
        ("proj", Value::Ref(proj)),
        ("target", Value::Ref(target)),
        ("t", Value::Int(t)),
        ("salt", Value::str(salt)),
    ])
}

#[test]
fn pierce_budget_uses_sweep_time_and_deduplicates_targets() {
    let (mut rt, f) = setup();
    let shooter = rt.spawn(
        f.unit_ty,
        vec![(rt.field(f.unit_ty, "name"), Value::str("archer"))],
    );
    let projectile = rt.spawn(
        f.projectile_ty,
        vec![(f.cred, Value::map([("owner", Value::str("archer"))]))],
    );
    let t1 = rt.spawn(f.target_ty, vec![]);
    let t2 = rt.spawn(f.target_ty, vec![]);
    let t3 = rt.spawn(f.target_ty, vec![]);
    let t4 = rt.spawn(f.target_ty, vec![]);

    rt.destroy(shooter);
    rt.spawn(
        f.cand_ty,
        vec![(f.cand_hit, cand(projectile, t4, 40, "late"))],
    );
    rt.spawn(
        f.cand_ty,
        vec![(f.cand_hit, cand(projectile, t1, 10, "first"))],
    );
    rt.spawn(
        f.cand_ty,
        vec![(f.cand_hit, cand(projectile, t1, 15, "dupe"))],
    );
    rt.spawn(
        f.cand_ty,
        vec![(f.cand_hit, cand(projectile, t3, 30, "third"))],
    );
    rt.spawn(
        f.cand_ty,
        vec![(f.cand_hit, cand(projectile, t2, 20, "second"))],
    );

    rt.step();
    rt.step();

    assert_eq!(rt.read(t1, f.hit_count), Value::Int(1));
    assert_eq!(rt.read(t2, f.hit_count), Value::Int(1));
    assert_eq!(rt.read(t3, f.hit_count), Value::Int(1));
    assert_eq!(rt.read(t4, f.hit_count), Value::Int(0));
    assert_eq!(rt.read(t1, f.last_owner), Value::str("archer"));
}
