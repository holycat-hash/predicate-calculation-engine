//! Runnable checks for docs/09-buff-debuff-ledger.md.

use std::collections::BTreeMap;

use pce::predicate::{new_path, new_val, own, own_field, type_scope};
use pce::{
    CmpOp, Cond, Delivery, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, Scope, ValRef, Value,
};

const INF: i64 = 1 << 40;
const STACK_CAP: i64 = 3;

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

fn as_f64(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
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
struct F {
    caster_ty: EntityTypeId,
    unit_ty: EntityTypeId,
    buff_op_out: FieldId,
    buff_book: FieldId,
    atk_final: FieldId,
    next_expire: FieldId,
}

fn setup() -> (Runtime, F) {
    let mut rt = Runtime::new();
    let caster_ty = rt.register_entity_type(
        "Caster",
        vec![FieldDef::new("buff_op_out", Value::Null)],
        false,
    );
    let unit_ty = rt.register_entity_type(
        "Unit",
        vec![
            FieldDef::new("base_atk", Value::Int(100)),
            FieldDef::new("buff_book", Value::Map(BTreeMap::new())),
            FieldDef::new("atk_final", Value::Float(100.0)),
            FieldDef::new("next_expire", Value::Int(INF)),
            FieldDef::new("expiry_op", Value::Null),
        ],
        false,
    );

    let f = F {
        caster_ty,
        unit_ty,
        buff_op_out: rt.field(caster_ty, "buff_op_out"),
        buff_book: rt.field(unit_ty, "buff_book"),
        atk_final: rt.field(unit_ty, "atk_final"),
        next_expire: rt.field(unit_ty, "next_expire"),
    };
    let base_atk = rt.field(unit_ty, "base_atk");
    let expiry_op = rt.field(unit_ty, "expiry_op");

    let clock_ty = rt.clock().ty;
    let clock_frame = rt.clock().f_frame;
    rt.register_calculation(
        "expiry_probe",
        unit_ty,
        Predicate::new(
            type_scope(clock_ty, clock_frame),
            Cond::Cmp(new_val(), CmpOp::Ge, own_field(f.next_expire)),
            Delivery::Each(vec![Proj::New(vec![])]),
        ),
        &[expiry_op],
        Box::new(move |ctx, input| {
            let Input::Each(row) = input else { return };
            ctx.write(
                expiry_op,
                Value::map([
                    ("target", Value::Ref(ctx.self_id())),
                    ("op", Value::str("expire")),
                    ("frame", row[0].clone()),
                ]),
            );
        }),
    )
    .unwrap();

    let (book_f, atk_f, next_f) = (f.buff_book, f.atk_final, f.next_expire);
    rt.register_calculation(
        "book",
        unit_ty,
        Predicate::new(
            scope_or(vec![type_scope(caster_ty, f.buff_op_out), own(expiry_op)]),
            target_is_self(),
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[book_f, atk_f, next_f],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let mut book = map_of(&ctx.read_own(book_f));
            let mut applies = vec![];
            let mut expires = vec![];
            let mut dispels = vec![];

            for row in rows {
                let op = row[0].clone();
                match as_str(&path(&op, "op")).as_str() {
                    "apply" => applies.push(op),
                    "expire" => expires.push(as_i64(&path(&op, "frame"))),
                    "dispel" => dispels.push(as_str(&path(&op, "tag"))),
                    _ => {}
                }
            }

            for op in applies {
                let kind = as_str(&path(&op, "kind"));
                let mut entry = book.get(&kind).map(map_of).unwrap_or_default();
                let old_stacks = entry.get("stacks").map(as_i64).unwrap_or(0);
                let stacks = (old_stacks + as_i64(&path(&op, "stacks"))).min(STACK_CAP);
                let until = entry
                    .get("until")
                    .map(as_i64)
                    .unwrap_or(0)
                    .max(as_i64(&path(&op, "frame")) + as_i64(&path(&op, "dur")));
                entry.insert("add".to_string(), path(&op, "add"));
                entry.insert("mul".to_string(), path(&op, "mul"));
                entry.insert("stacks".to_string(), Value::Int(stacks));
                entry.insert("until".to_string(), Value::Int(until));
                entry.insert("tag".to_string(), path(&op, "tag"));
                book.insert(kind, Value::Map(entry));
            }

            for frame in expires {
                book.retain(|_, entry| as_i64(&path(entry, "until")) > frame);
            }
            for tag in dispels {
                book.retain(|_, entry| as_str(&path(entry, "tag")) != tag);
            }

            let mut add = 0.0;
            let mut mul = 1.0;
            let mut next = INF;
            for entry in book.values() {
                let stacks = as_i64(&path(entry, "stacks"));
                add += as_f64(&path(entry, "add")) * stacks as f64;
                mul *= as_f64(&path(entry, "mul")).powi(stacks as i32);
                next = next.min(as_i64(&path(entry, "until")));
            }
            let base = as_f64(&ctx.read_own(base_atk));
            ctx.write(book_f, Value::Map(book));
            ctx.write(atk_f, Value::Float((base + add) * mul));
            ctx.write(next_f, Value::Int(next));
        }),
    )
    .unwrap();

    (rt, f)
}

