//! 核心 API 与白送优化 / C 档位的行为验证：
//! 共享阈值表、值桶、ECS 快路、fold min 多重集、RowPolicy（C6）、
//! Canonical 确定性（C4）、Strict 检测（C5/C2/C1）、免费 profiler。

use pce::predicate::{lit, new_val, own, own_field, type_scope};
use pce::{
    CalcId, CalcOptions, CmpOp, Cond, Ctx, Delivery, Detect, Determinism, Dir, EntityTypeId, Expr,
    FieldDef, FieldId, FoldOp, Input, Predicate, Proj, RowPolicy, Runtime, Scope, Tier, ValRef,
    Value,
};

fn field(name: &str, default: impl Into<Value>) -> FieldDef {
    FieldDef::new(name, default.into())
}

fn entity(rt: &mut Runtime, name: &str, fields: Vec<FieldDef>) -> EntityTypeId {
    rt.register_entity_type(name, fields, false)
}

fn compact_entity(rt: &mut Runtime, name: &str, fields: Vec<FieldDef>) -> EntityTypeId {
    rt.register_entity_type_with(name, fields, false, RowPolicy::Compact)
}

fn singleton(rt: &mut Runtime, name: &str, fields: Vec<FieldDef>) -> EntityTypeId {
    rt.register_entity_type(name, fields, true)
}

fn val(v: impl Into<Value>) -> Expr {
    lit(v.into())
}

fn new_path(path: &[&str]) -> Expr {
    Expr::Val(ValRef::New(path.iter().map(|s| s.to_string()).collect()))
}

fn new_proj() -> Proj {
    Proj::New(vec![])
}

fn new_path_proj(path: &[&str]) -> Proj {
    Proj::New(path.iter().map(|s| s.to_string()).collect())
}

fn register(
    rt: &mut Runtime,
    name: &str,
    ty: EntityTypeId,
    pred: Predicate,
    writes: &[FieldId],
    f: impl Fn(&mut Ctx, &Input) + Send + Sync + 'static,
) -> Result<CalcId, String> {
    rt.register_calculation(name, ty, pred, writes, Box::new(f))
}

fn register_opt(
    rt: &mut Runtime,
    name: &str,
    ty: EntityTypeId,
    pred: Predicate,
    writes: &[FieldId],
    opts: CalcOptions,
    f: impl Fn(&mut Ctx, &Input) + Send + Sync + 'static,
) -> Result<CalcId, String> {
    rt.register_calculation_opt(name, ty, pred, writes, opts, Box::new(f))
}

/// type scope + 常量阈值条件 → 共享排序阈值表（O(log s + k)）。
#[test]
fn threshold_table_routes_constant_cmp() {
    let mut rt = Runtime::new();
    let unit = entity(&mut rt, "Unit", vec![field("hp", 0)]);
    let watcher = singleton(&mut rt, "Watcher", vec![field("hits", 0)]);
    let f_hp = rt.field(unit, "hp");
    let f_hits = rt.field(watcher, "hits");

    register(
        &mut rt,
        "low_hp",
        watcher,
        Predicate::new(
            type_scope(unit, f_hp),
            Cond::Cmp(new_val(), CmpOp::Lt, val(10)),
            Delivery::Each(vec![new_proj()]),
        ),
        &[f_hits],
        move |ctx, _| {
            let n = ctx.read_own(f_hits).as_i64().unwrap();
            ctx.write(f_hits, n + 1);
        },
    )
    .unwrap();

    let u = rt.spawn(unit, vec![]);
    let w0 = rt.alive(watcher)[0];
    rt.step(); // 消化 spawn 写集；init 未写 hp，因此不会命中 hp 订阅。

    rt.debug_write(u, f_hp, Value::Int(5)); // 5 < 10 命中
    rt.step();
    assert_eq!(rt.read(w0, f_hits), Value::Int(1));

    rt.debug_write(u, f_hp, Value::Int(20)); // 不命中
    rt.step();
    assert_eq!(rt.read(w0, f_hits), Value::Int(1));

    rt.debug_write(u, f_hp, Value::Int(9)); // 命中
    rt.step();
    assert_eq!(rt.read(w0, f_hits), Value::Int(2));
}

