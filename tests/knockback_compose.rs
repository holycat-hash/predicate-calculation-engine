//! Executable checks for docs/17-knockback-compose.md:
//! movement sources are batched into one mover, active movement is filtered by
//! root, hooks are arbitrated by a deterministic key, and the winner is echoed.

use pce::predicate::{inst, lit, new_path, own, own_field, type_scope};
use pce::{
    CmpOp, Cond, Delivery, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, Scope, ValRef, Value,
};

const WALL_X: i64 = 10;

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

#[derive(Clone, Copy)]
struct F {
    unit_ty: EntityTypeId,
    hook_ty: EntityTypeId,
    position: FieldId,
    root_until: FieldId,
    kb_resist: FieldId,
    slow_mul: FieldId,
    move_op: FieldId,
    grab_winner: FieldId,
    hook_out: FieldId,
    hook_target: FieldId,
    result: FieldId,
}

fn setup() -> (Runtime, F) {
    let mut rt = Runtime::new();
    let unit_ty = rt.register_entity_type(
        "Unit",
        vec![
            FieldDef::new("position", vec2(0, 0)),
            FieldDef::new("root_until", Value::Int(0)),
            FieldDef::new("kb_resist", Value::Int(0)),
            FieldDef::new("slow_mul", Value::Float(1.0)),
            FieldDef::new("move_op", Value::Null),
            FieldDef::reference("grab_winner"),
        ],
        false,
    );
    let hook_ty = rt.register_entity_type(
        "Hook",
        vec![
            FieldDef::new("hook_out", Value::Null),
            FieldDef::reference("hook_target"),
            FieldDef::new("result", Value::str("pending")),
        ],
        false,
    );
    let f = F {
        unit_ty,
        hook_ty,
        position: rt.field(unit_ty, "position"),
        root_until: rt.field(unit_ty, "root_until"),
        kb_resist: rt.field(unit_ty, "kb_resist"),
        slow_mul: rt.field(unit_ty, "slow_mul"),
        move_op: rt.field(unit_ty, "move_op"),
        grab_winner: rt.field(unit_ty, "grab_winner"),
        hook_out: rt.field(hook_ty, "hook_out"),
        hook_target: rt.field(hook_ty, "hook_target"),
        result: rt.field(hook_ty, "result"),
    };

    let active_while_rooted = Cond::And(
        Box::new(Cond::Cmp(
            new_path(&["class"]),
            CmpOp::Eq,
            lit(Value::str("active")),
        )),
        Box::new(Cond::Cmp(
            new_path(&["frame"]),
            CmpOp::Lt,
            own_field(f.root_until),
        )),
    );
    rt.register_calculation(
        "mover",
        unit_ty,
        Predicate::new(
            scope_or(vec![type_scope(hook_ty, f.hook_out), own(f.move_op)]),
            Cond::And(
                Box::new(target_is_self()),
                Box::new(Cond::AndNot(
                    Box::new(Cond::True),
                    Box::new(active_while_rooted),
                )),
            ),
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[f.position, f.grab_winner],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let ops: Vec<Value> = rows.iter().map(|r| r[0].clone()).collect();
            let mut carrier = vec2(0, 0);
            let mut forced: Vec<Value> = vec![];
            let mut active: Vec<Value> = vec![];
            let mut regular: Vec<Value> = vec![];
            let mut hooks: Vec<Value> = vec![];

            for op in ops {
                if as_str(&path(&op, "kind")) == "hook" {
                    hooks.push(op);
                    continue;
                }
                match as_str(&path(&op, "class")).as_str() {
                    "carrier" => carrier = add_vec(&carrier, &path(&op, "vec")),
                    "forced" => forced.push(op),
                    "active" => active.push(op),
                    _ => regular.push(op),
                }
            }

            let mut winner_ref = Value::Null;
            let mut base = if let Some(winner) = hooks
                .iter()
                .max_by_key(|v| (as_i64(&path(v, "prio")), as_str(&path(v, "salt"))))
            {
                winner_ref = path(winner, "source");
                path(winner, "vec")
            } else if !forced.is_empty() {
                let sum = forced
                    .iter()
                    .fold(vec2(0, 0), |acc, op| add_vec(&acc, &path(op, "vec")));
                let keep = (100 - as_i64(&ctx.read_own(f.kb_resist))).clamp(0, 100);
                scale_vec(&sum, keep as f64 / 100.0)
            } else if let Some(best) = active.iter().max_by_key(|v| {
                let vec = path(v, "vec");
                let x = as_i64(&path(&vec, "x"));
                let y = as_i64(&path(&vec, "y"));
                (x * x + y * y, as_str(&path(v, "salt")))
            }) {
                path(best, "vec")
            } else {
                let sum = regular
                    .iter()
                    .fold(vec2(0, 0), |acc, op| add_vec(&acc, &path(op, "vec")));
                scale_vec(&sum, as_f64(&ctx.read_own(f.slow_mul)))
            };
            base = add_vec(&base, &carrier);

            let pos = ctx.read_own(f.position);
            let nx = (as_i64(&path(&pos, "x")) + as_i64(&path(&base, "x"))).clamp(0, WALL_X);
            let ny = as_i64(&path(&pos, "y")) + as_i64(&path(&base, "y"));
            ctx.write(f.position, vec2(nx, ny));
            ctx.write(f.grab_winner, winner_ref);
        }),
    )
    .unwrap();

    rt.register_calculation(
        "hook_result",
        hook_ty,
        Predicate::new(
            inst(f.hook_target, f.grab_winner),
            Cond::True,
            Delivery::Each(vec![Proj::New(vec![])]),
        ),
        &[f.result],
        Box::new(move |ctx, input| {
            let Input::Each(row) = input else { return };
            let result = if row[0] == Value::Ref(ctx.self_id()) {
                "win"
            } else {
                "lose"
            };
            ctx.write(f.result, Value::str(result));
        }),
    )
    .unwrap();

    (rt, f)
}