#[allow(clippy::too_many_arguments)]
fn apply(
    target: InstanceId,
    kind: &str,
    mul: f64,
    stacks: i64,
    dur: i64,
    tag: &str,
    salt: &str,
    frame: i64,
) -> Value {
    Value::map([
        ("target", Value::Ref(target)),
        ("op", Value::str("apply")),
        ("kind", Value::str(kind)),
        ("add", Value::Int(0)),
        ("mul", Value::Float(mul)),
        ("stacks", Value::Int(stacks)),
        ("dur", Value::Int(dur)),
        ("tag", Value::str(tag)),
        ("salt", Value::str(salt)),
        ("frame", Value::Int(frame)),
    ])
}

fn dispel(target: InstanceId, tag: &str, salt: &str) -> Value {
    Value::map([
        ("target", Value::Ref(target)),
        ("op", Value::str("dispel")),
        ("tag", Value::str(tag)),
        ("salt", Value::str(salt)),
    ])
}

#[test]
fn batch_book_caps_refreshes_dispels_and_expires_without_order_dependency() {
    let (mut rt, f) = setup();
    let caster = rt.spawn(f.caster_ty, vec![]);
    let unit = rt.spawn(f.unit_ty, vec![]);
    let frame = rt.frame() as i64;

    rt.debug_write(
        caster,
        f.buff_op_out,
        apply(unit, "might", 1.2, 2, 5, "magic_buff", "a", frame),
    );
    rt.debug_write(
        caster,
        f.buff_op_out,
        apply(unit, "might", 1.2, 2, 10, "magic_buff", "b", frame),
    );
    rt.debug_write(
        caster,
        f.buff_op_out,
        apply(unit, "frailty", 0.5, 1, 10, "magic_debuff", "c", frame),
    );
    rt.debug_write(caster, f.buff_op_out, dispel(unit, "magic_debuff", "d"));
    rt.step();

    let book = map_of(&rt.read(unit, f.buff_book));
    assert_eq!(book.len(), 1);
    let might = book.get("might").unwrap();
    assert_eq!(as_i64(&path(might, "stacks")), STACK_CAP);
    assert_eq!(as_i64(&path(might, "until")), frame + 10);
    assert!((as_f64(&rt.read(unit, f.atk_final)) - 172.8).abs() < 1e-9);
    assert_eq!(as_i64(&rt.read(unit, f.next_expire)), frame + 10);

    while rt.frame() < (frame + 11) as u64 {
        rt.step();
    }

    assert!(map_of(&rt.read(unit, f.buff_book)).is_empty());
    assert!((as_f64(&rt.read(unit, f.atk_final)) - 100.0).abs() < 1e-9);
    assert_eq!(as_i64(&rt.read(unit, f.next_expire)), INF);
}
