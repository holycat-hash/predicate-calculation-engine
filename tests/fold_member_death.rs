//! fold min/max 成员死亡撤销（§6.3）：成员实例死亡（无写）后，
//! 其对 min/max 多重集的贡献被按实例精确撤销，聚合值收缩，并在下一帧重投递。

use pce::predicate::{inst, type_scope};
use pce::{
    Cond, Delivery, EntityTypeId, FieldDef, FieldId, FoldOp, Predicate, Runtime, Scope, Value,
};

fn field(name: &str, default: impl Into<Value>) -> FieldDef {
    FieldDef::new(name, default.into())
}

fn enemy_and_bar(rt: &mut Runtime, op: FoldOp) -> (EntityTypeId, FieldId, FieldId, EntityTypeId) {
    let enemy = rt.register_entity_type("Enemy", vec![field("hp", 0)], false);
    let bar = rt.register_entity_type("Bar", vec![field("agg", ())], true);
    let f_hp = rt.field(enemy, "hp");
    let f_agg = rt.field(bar, "agg");
    rt.register_calculation(
        "agg",
        bar,
        Predicate::new(type_scope(enemy, f_hp), Cond::True, Delivery::Fold(op)),
        &[f_agg],
        Box::new(move |ctx, input| ctx.write(f_agg, input.agg().clone())),
    )
    .unwrap();
    (enemy, f_hp, f_agg, bar)
}

/// 最小成员**死亡**后 min 收缩到次小值（旧实现死成员永久卡住 min）。
#[test]
fn min_recovers_after_member_death() {
    let mut rt = Runtime::new();
    let (enemy, f_hp, f_agg, bar) = enemy_and_bar(&mut rt, FoldOp::Min);

    let e1 = rt.spawn(enemy, vec![(f_hp, Value::Int(5))]);
    let _e2 = rt.spawn(enemy, vec![(f_hp, Value::Int(10))]);
    let _e3 = rt.spawn(enemy, vec![(f_hp, Value::Int(20))]);
    let b0 = rt.alive(bar)[0];
    rt.step();
    assert_eq!(rt.read(b0, f_agg), Value::Float(5.0));

    // 最弱成员死亡（外部 destroy，无对 hp 的写）
    rt.destroy(e1);
    rt.step(); // 死亡撤销标脏的 fold 重投递收缩后的 min
    assert_eq!(rt.read(b0, f_agg), Value::Float(10.0));
}

/// max 对称：最大成员死亡后收缩到次大值。
#[test]
fn max_recovers_after_member_death() {
    let mut rt = Runtime::new();
    let (enemy, f_hp, f_agg, bar) = enemy_and_bar(&mut rt, FoldOp::Max);

    let _e1 = rt.spawn(enemy, vec![(f_hp, Value::Int(5))]);
    let _e2 = rt.spawn(enemy, vec![(f_hp, Value::Int(10))]);
    let e3 = rt.spawn(enemy, vec![(f_hp, Value::Int(20))]);
    let b0 = rt.alive(bar)[0];
    rt.step();
    assert_eq!(rt.read(b0, f_agg), Value::Float(20.0));

    rt.destroy(e3);
    rt.step();
    assert_eq!(rt.read(b0, f_agg), Value::Float(10.0));
}

/// 全体成员死亡 → 空 fold，min 交付 Null（聚合彻底收缩）。
#[test]
fn min_empties_to_null_when_all_die() {
    let mut rt = Runtime::new();
    let (enemy, f_hp, f_agg, bar) = enemy_and_bar(&mut rt, FoldOp::Min);

    let e1 = rt.spawn(enemy, vec![(f_hp, Value::Int(5))]);
    let e2 = rt.spawn(enemy, vec![(f_hp, Value::Int(10))]);
    let b0 = rt.alive(bar)[0];
    rt.step();
    assert_eq!(rt.read(b0, f_agg), Value::Float(5.0));

    rt.destroy(e1);
    rt.destroy(e2);
    rt.step();
    assert_eq!(rt.read(b0, f_agg), Value::Null);
}

/// sum 是当前成员值的增量视图；成员死亡后必须撤销其数值贡献。
#[test]
fn sum_removes_dead_member() {
    let mut rt = Runtime::new();
    let (enemy, f_hp, f_agg, bar) = enemy_and_bar(&mut rt, FoldOp::Sum);

    let e1 = rt.spawn(enemy, vec![(f_hp, Value::Int(5))]);
    let _e2 = rt.spawn(enemy, vec![(f_hp, Value::Int(10))]);
    let b0 = rt.alive(bar)[0];
    rt.step();
    assert_eq!(rt.read(b0, f_agg), Value::Float(15.0));

    rt.destroy(e1);
    rt.step();
    assert_eq!(rt.read(b0, f_agg), Value::Float(10.0));
}

/// count 同样是当前贡献 cell 数，不是历史写事件数。
#[test]
fn count_removes_dead_member() {
    let mut rt = Runtime::new();
    let (enemy, f_hp, f_agg, bar) = enemy_and_bar(&mut rt, FoldOp::Count);

    let e1 = rt.spawn(enemy, vec![(f_hp, Value::Int(5))]);
    let _e2 = rt.spawn(enemy, vec![(f_hp, Value::Int(10))]);
    let b0 = rt.alive(bar)[0];
    rt.step();
    assert_eq!(rt.read(b0, f_agg), Value::Int(2));

    rt.destroy(e1);
    rt.step();
    assert_eq!(rt.read(b0, f_agg), Value::Int(1));
}