/// type scope + crossed(常量) → 阈值表区间查询；边沿语义保持。
#[test]
fn crossed_constant_uses_shared_table() {
    let mut rt = Runtime::new();
    let unit = entity(&mut rt, "Unit", vec![field("hp", 100)]);
    let watcher = singleton(&mut rt, "Watcher", vec![field("hits", 0)]);
    let f_hp = rt.field(unit, "hp");
    let f_hits = rt.field(watcher, "hits");

    register(
        &mut rt,
        "dip50",
        watcher,
        Predicate::new(
            type_scope(unit, f_hp),
            Cond::Crossed(val(50), Dir::Down),
            Delivery::Each(vec![]),
        ),
        &[f_hits],
        move |ctx, _| {
            let n = ctx.read_own(f_hits).as_i64().unwrap();
            ctx.write(f_hits, n + 1);
        },
    )
    .unwrap();

    let u = rt.spawn(unit, vec![]);
    let w0 = rt.alive(watcher)[0];
    rt.step();

    rt.debug_write(u, f_hp, Value::Int(60)); // 100→60 未穿 50
    rt.step();
    assert_eq!(rt.read(w0, f_hits), Value::Int(0));

    rt.debug_write(u, f_hp, Value::Int(40)); // 60→40 下穿
    rt.step();
    assert_eq!(rt.read(w0, f_hits), Value::Int(1));

    rt.debug_write(u, f_hp, Value::Int(30)); // 已在下方，不重复
    rt.step();
    assert_eq!(rt.read(w0, f_hits), Value::Int(1));
}

/// type scope + 常量等值 → 值桶（O(1) + k）；Int/Float 跨类型判等无假阴性。
#[test]
fn value_bucket_equality() {
    let mut rt = Runtime::new();
    let unit = entity(
        &mut rt,
        "Unit",
        vec![field("state", "idle"), field("code", 0)],
    );
    let watcher = singleton(
        &mut rt,
        "Watcher",
        vec![field("deaths", 0), field("threes", 0)],
    );
    let (f_state, f_code) = (rt.field(unit, "state"), rt.field(unit, "code"));
    let (f_deaths, f_threes) = (rt.field(watcher, "deaths"), rt.field(watcher, "threes"));

    register(
        &mut rt,
        "on_dead",
        watcher,
        Predicate::new(
            type_scope(unit, f_state),
            Cond::Cmp(new_val(), CmpOp::Eq, val("dead")),
            Delivery::Each(vec![]),
        ),
        &[f_deaths],
        move |ctx, _| {
            let n = ctx.read_own(f_deaths).as_i64().unwrap();
            ctx.write(f_deaths, n + 1);
        },
    )
    .unwrap();
    register(
        &mut rt,
        "on_three",
        watcher,
        Predicate::new(
            type_scope(unit, f_code),
            Cond::Cmp(new_val(), CmpOp::Eq, val(3)),
            Delivery::Each(vec![]),
        ),
        &[f_threes],
        move |ctx, _| {
            let n = ctx.read_own(f_threes).as_i64().unwrap();
            ctx.write(f_threes, n + 1);
        },
    )
    .unwrap();

    let u = rt.spawn(unit, vec![]);
    let w0 = rt.alive(watcher)[0];
    rt.step();

    rt.debug_write(u, f_state, Value::str("running"));
    rt.step();
    assert_eq!(rt.read(w0, f_deaths), Value::Int(0));

    rt.debug_write(u, f_state, Value::str("dead"));
    rt.step();
    assert_eq!(rt.read(w0, f_deaths), Value::Int(1));

    // Float(3.0) 写入必须命中 Int(3) 桶（val_eq 语义，探桶无假阴性）
    rt.debug_write(u, f_code, Value::Float(3.0));
    rt.step();
    assert_eq!(rt.read(w0, f_threes), Value::Int(1));
}

/// Clock 谓词退化为 ECS（白送优化）：type(Clock, frame) + 恒真 + each
/// → 跳过路由，稠密列遍历，每帧每存活实例触发一次。
#[test]
fn ecs_fast_path_runs_per_frame_per_instance() {
    let mut rt = Runtime::new();
    let unit = compact_entity(&mut rt, "Unit", vec![field("age", 0)]);
    let f_age = rt.field(unit, "age");
    let clock_scope = {
        let clock = rt.clock();
        type_scope(clock.ty, clock.f_frame)
    };

    register(
        &mut rt,
        "tick_age",
        unit,
        Predicate::new(clock_scope, Cond::True, Delivery::Each(vec![new_proj()])),
        &[f_age],
        move |ctx, _| {
            let n = ctx.read_own(f_age).as_i64().unwrap();
            ctx.write(f_age, n + 1);
        },
    )
    .unwrap();

    let a = rt.spawn(unit, vec![]);
    let b = rt.spawn(unit, vec![]);
    for _ in 0..5 {
        rt.step();
    }
    assert_eq!(rt.read(a, f_age), Value::Int(5));
    assert_eq!(rt.read(b, f_age), Value::Int(5));

    // 死亡后不再触发（稠密遍历只扫存活行）
    rt.destroy(b);
    rt.step();
    assert_eq!(rt.read(a, f_age), Value::Int(6));
    assert_eq!(rt.read(b, f_age), Value::Null); // 死实例读 Null
}

