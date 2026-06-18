//! 04 动态订阅目标与空间邻域（docs/04-dynamic-subscription.md）的可运行验证：
//! Unit 改写 my_cell ref 后，inst(my_cell, occupants) 自动改盯新 Cell，占用维护仍是 batch 集合函数。

use std::collections::BTreeMap;

use pce::predicate::{inst, new_val, own, type_scope};
use pce::{
    CmpOp, Cond, Delivery, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, ValRef, Value,
};

fn as_i64(v: &Value) -> i64 {
    v.as_f64().unwrap_or(0.0) as i64
}

fn path(v: &Value, key: &str) -> Value {
    v.get_path(&[key.to_string()])
}

fn map_of(v: &Value) -> BTreeMap<String, Value> {
    match v {
        Value::Map(m) => m.clone(),
        _ => BTreeMap::new(),
    }
}

fn pos_key(v: &Value) -> String {
    format!("{},{}", as_i64(&path(v, "x")), as_i64(&path(v, "y")))
}

fn writer_key(v: &Value) -> String {
    match v {
        Value::Ref(inst) => format!("{}:{}", inst.ty.0, inst.id),
        _ => "null".to_string(),
    }
}

#[derive(Clone, Copy)]
struct W {
    unit_ty: EntityTypeId,
    position: FieldId,
    my_cell: FieldId,
    seen_count: FieldId,
    seen_size: FieldId,
    occupants: FieldId,
    c00: InstanceId,
    c10: InstanceId,
}

fn setup() -> (Runtime, W) {
    let mut rt = Runtime::new();
    let cell_ty = rt.register_entity_type(
        "Cell",
        vec![FieldDef::new("occupants", Value::Map(BTreeMap::new()))],
        false,
    );
    let grid_ty = rt.register_entity_type(
        "Grid",
        vec![FieldDef::new("cell_table", Value::Map(BTreeMap::new()))],
        false,
    );
    let unit_ty = rt.register_entity_type(
        "Unit",
        vec![
            FieldDef::new("position", Value::Null),
            FieldDef::reference("my_cell"),
            FieldDef::new("seen_count", Value::Int(0)),
            FieldDef::new("seen_size", Value::Int(0)),
        ],
        false,
    );

    let occupants = rt.field(cell_ty, "occupants");
    let cell_table = rt.field(grid_ty, "cell_table");
    let position = rt.field(unit_ty, "position");
    let my_cell = rt.field(unit_ty, "my_cell");
    let seen_count = rt.field(unit_ty, "seen_count");
    let seen_size = rt.field(unit_ty, "seen_size");

    let c00 = rt.spawn(cell_ty, vec![]);
    let c10 = rt.spawn(cell_ty, vec![]);
    let grid = rt.spawn(
        grid_ty,
        vec![(
            cell_table,
            Value::map([("0,0", Value::Ref(c00)), ("1,0", Value::Ref(c10))]),
        )],
    );

    let table_f = cell_table;
    rt.register_calculation(
        "locate",
        unit_ty,
        Predicate::new(
            own(position),
            Cond::Changed,
            Delivery::Each(vec![Proj::New(vec![])]),
        ),
        &[my_cell],
        Box::new(move |ctx, input| {
            let Input::Each(row) = input else { return };
            let key = pos_key(&row[0]);
            let cell = match ctx.read(grid, table_f) {
                Value::Map(table) => table.get(&key).cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            };
            ctx.write(my_cell, cell);
        }),
    )
    .unwrap();

    let old_val = Expr::Val(ValRef::Old(vec![]));
    rt.register_calculation(
        "occupancy",
        cell_ty,
        Predicate::new(
            type_scope(unit_ty, my_cell),
            Cond::Or(
                Box::new(Cond::Cmp(new_val(), CmpOp::Eq, Expr::Val(ValRef::SelfRef))),
                Box::new(Cond::Cmp(old_val, CmpOp::Eq, Expr::Val(ValRef::SelfRef))),
            ),
            Delivery::Batch(vec![Proj::WriterId, Proj::New(vec![]), Proj::Old(vec![])]),
        ),
        &[occupants],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let self_ref = Value::Ref(ctx.self_id());
            let mut occ = map_of(&ctx.read_own(occupants));
            for row in rows {
                let key = writer_key(&row[0]);
                if row[2] == self_ref {
                    occ.remove(&key);
                }
                if row[1] == self_ref {
                    occ.insert(key, Value::Bool(true));
                }
            }
            ctx.write(occupants, Value::Map(occ));
        }),
    )
    .unwrap();

    rt.register_calculation(
        "react",
        unit_ty,
        Predicate::new(
            inst(my_cell, occupants),
            Cond::True,
            Delivery::Each(vec![Proj::New(vec![])]),
        ),
        &[seen_count, seen_size],
        Box::new(move |ctx, input| {
            let Input::Each(row) = input else { return };
            ctx.write(
                seen_count,
                Value::Int(as_i64(&ctx.read_own(seen_count)) + 1),
            );
            ctx.write(seen_size, Value::Int(map_of(&row[0]).len() as i64));
        }),
    )
    .unwrap();

    (
        rt,
        W {
            unit_ty,
            position,
            my_cell,
            seen_count,
            seen_size,
            occupants,
            c00,
            c10,
        },
    )
}

fn pos(x: i64, y: i64) -> Value {
    Value::map([("x", Value::Int(x)), ("y", Value::Int(y))])
}

