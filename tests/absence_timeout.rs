//! 01 超时 / 静默 N 帧 / 心跳掉线（docs/01-absence-timeout.md）的可运行验证：
//! 租约只由正事件心跳续期，过期只经 Clock.frame 这个显式轮询出口触发，且死亡边沿只写一次。

use pce::predicate::{lit, new_path, new_val, own_field, type_scope};
use pce::{
    CmpOp, Cond, Delivery, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, ValRef, Value,
};

const LEASE: i64 = 30;

fn as_i64(v: &Value) -> i64 {
    v.as_f64().unwrap_or(0.0) as i64
}

#[derive(Clone, Copy)]
struct W {
    session_ty: EntityTypeId,
    conn_ty: EntityTypeId,
    lease_until: FieldId,
    state: FieldId,
    expire_count: FieldId,
    beat: FieldId,
}

fn setup() -> (Runtime, W) {
    let mut rt = Runtime::new();
    let session_ty = rt.register_entity_type(
        "Session",
        vec![
            FieldDef::new("lease_until", Value::Int(LEASE)),
            FieldDef::new("state", Value::str("alive")),
            FieldDef::new("expire_count", Value::Int(0)),
        ],
        false,
    );
    let conn_ty = rt.register_entity_type("Conn", vec![FieldDef::new("beat", Value::Null)], false);
    let w = W {
        session_ty,
        conn_ty,
        lease_until: rt.field(session_ty, "lease_until"),
        state: rt.field(session_ty, "state"),
        expire_count: rt.field(session_ty, "expire_count"),
        beat: rt.field(conn_ty, "beat"),
    };

    let lease_f = w.lease_until;
    rt.register_calculation(
        "renew_lease",
        session_ty,
        Predicate::new(
            type_scope(conn_ty, w.beat),
            Cond::Cmp(new_path(&["session"]), CmpOp::Eq, Expr::Val(ValRef::SelfRef)),
            Delivery::Each(vec![Proj::New(vec!["frame".to_string()])]),
        ),
        &[lease_f],
        Box::new(move |ctx, input| {
            let Input::Each(row) = input else { return };
            ctx.write(lease_f, Value::Int(as_i64(&row[0]) + LEASE));
        }),
    )
    .unwrap();

    let clock_ty = rt.clock().ty;
    let clock_frame = rt.clock().f_frame;
    let (state_f, count_f) = (w.state, w.expire_count);
    rt.register_calculation(
        "expire_session",
        session_ty,
        Predicate::new(
            type_scope(clock_ty, clock_frame),
            Cond::AndNot(
                Box::new(Cond::Cmp(new_val(), CmpOp::Gt, own_field(lease_f))),
                Box::new(Cond::Cmp(own_field(state_f), CmpOp::Eq, lit(Value::str("dead")))),
            ),
            Delivery::Each(vec![]),
        ),
        &[state_f, count_f],
        Box::new(move |ctx, _| {
            ctx.write(state_f, Value::str("dead"));
            ctx.write(count_f, Value::Int(as_i64(&ctx.read_own(count_f)) + 1));
        }),
    )
    .unwrap();

    (rt, w)
}

fn step_n(rt: &mut Runtime, n: usize) {
    for _ in 0..n {
        rt.step();
    }
}

fn heartbeat(rt: &mut Runtime, conn: InstanceId, session: InstanceId, w: W) {
    rt.debug_write(
        conn,
        w.beat,
        Value::map([
            ("session", Value::Ref(session)),
            ("frame", Value::Int(rt.frame() as i64)),
        ]),
    );
}

#[test]
fn heartbeat_renews_lease_and_clock_expiry_fires_once() {
    let (mut rt, w) = setup();
    let session = rt.spawn(w.session_ty, vec![]);
    let conn = rt.spawn(w.conn_ty, vec![]);

    step_n(&mut rt, 20);
    assert_eq!(rt.read(session, w.state), Value::str("alive"));

    heartbeat(&mut rt, conn, session, w);
    rt.step();
    assert_eq!(rt.frame(), 21);
    assert_eq!(rt.read(session, w.lease_until), Value::Int(50));
    assert_eq!(rt.read(session, w.state), Value::str("alive"));

    while rt.frame() < 50 {
        rt.step();
    }
    assert_eq!(rt.read(session, w.state), Value::str("alive"));

    rt.step();
    assert_eq!(rt.frame(), 51);
    assert_eq!(rt.read(session, w.state), Value::str("dead"));
    assert_eq!(rt.read(session, w.expire_count), Value::Int(1));

    step_n(&mut rt, 5);
    assert_eq!(rt.read(session, w.state), Value::str("dead"));
    assert_eq!(rt.read(session, w.expire_count), Value::Int(1));
}