/// fold min 用多重集维护：最小成员值回升后正确收缩到次小值
/// （旧实现的运行 min 会卡死在历史最小）。
#[test]
fn fold_min_recovers_after_member_rises() {
    let mut rt = Runtime::new();
    let enemy = entity(&mut rt, "Enemy", vec![field("hp", 0)]);
    let bar = singleton(&mut rt, "Bar", vec![field("weakest", ())]);
    let f_hp = rt.field(enemy, "hp");
    let f_weakest = rt.field(bar, "weakest");

    register(
        &mut rt,
        "weakest",
        bar,
        Predicate::new(
            type_scope(enemy, f_hp),
            Cond::True,
            Delivery::Fold(FoldOp::Min),
        ),
        &[f_weakest],
        move |ctx, input| ctx.write(f_weakest, input.agg().clone()),
    )
    .unwrap();

    let e1 = rt.spawn(enemy, vec![(f_hp, Value::Int(5))]);
    let _e2 = rt.spawn(enemy, vec![(f_hp, Value::Int(10))]);
    let b0 = rt.alive(bar)[0];
    rt.step();
    assert_eq!(rt.read(b0, f_weakest), Value::Float(5.0));

    rt.debug_write(e1, f_hp, Value::Int(20)); // 最小成员回升
    rt.step();
    assert_eq!(rt.read(b0, f_weakest), Value::Float(10.0));
}

/// C6 压缩行：死亡 swap-remove 重映射后，幸存实例数据不动、
/// 旧 id 复用有代际防 ABA。
#[test]
fn compact_rows_remap_safely() {
    let mut rt = Runtime::new();
    let unit = compact_entity(&mut rt, "Unit", vec![field("tag", 0)]);
    let f_tag = rt.field(unit, "tag");

    let a = rt.spawn(unit, vec![(f_tag, Value::Int(1))]);
    let b = rt.spawn(unit, vec![(f_tag, Value::Int(2))]);
    let c = rt.spawn(unit, vec![(f_tag, Value::Int(3))]);
    rt.step();

    rt.destroy(b); // 中间行死亡 → 末行 c 搬入其位
    rt.step();
    assert_eq!(rt.alive(unit).len(), 2);
    assert_eq!(rt.read(a, f_tag), Value::Int(1));
    assert_eq!(rt.read(c, f_tag), Value::Int(3)); // 重映射后 id→row 间接仍正确
    assert_eq!(rt.read(b, f_tag), Value::Null);

    // id 复用：新实例可能拿到 b 的 id，但代际不同——旧 ref 不误指新住户
    let d = rt.spawn(unit, vec![(f_tag, Value::Int(4))]);
    rt.step();
    assert_eq!(rt.read(d, f_tag), Value::Int(4));
    assert_eq!(rt.read(b, f_tag), Value::Null);
}

/// C4 Canonical：batch 按 (writer, field) 规范序交付——买回确定性。
#[test]
fn canonical_batch_delivers_sorted() {
    let mut rt = Runtime::new();
    rt.set_determinism(Determinism::Canonical);
    let unit = entity(&mut rt, "Unit", vec![field("v", 0)]);
    let log = singleton(&mut rt, "Log", vec![field("order", "")]);
    let f_v = rt.field(unit, "v");
    let f_order = rt.field(log, "order");

    register(
        &mut rt,
        "collect",
        log,
        Predicate::new(
            type_scope(unit, f_v),
            Cond::True,
            Delivery::Batch(vec![new_proj()]),
        ),
        &[f_order],
        move |ctx, input| {
            let s: Vec<String> = input
                .rows()
                .iter()
                .map(|r| r[0].as_i64().unwrap().to_string())
                .collect();
            ctx.write(f_order, s.join(","));
        },
    )
    .unwrap();

    let a = rt.spawn(unit, vec![]);
    let b = rt.spawn(unit, vec![]);
    let c = rt.spawn(unit, vec![]);
    let l0 = rt.alive(log)[0];
    rt.step();

    // 倒序写入；Canonical 档下交付按 writer id 升序
    rt.debug_write(c, f_v, Value::Int(3));
    rt.debug_write(b, f_v, Value::Int(2));
    rt.debug_write(a, f_v, Value::Int(1));
    rt.step();
    assert_eq!(rt.read(l0, f_order), Value::str("1,2,3"));
}

