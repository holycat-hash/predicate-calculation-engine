//! 全存快照 / 回滚（GGPO 式 rollback netcode）：snapshot 捕获全部动态仿真状态，
//! restore 回滚后用修正输入重放——结果只反映重放输入，不残留被回滚的那次推进。

use pce::predicate::own;
use pce::{Cond, Delivery, FieldDef, Predicate, Runtime, Value};

fn field(name: &str, default: impl Into<Value>) -> FieldDef {
    FieldDef::new(name, default.into())
}

/// 积分器：每次 vx 写入，x ← snapshot(x) + vx。用作可重放的确定性仿真。
fn integrator() -> (Runtime, pce::EntityTypeId, pce::FieldId, pce::FieldId) {
    let mut rt = Runtime::new();
    let body = rt.register_entity_type("Body", vec![field("x", 0), field("vx", 0)], false);
    let f_x = rt.field(body, "x");
    let f_vx = rt.field(body, "vx");
    rt.register_calculation(
        "integrate",
        body,
        Predicate::new(own(f_vx), Cond::True, Delivery::Each(vec![])),
        &[f_x],
        Box::new(move |ctx, _| {
            let x = ctx.read_own(f_x).as_i64().unwrap_or(0);
            let vx = ctx.read_own(f_vx).as_i64().unwrap_or(0);
            ctx.write(f_x, x + vx);
        }),
    )
    .unwrap();
    (rt, body, f_x, f_vx)
}

/// 预测输入错误 → 回滚 → 修正输入重放，x 只反映修正值。
#[test]
fn rollback_replays_with_corrected_input() {
    let (mut rt, body, f_x, f_vx) = integrator();
    let b = rt.spawn(body, vec![]);
    rt.step(); // 消化 spawn
    assert_eq!(rt.read(b, f_x), Value::Int(0));

    let snap = rt.snapshot();
    let saved_frame = snap.frame();

    // 预测：vx = 5
    rt.debug_write(b, f_vx, Value::Int(5));
    rt.step();
    assert_eq!(rt.read(b, f_x), Value::Int(5));

    // 收到正确输入：vx 实际是 2。回滚到快照，重放。
    rt.restore(&snap);
    assert_eq!(rt.read(b, f_x), Value::Int(0)); // 状态已回滚
    assert_eq!(rt.frame(), saved_frame); // 帧号也回滚

    rt.debug_write(b, f_vx, Value::Int(2));
    rt.step();
    assert_eq!(rt.read(b, f_x), Value::Int(2)); // 只反映修正输入，无 5 的残留
}

/// 同一快照重放同一输入 → 位级相同结果（确定性重放，lockstep 前提）。
#[test]
fn replay_is_deterministic() {
    let (mut rt, body, f_x, f_vx) = integrator();
    let b = rt.spawn(body, vec![]);
    rt.step();
    let snap = rt.snapshot();

    let run = |rt: &mut Runtime| {
        rt.restore(&snap);
        for v in [3, 7, 2] {
            rt.debug_write(b, f_vx, Value::Int(v));
            rt.step();
        }
        rt.read(b, f_x)
    };
    let a = run(&mut rt);
    let c = run(&mut rt);
    assert_eq!(a, c);
    assert_eq!(a, Value::Int(12)); // 0+3+7+2
}

/// last_schedule 是公开动态状态，rollback 后不能暴露预测帧的执行计划。
#[test]
fn rollback_restores_last_schedule() {
    let (mut rt, body, _f_x, f_vx) = integrator();
    let b = rt.spawn(body, vec![]);
    rt.step();
    let snap = rt.snapshot();
    assert!(rt.last_schedule().groups.is_empty());

    rt.debug_write(b, f_vx, Value::Int(5));
    rt.step();
    assert!(!rt.last_schedule().groups.is_empty());

    rt.restore(&snap);
    assert!(rt.last_schedule().groups.is_empty());
}

/// 快照含生命周期状态：回滚使被销毁的实例复活、未发生的 spawn 撤销。
#[test]
fn rollback_restores_lifecycle_and_id_allocation() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![field("tag", 0)], false);
    let f_tag = rt.field(unit, "tag");

    let a = rt.spawn(unit, vec![(f_tag, Value::Int(1))]);
    rt.step();
    assert_eq!(rt.alive(unit).len(), 1);

    let snap = rt.snapshot();

    // 推进：销毁 a、spawn 新实例。
    rt.destroy(a);
    let _c = rt.spawn(unit, vec![(f_tag, Value::Int(2))]);
    rt.step();
    assert!(!matches!(rt.read(a, f_tag), Value::Int(1))); // a 已死（读 Null）

    // 回滚：a 复活、新实例消失、id 分配状态恢复。
    rt.restore(&snap);
    assert_eq!(rt.alive(unit).len(), 1);
    assert_eq!(rt.read(a, f_tag), Value::Int(1));

    // 回滚后再 spawn 应复用与首次推进相同的 id/代际路径（确定性）。
    let d = rt.spawn(unit, vec![(f_tag, Value::Int(9))]);
    rt.step();
    assert_eq!(rt.read(d, f_tag), Value::Int(9));
    assert_eq!(rt.alive(unit).len(), 2);
}

/// fold 增量状态进快照：回滚连 fold 多重集一并恢复。
#[test]
fn rollback_restores_fold_state() {
    use pce::FoldOp;
    use pce::predicate::type_scope;

    let mut rt = Runtime::new();
    let enemy = rt.register_entity_type("Enemy", vec![field("hp", 0)], false);
    let bar = rt.register_entity_type("Bar", vec![field("weakest", ())], true);
    let f_hp = rt.field(enemy, "hp");
    let f_weakest = rt.field(bar, "weakest");
    rt.register_calculation(
        "weakest",
        bar,
        Predicate::new(
            type_scope(enemy, f_hp),
            Cond::True,
            Delivery::Fold(FoldOp::Min),
        ),
        &[f_weakest],
        Box::new(move |ctx, input| ctx.write(f_weakest, input.agg().clone())),
    )
    .unwrap();

    let e1 = rt.spawn(enemy, vec![(f_hp, Value::Int(10))]);
    let b0 = rt.alive(bar)[0];
    rt.step();
    assert_eq!(rt.read(b0, f_weakest), Value::Float(10.0));

    let snap = rt.snapshot();

    rt.debug_write(e1, f_hp, Value::Int(3)); // min → 3
    rt.step();
    assert_eq!(rt.read(b0, f_weakest), Value::Float(3.0));

    rt.restore(&snap); // fold 多重集回滚：min 回到 10
    rt.debug_write(e1, f_hp, Value::Int(7));
    rt.step();
    assert_eq!(rt.read(b0, f_weakest), Value::Float(7.0)); // 不是 3（被回滚）
}