/// 同帧先写 folded 字段再死亡，死者不能被 pending 字段写重新插回聚合。
#[test]
fn min_does_not_readd_dead_member_with_pending_write() {
    let mut rt = Runtime::new();
    let (enemy, f_hp, f_agg, bar) = enemy_and_bar(&mut rt, FoldOp::Min);

    let e1 = rt.spawn(enemy, vec![(f_hp, Value::Int(5))]);
    let _e2 = rt.spawn(enemy, vec![(f_hp, Value::Int(10))]);
    let b0 = rt.alive(bar)[0];
    rt.step();
    assert_eq!(rt.read(b0, f_agg), Value::Float(5.0));

    rt.debug_write(e1, f_hp, Value::Int(1));
    rt.destroy(e1);
    rt.step();
    assert_eq!(rt.read(b0, f_agg), Value::Float(10.0));
}

/// fold 贡献键必须是 cell，而不是只按 InstanceId；同实例多字段都要保留。
#[test]
fn min_keeps_distinct_cells_on_same_instance() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![field("a", 0), field("b", 0)], false);
    let board = rt.register_entity_type("Board", vec![field("agg", ())], true);
    let f_a = rt.field(unit, "a");
    let f_b = rt.field(unit, "b");
    let f_agg = rt.field(board, "agg");

    rt.register_calculation(
        "min_any",
        board,
        Predicate::new(
            Scope::Or(
                Box::new(type_scope(unit, f_a)),
                Box::new(type_scope(unit, f_b)),
            ),
            Cond::True,
            Delivery::Fold(FoldOp::Min),
        ),
        &[f_agg],
        Box::new(move |ctx, input| ctx.write(f_agg, input.agg().clone())),
    )
    .unwrap();

    let _u = rt.spawn(unit, vec![(f_a, Value::Int(5)), (f_b, Value::Int(10))]);
    let b0 = rt.alive(board)[0];
    rt.step();
    assert_eq!(rt.read(b0, f_agg), Value::Float(5.0));
}

/// inst(ref, field) fold 的目标死亡后也要撤销；仅按 type_scope 登记会漏掉这里。
#[test]
fn inst_fold_clears_when_target_dies() {
    let mut rt = Runtime::new();
    let enemy = rt.register_entity_type("Enemy", vec![field("hp", 0)], false);
    let watcher = rt.register_entity_type(
        "Watcher",
        vec![FieldDef::reference("target"), field("agg", ())],
        true,
    );
    let f_hp = rt.field(enemy, "hp");
    let f_target = rt.field(watcher, "target");
    let f_agg = rt.field(watcher, "agg");
    let w0 = rt.alive(watcher)[0];

    rt.register_calculation(
        "watch_target",
        watcher,
        Predicate::new(
            inst(f_target, f_hp),
            Cond::True,
            Delivery::Fold(FoldOp::Min),
        ),
        &[f_agg],
        Box::new(move |ctx, input| ctx.write(f_agg, input.agg().clone())),
    )
    .unwrap();

    let e = rt.spawn(enemy, vec![(f_hp, Value::Int(5))]);
    rt.debug_write(w0, f_target, Value::Ref(e));
    rt.step();
    assert_eq!(rt.read(w0, f_agg), Value::Float(5.0));

    rt.destroy(e);
    rt.step();
    assert_eq!(rt.read(w0, f_target), Value::Null);
    assert_eq!(rt.read(w0, f_agg), Value::Null);
}

/// in-sim 自决死亡（destroy_self，§6.3 唯一销毁入口）同样触发撤销。
#[test]
fn min_recovers_after_in_sim_self_destroy() {
    let mut rt = Runtime::new();
    let enemy =
        rt.register_entity_type("Enemy", vec![field("hp", 0), field("doomed", false)], false);
    let bar = rt.register_entity_type("Bar", vec![field("agg", ())], true);
    let f_hp = rt.field(enemy, "hp");
    let f_doomed = rt.field(enemy, "doomed");
    let f_agg = rt.field(bar, "agg");

    rt.register_calculation(
        "weakest",
        bar,
        Predicate::new(
            type_scope(enemy, f_hp),
            Cond::True,
            Delivery::Fold(FoldOp::Min),
        ),
        &[f_agg],
        Box::new(move |ctx, input| ctx.write(f_agg, input.agg().clone())),
    )
    .unwrap();
    // 自决：doomed 置 true 时自己写 _alive=false（§6.3）。
    rt.register_calculation(
        "reaper",
        enemy,
        Predicate::new(
            pce::predicate::own(f_doomed),
            Cond::Became(Value::Bool(true)),
            Delivery::Each(vec![]),
        ),
        &[],
        Box::new(move |ctx, _| ctx.destroy_self()),
    )
    .unwrap();

    let e1 = rt.spawn(enemy, vec![(f_hp, Value::Int(5))]);
    let _e2 = rt.spawn(enemy, vec![(f_hp, Value::Int(10))]);
    let b0 = rt.alive(bar)[0];
    rt.step();
    assert_eq!(rt.read(b0, f_agg), Value::Float(5.0));

    rt.debug_write(e1, f_doomed, Value::Bool(true)); // 触发自决
    rt.step(); // reaper 运行 → e1 写 _alive=false → 帧界结算 + 撤销
    rt.step(); // 撤销标脏的 fold 重投递
    assert_eq!(rt.read(b0, f_agg), Value::Float(10.0));
}