/// C5 Strict：同字段被同一 calculation 多次运行写入不同值 → panic
/// （release 默认 Silent 静默折叠，§8 开放问题四的档位化答案）。
#[test]
#[should_panic(expected = "多次运行对同字段写入不同值")]
fn strict_detect_panics_on_conflicting_writes() {
    let mut rt = Runtime::new();
    rt.set_detect(Detect::Strict);
    let unit = entity(&mut rt, "Unit", vec![field("v", 0)]);
    let sink = singleton(&mut rt, "Sink", vec![field("last", 0)]);
    let f_v = rt.field(unit, "v");
    let f_last = rt.field(sink, "last");

    register(
        &mut rt,
        "copy_last",
        sink,
        Predicate::new(
            type_scope(unit, f_v),
            Cond::True,
            Delivery::Each(vec![new_proj()]),
        ),
        &[f_last],
        move |ctx, input| ctx.write(f_last, input.arg(0).clone()),
    )
    .unwrap();

    let a = rt.spawn(unit, vec![]);
    let b = rt.spawn(unit, vec![]);
    rt.step();
    rt.debug_write(a, f_v, Value::Int(7));
    rt.debug_write(b, f_v, Value::Int(9)); // 同帧两条命中 → 两次运行写不同值
    rt.step();
}

/// C2 读集声明 + Strict：越界 read_own 即 panic。
#[test]
#[should_panic(expected = "读了未声明字段")]
fn strict_detect_panics_on_undeclared_read() {
    let mut rt = Runtime::new();
    rt.set_detect(Detect::Strict);
    let unit = entity(&mut rt, "Unit", vec![field("a", 0), field("b", 0)]);
    let (f_a, f_b) = (rt.field(unit, "a"), rt.field(unit, "b"));

    register_opt(
        &mut rt,
        "oob_read",
        unit,
        Predicate::new(own(f_a), Cond::True, Delivery::Each(vec![])),
        &[f_b],
        CalcOptions {
            reads: Some(vec![f_a]),
            ..CalcOptions::default()
        },
        move |ctx, _| {
            let _ = ctx.read_own(f_b); // 声明只读 a，却读 b
            ctx.write(f_b, ctx.read_own(f_a));
        },
    )
    .unwrap();

    let u = rt.spawn(unit, vec![]);
    rt.step();
    rt.debug_write(u, f_a, Value::Int(1));
    rt.step();
}

/// C1 Kernel 档 + Strict：kernel 子集禁动态分配（spawn）。
#[test]
#[should_panic(expected = "Kernel 档")]
fn kernel_tier_forbids_spawn() {
    let mut rt = Runtime::new();
    rt.set_detect(Detect::Strict);
    let unit = entity(&mut rt, "Unit", vec![field("v", 0)]);
    let f_v = rt.field(unit, "v");

    register_opt(
        &mut rt,
        "bad_kernel",
        unit,
        Predicate::new(own(f_v), Cond::True, Delivery::Each(vec![])),
        &[f_v],
        CalcOptions {
            tier: Tier::Kernel,
            ..CalcOptions::default()
        },
        move |ctx, _| ctx.spawn(ctx.self_id().ty, vec![]),
    )
    .unwrap();

    let u = rt.spawn(unit, vec![]);
    rt.step();
    rt.debug_write(u, f_v, Value::Int(1));
    rt.step();
}

/// 免费 profiler（D2 买单）：写频与触发计数零边际产出。
#[test]
fn profiler_counts_writes_and_triggers() {
    let mut rt = Runtime::new();
    let unit = entity(&mut rt, "Unit", vec![field("v", 0)]);
    let f_v = rt.field(unit, "v");

    let calc = register(
        &mut rt,
        "noop",
        unit,
        Predicate::new(own(f_v), Cond::True, Delivery::Each(vec![])),
        &[],
        |_, _| {},
    )
    .unwrap();

    let u = rt.spawn(unit, vec![]);
    rt.step();
    rt.debug_write(u, f_v, Value::Int(1));
    rt.step();
    rt.debug_write(u, f_v, Value::Int(2));
    rt.step();

    let p = rt.profile();
    assert_eq!(p.writes(unit, f_v), 2);
    assert_eq!(p.triggers(calc), 2);
    assert!(p.frames >= 3);
    assert!(!p.hot_cells().is_empty());
}

