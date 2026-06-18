//! Executable checks for docs/18-cast-interrupt.md:
//! due probes are not cancelled; stale probes are made harmless by cast_seq.

use pce::predicate::{new_path, new_val, own, own_field, type_scope};
use pce::{
    CmpOp, Cond, Delivery, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, Scope, ValRef, Value,
};

const CAST_TIME: i64 = 3;
const UNINTAIL: i64 = 1;
const INF: i64 = 1_000_000;

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

fn as_str(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        _ => String::new(),
    }
}

#[derive(Clone, Copy)]
struct F {
    caster_ty: EntityTypeId,
    begin_op: FieldId,
    due_op: FieldId,
    stun_op: FieldId,
    phase: FieldId,
    cast_seq: FieldId,
    due_at: FieldId,
    mana: FieldId,
    complete_count: FieldId,
}

fn setup() -> (Runtime, F) {
    let mut rt = Runtime::new();
    let caster_ty = rt.register_entity_type(
        "Caster",
        vec![
            FieldDef::new("begin_op", Value::Null),
            FieldDef::new("due_op", Value::Null),
            FieldDef::new("stun_op", Value::Null),
            FieldDef::new("phase", Value::str("idle")),
            FieldDef::new("cast_seq", Value::Int(0)),
            FieldDef::new("due_at", Value::Int(INF)),
            FieldDef::new("unint_from", Value::Int(INF)),
            FieldDef::new("mana", Value::Int(100)),
            FieldDef::new("cast_cost", Value::Int(0)),
            FieldDef::new("complete_count", Value::Int(0)),
        ],
        false,
    );
    let f = F {
        caster_ty,
        begin_op: rt.field(caster_ty, "begin_op"),
        due_op: rt.field(caster_ty, "due_op"),
        stun_op: rt.field(caster_ty, "stun_op"),
        phase: rt.field(caster_ty, "phase"),
        cast_seq: rt.field(caster_ty, "cast_seq"),
        due_at: rt.field(caster_ty, "due_at"),
        mana: rt.field(caster_ty, "mana"),
        complete_count: rt.field(caster_ty, "complete_count"),
    };
    let unint_from = rt.field(caster_ty, "unint_from");
    let cast_cost = rt.field(caster_ty, "cast_cost");
    let clock_ty = rt.clock().ty;
    let clock_frame = rt.clock().f_frame;

    rt.register_calculation(
        "probe",
        caster_ty,
        Predicate::new(
            type_scope(clock_ty, clock_frame),
            Cond::Cmp(new_val(), CmpOp::Ge, own_field(f.due_at)),
            Delivery::Each(vec![Proj::New(vec![])]),
        ),
        &[f.due_op],
        Box::new(move |ctx, input| {
            let Input::Each(row) = input else { return };
            ctx.write(
                f.due_op,
                Value::map([
                    ("target", Value::Ref(ctx.self_id())),
                    ("op", Value::str("due")),
                    ("seq", ctx.read_own(f.cast_seq)),
                    ("frame", row[0].clone()),
                ]),
            );
        }),
    )
    .unwrap();

    rt.register_calculation(
        "settle",
        caster_ty,
        Predicate::new(
            scope_or(vec![own(f.begin_op), own(f.due_op), own(f.stun_op)]),
            target_is_self(),
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[
            f.phase,
            f.cast_seq,
            f.due_at,
            unint_from,
            f.mana,
            cast_cost,
            f.complete_count,
        ],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let cur_seq = as_i64(&ctx.read_own(f.cast_seq));
            let mut phase = as_str(&ctx.read_own(f.phase));
            let mut seq = cur_seq;
            let mut due_at = as_i64(&ctx.read_own(f.due_at));
            let mut unint = as_i64(&ctx.read_own(unint_from));
            let mut mana = as_i64(&ctx.read_own(f.mana));
            let mut cost = as_i64(&ctx.read_own(cast_cost));
            let mut complete = as_i64(&ctx.read_own(f.complete_count));

            let mut begins: Vec<Value> = vec![];
            let mut terminals: Vec<Value> = vec![];
            for row in rows {
                let op = &row[0];
                match as_str(&path(op, "op")).as_str() {
                    "begin" => begins.push(op.clone()),
                    "due" | "stun" | "cancel" => {
                        let op_seq = as_i64(&path(op, "seq"));
                        if op_seq == cur_seq || op_seq == 0 {
                            terminals.push(op.clone());
                        }
                    }
                    _ => {}
                }
            }

            if phase == "idle"
                && let Some(begin) = begins.iter().max_by_key(|v| as_i64(&path(v, "frame")))
            {
                let frame = as_i64(&path(begin, "frame"));
                cost = as_i64(&path(begin, "cost"));
                mana -= cost;
                seq += 1;
                phase = "casting".to_string();
                due_at = frame + CAST_TIME;
                unint = due_at - UNINTAIL;
            }

            if phase == "casting" {
                let due = terminals.iter().any(|op| as_str(&path(op, "op")) == "due");
                let interrupt = terminals.iter().any(|op| {
                    matches!(as_str(&path(op, "op")).as_str(), "stun" | "cancel")
                        && as_i64(&path(op, "frame")) < unint
                });
                if due {
                    complete += 1;
                    seq += 1;
                    phase = "idle".to_string();
                    due_at = INF;
                    unint = INF;
                    cost = 0;
                } else if interrupt {
                    mana += cost * 7 / 10;
                    seq += 1;
                    phase = "idle".to_string();
                    due_at = INF;
                    unint = INF;
                    cost = 0;
                }
            }

            ctx.write(f.phase, Value::str(&phase));
            ctx.write(f.cast_seq, Value::Int(seq));
            ctx.write(f.due_at, Value::Int(due_at));
            ctx.write(unint_from, Value::Int(unint));
            ctx.write(f.mana, Value::Int(mana));
            ctx.write(cast_cost, Value::Int(cost));
            ctx.write(f.complete_count, Value::Int(complete));
        }),
    )
    .unwrap();

    (rt, f)
}

fn begin(rt: &mut Runtime, caster: InstanceId, f: F, cost: i64) {
    rt.debug_write(
        caster,
        f.begin_op,
        Value::map([
            ("target", Value::Ref(caster)),
            ("op", Value::str("begin")),
            ("seq", Value::Int(0)),
            ("frame", Value::Int(rt.frame() as i64)),
            ("cost", Value::Int(cost)),
        ]),
    );
}

#[test]
fn stale_due_probe_cannot_complete_the_next_cast() {
    let (mut rt, f) = setup();
    let caster = rt.spawn(f.caster_ty, vec![]);

    begin(&mut rt, caster, f, 10);
    rt.step();
    assert_eq!(rt.read(caster, f.phase), Value::str("casting"));
    assert_eq!(rt.read(caster, f.cast_seq), Value::Int(1));

    rt.step();
    rt.step();
    rt.step();
    assert_eq!(rt.read(caster, f.phase), Value::str("idle"));
    assert_eq!(rt.read(caster, f.complete_count), Value::Int(1));
    assert_eq!(rt.read(caster, f.cast_seq), Value::Int(2));

    begin(&mut rt, caster, f, 10);
    rt.step();

    assert_eq!(rt.read(caster, f.phase), Value::str("casting"));
    assert_eq!(rt.read(caster, f.complete_count), Value::Int(1));
    assert_eq!(rt.read(caster, f.cast_seq), Value::Int(3));
    assert_eq!(rt.read(caster, f.mana), Value::Int(80));
}