fn set_pos(rt: &mut Runtime, unit: InstanceId, w: W, x: i64, y: i64) {
    rt.debug_write(unit, w.position, pos(x, y));
}

fn step_n(rt: &mut Runtime, n: usize) {
    for _ in 0..n {
        rt.step();
    }
}

fn seen(rt: &Runtime, unit: InstanceId, w: W) -> (i64, i64) {
    (
        as_i64(&rt.read(unit, w.seen_count)),
        as_i64(&rt.read(unit, w.seen_size)),
    )
}

#[test]
fn inst_subscription_follows_my_cell_ref_after_movement() {
    let (mut rt, w) = setup();
    let a = rt.spawn(w.unit_ty, vec![]);
    let b = rt.spawn(w.unit_ty, vec![]);

    set_pos(&mut rt, a, w, 0, 0);
    set_pos(&mut rt, b, w, 0, 0);
    step_n(&mut rt, 3);

    assert_eq!(rt.read(a, w.my_cell), Value::Ref(w.c00));
    assert_eq!(rt.read(b, w.my_cell), Value::Ref(w.c00));
    assert_eq!(map_of(&rt.read(w.c00, w.occupants)).len(), 2);
    assert_eq!(seen(&rt, a, w), (1, 2));
    assert_eq!(seen(&rt, b, w), (1, 2));

    set_pos(&mut rt, b, w, 1, 0);
    step_n(&mut rt, 3);

    assert_eq!(rt.read(b, w.my_cell), Value::Ref(w.c10));
    assert_eq!(map_of(&rt.read(w.c00, w.occupants)).len(), 1);
    assert_eq!(map_of(&rt.read(w.c10, w.occupants)).len(), 1);
    assert_eq!(seen(&rt, a, w), (2, 1));
    assert_eq!(seen(&rt, b, w), (2, 1));

    let c = rt.spawn(w.unit_ty, vec![]);
    set_pos(&mut rt, c, w, 0, 0);
    step_n(&mut rt, 3);

    assert_eq!(map_of(&rt.read(w.c00, w.occupants)).len(), 2);
    assert_eq!(seen(&rt, a, w), (3, 2));
    assert_eq!(seen(&rt, c, w), (1, 2));
    assert_eq!(seen(&rt, b, w), (2, 1));
}

#[test]
fn inst_rebind_routes_against_previous_ref_snapshot() {
    let mut rt = Runtime::new();
    let target_ty = rt.register_entity_type(
        "Target",
        vec![FieldDef::new("watched", Value::Int(0))],
        false,
    );
    let watched = rt.field(target_ty, "watched");
    let holder_ty = rt.register_entity_type(
        "Holder",
        vec![
            FieldDef::reference("target"),
            FieldDef::new("seen", Value::Int(0)),
        ],
        false,
    );
    let target = rt.field(holder_ty, "target");
    let seen = rt.field(holder_ty, "seen");

    rt.register_calculation(
        "watch_target",
        holder_ty,
        Predicate::new(
            inst(target, watched),
            Cond::True,
            Delivery::Each(vec![Proj::New(vec![])]),
        ),
        &[seen],
        Box::new(move |ctx, input| ctx.write(seen, input.arg(0).clone())),
    )
    .unwrap();

    let a = rt.spawn(target_ty, vec![]);
    let b = rt.spawn(target_ty, vec![]);
    let h = rt.spawn(holder_ty, vec![(target, Value::Ref(a))]);
    rt.step();

    rt.debug_write(h, target, Value::Ref(b));
    rt.debug_write(a, watched, Value::Int(11));
    rt.step();
    assert_eq!(
        rt.read(h, seen),
        Value::Int(11),
        "old target writes in the rebind frame still route to the previous subscriber"
    );

    rt.debug_write(b, watched, Value::Int(22));
    rt.step();
    assert_eq!(
        rt.read(h, seen),
        Value::Int(22),
        "new target writes route after the ref write has had one frame to become the subscription"
    );
}

#[test]
fn inst_rebind_does_not_route_new_target_in_same_write_set() {
    let mut rt = Runtime::new();
    let target_ty = rt.register_entity_type(
        "Target",
        vec![FieldDef::new("watched", Value::Int(0))],
        false,
    );
    let watched = rt.field(target_ty, "watched");
    let holder_ty = rt.register_entity_type(
        "Holder",
        vec![
            FieldDef::reference("target"),
            FieldDef::new("seen", Value::Int(0)),
        ],
        false,
    );
    let target = rt.field(holder_ty, "target");
    let seen = rt.field(holder_ty, "seen");

    rt.register_calculation(
        "watch_target",
        holder_ty,
        Predicate::new(
            inst(target, watched),
            Cond::True,
            Delivery::Each(vec![Proj::New(vec![])]),
        ),
        &[seen],
        Box::new(move |ctx, input| ctx.write(seen, input.arg(0).clone())),
    )
    .unwrap();

    let a = rt.spawn(target_ty, vec![]);
    let b = rt.spawn(target_ty, vec![]);
    let h = rt.spawn(holder_ty, vec![(target, Value::Ref(a))]);
    rt.step();

    rt.debug_write(h, target, Value::Ref(b));
    rt.debug_write(b, watched, Value::Int(22));
    rt.step();
    assert_eq!(
        rt.read(h, seen),
        Value::Int(0),
        "new target writes in the rebind frame must not route early"
    );
}
