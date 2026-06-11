//! Executable checks for docs/05-cooldown-state-machine.md.

use pce::predicate::{lit, new_path, own, own_field};
use pce::{
    CmpOp, Cond, Delivery, Expr, FieldDef, FieldId, Input, Predicate, Proj, Runtime, ValRef, Value,
};

const COOLDOWN: i64 = 60;

#[derive(Clone, Copy)]
struct F {
    cast_req: FieldId,
    cd_until: FieldId,
    state: FieldId,
    casting_skill: FieldId,
    cast_count: FieldId,
}

fn and(a: Cond, b: Cond) -> Cond {
    Cond::And(Box::new(a), Box::new(b))
}

fn or(a: Cond, b: Cond) -> Cond {
    Cond::Or(Box::new(a), Box::new(b))
}

fn state_is(field: FieldId, value: &str) -> Cond {
    Cond::Cmp(
        Expr::Val(ValRef::Own(field)),
        CmpOp::Eq,
        lit(Value::str(value)),
    )
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

fn cast_req(skill: &str, frame: i64) -> Value {
    Value::map([("skill", Value::str(skill)), ("frame", Value::Int(frame))])
}

fn setup() -> (Runtime, pce::EntityTypeId, F) {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type(
        "Unit",
        vec![
            FieldDef::new("cast_req", Value::Null),
            FieldDef::new("cd_until", Value::Int(0)),
            FieldDef::new("state", Value::str("idle")),
            FieldDef::new("casting_skill", Value::Null),
            FieldDef::new("cast_count", Value::Int(0)),
        ],
        false,
    );
    let f = F {
        cast_req: rt.field(unit, "cast_req"),
        cd_until: rt.field(unit, "cd_until"),
        state: rt.field(unit, "state"),
        casting_skill: rt.field(unit, "casting_skill"),
        cast_count: rt.field(unit, "cast_count"),
    };

    let cd_until = f.cd_until;
    let casting_skill = f.casting_skill;
    let cast_count = f.cast_count;
    rt.register_calculation(
        "cast",
        unit,
        Predicate::new(
            own(f.cast_req),
            and(
                Cond::Cmp(new_path(&["frame"]), CmpOp::Ge, own_field(f.cd_until)),
                or(state_is(f.state, "idle"), state_is(f.state, "moving")),
            ),
            Delivery::Each(vec![
                Proj::New(vec!["skill".to_string()]),
                Proj::New(vec!["frame".to_string()]),
            ]),
        ),
        &[cd_until, casting_skill, cast_count],
        Box::new(move |ctx, input| {
            let Input::Each(row) = input else { return };
            let frame = as_i64(&row[1]);
            let count = as_i64(&ctx.read_own(cast_count));
            ctx.write(casting_skill, row[0].clone());
            ctx.write(cd_until, Value::Int(frame + COOLDOWN));
            ctx.write(cast_count, Value::Int(count + 1));
        }),
    )
    .unwrap();

    (rt, unit, f)
}

#[test]
fn cooldown_and_state_guards_filter_before_calculation_runs() {
    let (mut rt, unit, f) = setup();
    let actor = rt.spawn(unit, vec![]);

    rt.debug_write(actor, f.cast_req, cast_req("fire", 0));
    rt.step();
    assert_eq!(as_i64(&rt.read(actor, f.cast_count)), 1);
    assert_eq!(as_str(&rt.read(actor, f.casting_skill)), "fire");
    assert_eq!(as_i64(&rt.read(actor, f.cd_until)), COOLDOWN);

    rt.debug_write(actor, f.cast_req, cast_req("ice", 30));
    rt.step();
    assert_eq!(as_i64(&rt.read(actor, f.cast_count)), 1);
    assert_eq!(as_str(&rt.read(actor, f.casting_skill)), "fire");
    assert_eq!(as_i64(&rt.read(actor, f.cd_until)), COOLDOWN);

    rt.debug_write(actor, f.state, Value::str("stunned"));
    rt.debug_write(actor, f.cast_req, cast_req("bolt", COOLDOWN));
    rt.step();
    assert_eq!(as_i64(&rt.read(actor, f.cast_count)), 1);
    assert_eq!(as_str(&rt.read(actor, f.casting_skill)), "fire");

    rt.debug_write(actor, f.state, Value::str("moving"));
    rt.debug_write(actor, f.cast_req, cast_req("bolt", COOLDOWN));
    rt.step();
    assert_eq!(as_i64(&rt.read(actor, f.cast_count)), 2);
    assert_eq!(as_str(&rt.read(actor, f.casting_skill)), "bolt");
    assert_eq!(as_i64(&rt.read(actor, f.cd_until)), COOLDOWN * 2);
}

#[test]
fn state_field_has_one_static_writer() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type(
        "Unit",
        vec![
            FieldDef::new("state", Value::str("idle")),
            FieldDef::new("stun_hit", Value::Bool(false)),
            FieldDef::new("recover", Value::Bool(false)),
        ],
        false,
    );
    let state = rt.field(unit, "state");
    let stun_hit = rt.field(unit, "stun_hit");
    let recover = rt.field(unit, "recover");

    rt.register_calculation(
        "enter_stun",
        unit,
        Predicate::new(own(stun_hit), Cond::True, Delivery::Each(vec![])),
        &[state],
        Box::new(move |ctx, _| ctx.write(state, Value::str("stunned"))),
    )
    .unwrap();

    let err = rt
        .register_calculation(
            "leave_stun",
            unit,
            Predicate::new(own(recover), Cond::True, Delivery::Each(vec![])),
            &[state],
            Box::new(move |ctx, _| ctx.write(state, Value::str("idle"))),
        )
        .unwrap_err();
    assert!(err.contains("D1"));
}
