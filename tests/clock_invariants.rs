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

/// OQ1：`set_alarm_in` 相对排程——N 帧后写 `Clock.alarm`，订阅者到点触发一次（含 payload）。
#[test]
fn relative_alarm_fires_after_n_frames() {
    let mut rt = Runtime::new();
    let clock_ty = rt.clock().ty;
    let clock_alarm = rt.clock().f_alarm;
    let watch = rt.register_entity_type("Watch", vec![field("rang", 0)], true);
    let f_rang = rt.field(watch, "rang");

    rt.register_calculation(
        "on_alarm",
        watch,
        Predicate::new(
            type_scope(clock_ty, clock_alarm),
            Cond::Became(Value::Int(7)), // 只在 payload=7 的 alarm 上触发（同时验证排程与载荷）
            Delivery::Each(vec![]),
        ),
        &[f_rang],
        Box::new(move |ctx, _| {
            let n = ctx.read_own(f_rang).as_i64().unwrap_or(0);
            ctx.write(f_rang, n + 1);
        }),
    )
    .unwrap();

    let w0 = rt.alive(watch)[0];
    rt.set_alarm_in(2, Value::Int(7)); // frame()=0 → at_frame=2
    rt.step(); // frame 1：未到点
    assert_eq!(rt.read(w0, f_rang), Value::Int(0), "第 1 帧未到点");
    rt.step(); // frame 2：alarm 写 7 → Became(7) 触发一次
    assert_eq!(rt.read(w0, f_rang), Value::Int(1), "第 2 帧到点触发一次");
    rt.step(); // frame 3：无 alarm 写 → 无触发（缺席不是事件）
    assert_eq!(rt.read(w0, f_rang), Value::Int(1), "到点后不再触发");
}

#[test]
fn relative_alarm_saturates_instead_of_overflowing() {
    let mut rt = Runtime::new();
    rt.step();
    rt.set_alarm_in(u64::MAX, Value::Int(1));
    rt.step();
    assert_eq!(
        rt.read(rt.clock().inst, rt.clock().f_alarm),
        Value::Null,
        "overflowing relative alarms must not wrap into an immediate/past frame"
    );
}

#[test]
fn alarm_quota_is_observable_and_try_api_returns_err() {
    let mut rt = Runtime::new();
    rt.set_alarm_limit(Some(2));
    rt.try_set_alarm(10, Value::Int(1)).unwrap();
    rt.try_set_alarm(11, Value::Int(2)).unwrap();
    assert_eq!(rt.pending_alarms(), 2);

    let err = rt.try_set_alarm(12, Value::Int(3)).unwrap_err();
    assert!(err.contains("quota"), "{err}");
    assert_eq!(rt.pending_alarms(), 2);

    for _ in 0..10 {
        rt.step();
    }
    assert_eq!(rt.pending_alarms(), 1);
}
