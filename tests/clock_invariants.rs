use pce::predicate::{own, type_scope};
use pce::{Cond, Delivery, FieldDef, Predicate, Proj, Runtime, Value};

fn field(name: &str, default: impl Into<Value>) -> FieldDef {
    FieldDef::new(name, default.into())
}

/// Clock.frame 是 runtime write，但本帧 calculation 仍只能读到上一帧提交值。
#[test]
fn clock_frame_snapshot_read_is_previous_frame() {
    let mut rt = Runtime::new();
    let clock_inst = rt.clock().inst;
    let clock_frame = rt.clock().f_frame;
    let actor = rt.register_entity_type("Actor", vec![field("input", 0), field("seen", 0)], false);
    let f_input = rt.field(actor, "input");
    let f_seen = rt.field(actor, "seen");

    rt.register_calculation(
        "sample_clock",
        actor,
        Predicate::new(own(f_input), Cond::True, Delivery::Each(vec![])),
        &[f_seen],
        Box::new(move |ctx, _| ctx.write(f_seen, ctx.read(clock_inst, clock_frame))),
    )
    .unwrap();

    let a = rt.spawn(actor, vec![]);
    rt.step();
    assert_eq!(rt.read(clock_inst, clock_frame), Value::Int(1));

    rt.debug_write(a, f_input, Value::Int(1));
    rt.step();
    assert_eq!(rt.read(a, f_seen), Value::Int(1));
}

/// 同一帧多个 alarm 都应以上一帧提交值作为 old，不能被同帧前一条 alarm 污染。
#[test]
fn same_frame_alarm_writes_keep_snapshot_old_value() {
    let mut rt = Runtime::new();
    let clock_ty = rt.clock().ty;
    let clock_alarm = rt.clock().f_alarm;
    let watch = rt.register_entity_type("Watch", vec![field("bad_old", 0)], true);
    let f_bad_old = rt.field(watch, "bad_old");

    rt.register_calculation(
        "alarm_old",
        watch,
        Predicate::new(
            type_scope(clock_ty, clock_alarm),
            Cond::True,
            Delivery::Batch(vec![Proj::Old(vec![])]),
        ),
        &[f_bad_old],
        Box::new(move |ctx, input| {
            let bad = input
                .rows()
                .iter()
                .filter(|row| row[0] != Value::Null)
                .count();
            ctx.write(f_bad_old, bad as i64);
        }),
    )
    .unwrap();

    let w0 = rt.alive(watch)[0];
    rt.set_alarm(1, Value::Int(10));
    rt.set_alarm(1, Value::Int(20));
    rt.step();
    assert_eq!(rt.read(w0, f_bad_old), Value::Int(0));
}
