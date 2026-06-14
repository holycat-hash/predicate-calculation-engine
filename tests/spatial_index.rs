//! 空间 / 范围广相位索引（碰撞、AoI、范围查询）。
//! 直接验证 [`SpatialGrid`] 三类查询，并端到端验证 §6.1「物化为索引实体」上游集成：
//! 索引实体 batch 订阅 position 写 → 维护网格 → 把查询结果写进自己字段供下游订阅。

use std::sync::{Arc, Mutex};

use pce::entity::FIELD_ALIVE;
use pce::predicate::type_scope;
use pce::{Cond, Delivery, FieldDef, Predicate, Proj, Runtime, Scope, SpatialGrid, Value};

fn field(name: &str, default: impl Into<Value>) -> FieldDef {
    FieldDef::new(name, default.into())
}

/// 半径查询（AoI）、AABB 范围查询、广相位候选对、增量移动与移除。
#[test]
fn grid_queries_and_incremental_update() {
    // InstanceId 不可外部构造（含隐藏代际号），故从 runtime spawn 取真实 id。
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![field("p", 0)], false);
    let a = rt.spawn(unit, vec![]);
    let b = rt.spawn(unit, vec![]);
    let c = rt.spawn(unit, vec![]);

    let mut g = SpatialGrid::new(10.0);
    g.update(a, 0.0, 0.0);
    g.update(b, 1.0, 0.0); // 同格 (0,0)
    g.update(c, 100.0, 100.0); // 远处格 (10,10)
    assert_eq!(g.len(), 3);

    // 半径查询（AoI）：原点 5 半径内 = a, b
    let near = g.query_radius(0.0, 0.0, 5.0);
    assert_eq!(near, vec_sorted(&[a, b]));

    // AABB 范围查询
    let inside = g.query_aabb(-1.0, -1.0, 2.0, 2.0);
    assert_eq!(inside, vec_sorted(&[a, b]));

    // 广相位候选对：a,b 同格 → 一对；c 远处无对
    let pairs = g.candidate_pairs();
    assert_eq!(pairs.len(), 1);
    assert!(pairs.contains(&ordered(a, b)));

    // 增量移动 b 到 c 的格 → 候选对变为 (b,c)
    g.update(b, 100.0, 100.0);
    let pairs2 = g.candidate_pairs();
    assert_eq!(pairs2.len(), 1);
    assert!(pairs2.contains(&ordered(b, c)));

    // 移除 c
    g.remove(c);
    assert_eq!(g.len(), 2);
    assert!(g.candidate_pairs().is_empty());
}

/// 相邻格（非同格）也算候选对：广相位 8 邻域扫描。
#[test]
fn grid_pairs_across_adjacent_cells() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![field("p", 0)], false);
    let a = rt.spawn(unit, vec![]);
    let b = rt.spawn(unit, vec![]);

    let mut g = SpatialGrid::new(10.0);
    g.update(a, 9.0, 9.0); // 格 (0,0)
    g.update(b, 11.0, 11.0); // 相邻格 (1,1)
    let pairs = g.candidate_pairs();
    assert_eq!(pairs.len(), 1, "相邻格应产出候选对（窄相位由调用方判距）");
    assert!(pairs.contains(&ordered(a, b)));
}

/// 半径查询拒绝负半径；极端边界 cell 的邻格扫描不能溢出。
#[test]
fn grid_rejects_invalid_radius_and_handles_edge_cells() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![field("p", 0)], false);
    let a = rt.spawn(unit, vec![]);

    let mut g = SpatialGrid::new(1.0);
    g.update(a, i32::MAX as f64, 0.0);
    assert!(g.query_radius(i32::MAX as f64, 0.0, -1.0).is_empty());
    assert!(g.candidate_pairs().is_empty());
}

/// NaN / Infinity 坐标不能落入普通桶形成 broad-phase 假候选。
#[test]
#[should_panic(expected = "SpatialGrid 坐标")]
fn grid_rejects_nan_coordinates() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![field("p", 0)], false);
    let a = rt.spawn(unit, vec![]);

    let mut g = SpatialGrid::new(10.0);
    g.update(a, f64::NAN, 0.0);
}

/// 上游集成（§6.1）：索引实体 batch 订阅 position 写，维护网格，把候选对数写进
/// 自己字段；这是把空间查询折叠进四层、不进谓词词汇的标准走法。
#[test]
fn materialized_index_entity_publishes_pairs() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![field("pos", ())], false);
    let grid_e = rt.register_entity_type("Grid", vec![field("pair_count", 0)], true);
    let f_pos = rt.field(unit, "pos");
    let f_pairs = rt.field(grid_e, "pair_count");

    // 索引私有增量态：网格在 calc 闭包里持有（单索引实例每帧只跑一次，无并行竞争）。
    let grid = Arc::new(Mutex::new(SpatialGrid::new(10.0)));
    let grid_calc = grid.clone();

    rt.register_calculation(
        "grid_update",
        grid_e,
        // §7 示例 3：type(Unit, pos) + type(Unit, _alive) batch deliver(writer_id, new)
        Predicate::new(
            Scope::Or(
                Box::new(type_scope(unit, f_pos)),
                Box::new(type_scope(unit, FIELD_ALIVE)),
            ),
            Cond::True,
            Delivery::Batch(vec![Proj::WriterId, Proj::New(vec![])]),
        ),
        &[f_pairs],
        Box::new(move |ctx, input| {
            let mut g = grid_calc.lock().unwrap();
            for row in input.rows() {
                let wid = row[0].as_ref_id().unwrap();
                match &row[1] {
                    Value::Bool(false) => g.remove(wid),
                    Value::Map(_) => {
                        let x = row[1].get_path(&["x".to_string()]).as_f64().unwrap();
                        let y = row[1].get_path(&["y".to_string()]).as_f64().unwrap();
                        g.update(wid, x, y);
                    }
                    _ => {}
                }
            }
            ctx.write(f_pairs, g.candidate_pairs().len() as i64);
        }),
    )
    .unwrap();

    let u1 = rt.spawn(unit, vec![]);
    let u2 = rt.spawn(unit, vec![]);
    let u3 = rt.spawn(unit, vec![]);
    let g0 = rt.alive(grid_e)[0];
    rt.step(); // 消化 spawn（未写 pos，无 position 命中）

    let pos = |x: f64, y: f64| Value::map([("x", Value::Float(x)), ("y", Value::Float(y))]);
    rt.debug_write(u1, f_pos, pos(1.0, 1.0)); // 格 (0,0)
    rt.debug_write(u2, f_pos, pos(2.0, 2.0)); // 同格 (0,0)
    rt.debug_write(u3, f_pos, pos(80.0, 80.0)); // 远处格
    rt.step(); // batch 一次性喂入网格，发布候选对数

    assert_eq!(rt.read(g0, f_pairs), Value::Int(1)); // 仅 u1-u2 一对

    rt.destroy(u2);
    rt.step();
    assert_eq!(rt.read(g0, f_pairs), Value::Int(0)); // 死亡写驱动 remove
}

fn vec_sorted(ids: &[pce::InstanceId]) -> Vec<pce::InstanceId> {
    let mut v = ids.to_vec();
    v.sort_by_key(|i| (i.ty.0, i.id));
    v
}

fn ordered(a: pce::InstanceId, b: pce::InstanceId) -> (pce::InstanceId, pce::InstanceId) {
    if (a.ty.0, a.id) <= (b.ty.0, b.id) {
        (a, b)
    } else {
        (b, a)
    }
}