fn vec2(x: i64, y: i64) -> Value {
    Value::map([("x", Value::Int(x)), ("y", Value::Int(y))])
}

fn add_vec(a: &Value, b: &Value) -> Value {
    vec2(
        as_i64(&path(a, "x")) + as_i64(&path(b, "x")),
        as_i64(&path(a, "y")) + as_i64(&path(b, "y")),
    )
}

fn scale_vec(v: &Value, scale: f64) -> Value {
    vec2(
        (as_f64(&path(v, "x")) * scale).round() as i64,
        (as_f64(&path(v, "y")) * scale).round() as i64,
    )
}

fn movement(target: InstanceId, class: &str, x: i64, frame: i64, salt: &str) -> Value {
    Value::map([
        ("target", Value::Ref(target)),
        ("class", Value::str(class)),
        ("kind", Value::str("move")),
        ("vec", vec2(x, 0)),
        ("frame", Value::Int(frame)),
        ("salt", Value::str(salt)),
    ])
}

fn hook(target: InstanceId, source: InstanceId, prio: i64, x: i64, salt: &str) -> Value {
    Value::map([
        ("target", Value::Ref(target)),
        ("source", Value::Ref(source)),
        ("class", Value::str("forced")),
        ("kind", Value::str("hook")),
        ("vec", vec2(x, 0)),
        ("prio", Value::Int(prio)),
        ("salt", Value::str(salt)),
    ])
}

#[test]
fn hooks_arbitrate_once_active_move_is_rooted_and_wall_clips() {
    let (mut rt, f) = setup();
    let target = rt.spawn(f.unit_ty, vec![(f.root_until, Value::Int(100))]);
    let hook_a = rt.spawn(f.hook_ty, vec![(f.hook_target, Value::Ref(target))]);
    let hook_b = rt.spawn(f.hook_ty, vec![(f.hook_target, Value::Ref(target))]);

    rt.debug_write(
        target,
        f.move_op,
        movement(target, "active", 5, rt.frame() as i64, "dash"),
    );
    rt.debug_write(hook_a, f.hook_out, hook(target, hook_a, 1, 3, "a"));
    rt.debug_write(hook_b, f.hook_out, hook(target, hook_b, 2, 12, "b"));
    rt.step();

    assert_eq!(path(&rt.read(target, f.position), "x"), Value::Int(WALL_X));
    assert_eq!(rt.read(target, f.grab_winner), Value::Ref(hook_b));

    rt.step();
    assert_eq!(rt.read(hook_a, f.result), Value::str("lose"));
    assert_eq!(rt.read(hook_b, f.result), Value::str("win"));
}

#[test]
fn non_hook_movement_uses_segmented_composition_and_carrier_adds_on_top() {
    let (mut rt, f) = setup();
    let target = rt.spawn(
        f.unit_ty,
        vec![
            (f.root_until, Value::Int(100)),
            (f.kb_resist, Value::Int(50)),
            (f.slow_mul, Value::Float(0.5)),
        ],
    );

    rt.debug_write(
        target,
        f.move_op,
        movement(target, "active", 9, rt.frame() as i64, "rooted-dash"),
    );
    rt.debug_write(
        target,
        f.move_op,
        movement(target, "regular", 8, rt.frame() as i64, "walk"),
    );
    rt.debug_write(
        target,
        f.move_op,
        movement(target, "forced", 6, rt.frame() as i64, "blast-a"),
    );
    rt.debug_write(
        target,
        f.move_op,
        movement(target, "forced", -2, rt.frame() as i64, "blast-b"),
    );
    rt.debug_write(
        target,
        f.move_op,
        movement(target, "carrier", 3, rt.frame() as i64, "platform"),
    );

    rt.step();

    // active is filtered by root; forced non-empty shadows regular movement,
    // resist halves net forced (6 - 2), and carrier is always added.
    assert_eq!(path(&rt.read(target, f.position), "x"), Value::Int(5));
    assert_eq!(rt.read(target, f.grab_winner), Value::Null);
}