/// 核心注册 API 的 D1 校验：冲突经 Result 报错（不 panic）。
#[test]
fn core_registration_reports_d1_conflict() {
    let mut rt = Runtime::new();
    let unit = entity(&mut rt, "Unit", vec![field("v", 0)]);
    let f_v = rt.field(unit, "v");
    register(
        &mut rt,
        "w1",
        unit,
        Predicate::new(own(f_v), Cond::True, Delivery::Each(vec![])),
        &[f_v],
        |_, _| {},
    )
    .unwrap();
    let err = register(
        &mut rt,
        "w2",
        unit,
        Predicate::new(own(f_v), Cond::True, Delivery::Each(vec![])),
        &[f_v],
        |_, _| {},
    )
    .unwrap_err();
    assert!(err.contains("D1"), "{err}");
}

/// scope 并（|）与活阈值条件（own 字段）走诚实退化路径仍正确。
#[test]
fn live_threshold_and_scope_union() {
    let mut rt = Runtime::new();
    let unit = entity(
        &mut rt,
        "Unit",
        vec![
            field("hp", 100),
            field("hp_max", 100),
            field("mp", 100),
            field("weak", false),
        ],
    );
    let (f_hp, f_hp_max, f_mp, f_weak) = (
        rt.field(unit, "hp"),
        rt.field(unit, "hp_max"),
        rt.field(unit, "mp"),
        rt.field(unit, "weak"),
    );

    // hp 或 mp 任一写入，且 new < 30% 上限（活阈值）
    register(
        &mut rt,
        "weak_flag",
        unit,
        Predicate::new(
            Scope::Or(Box::new(own(f_hp)), Box::new(own(f_mp))),
            Cond::Cmp(
                new_val(),
                CmpOp::Lt,
                Expr::Mul(Box::new(own_field(f_hp_max)), Box::new(val(0.3))),
            ),
            Delivery::Each(vec![]),
        ),
        &[f_weak],
        move |ctx, _| ctx.write(f_weak, true),
    )
    .unwrap();

    let u = rt.spawn(unit, vec![]);
    rt.step();
    rt.debug_write(u, f_hp, Value::Int(50)); // 50 ≥ 30 不命中
    rt.step();
    assert_eq!(rt.read(u, f_weak), Value::Bool(false));
    rt.debug_write(u, f_mp, Value::Int(20)); // 20 < 30 命中（经 mp 一侧）
    rt.step();
    assert_eq!(rt.read(u, f_weak), Value::Bool(true));
}

/// `new.path = self` 仍由核心 AST 表达，并在注册期走 ref 点查快路。
#[test]
fn self_ref_fast_path_from_core_ast() {
    let mut rt = Runtime::new();
    let unit = entity(&mut rt, "Unit", vec![field("hp", 100)]);
    let attacker = entity(&mut rt, "Attacker", vec![field("attack_out", ())]);
    let f_hp = rt.field(unit, "hp");
    let f_attack_out = rt.field(attacker, "attack_out");

    register(
        &mut rt,
        "take_damage",
        unit,
        Predicate::new(
            type_scope(attacker, f_attack_out),
            Cond::Cmp(new_path(&["target"]), CmpOp::Eq, Expr::Val(ValRef::SelfRef)),
            Delivery::Each(vec![new_path_proj(&["dmg"])]),
        ),
        &[f_hp],
        move |ctx, input| {
            let hp = ctx.read_own(f_hp).as_i64().unwrap_or(0);
            let dmg = input.arg(0).as_i64().unwrap_or(0);
            ctx.write(f_hp, hp - dmg);
        },
    )
    .unwrap();

    let u1 = rt.spawn(unit, vec![(f_hp, Value::Int(100))]);
    let u2 = rt.spawn(unit, vec![(f_hp, Value::Int(100))]);
    let a = rt.spawn(attacker, vec![]);
    rt.step();

    rt.debug_write(
        a,
        f_attack_out,
        Value::map([("target", Value::Ref(u2)), ("dmg", Value::Int(7))]),
    );
    rt.step();

    assert_eq!(rt.read(u1, f_hp), Value::Int(100));
    assert_eq!(rt.read(u2, f_hp), Value::Int(93));
}
