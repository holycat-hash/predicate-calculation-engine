//! 核心 API 与白送优化 / C 档位的行为验证：
//! 共享阈值表、值桶、ECS 快路、fold min 多重集、RowPolicy（C6）、
//! Canonical 确定性（C4）、Strict 检测（C5/C2/C1）、免费 profiler。

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use pce::predicate::{lit, new_val, own, own_field, type_scope};
use pce::{
    CalcId, CalcOptions, CmpOp, Cond, Ctx, Delivery, Detect, Determinism, Dir, EntityTypeId, Expr,
    FieldDef, FieldId, FoldOp, Input, KernelBackend, KernelBatch, KernelBatchOutput, KernelColumn,
    KernelColumnWrite, KernelIr, KernelOp, KernelWrite, Predicate, Proj, Residency, RowPolicy,
    Runtime, ScalarKernelBackend, Scope, Tier, ValRef, Value,
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

fn and_scope(fields: &[FieldId]) -> Scope {
    let mut it = fields.iter().copied().map(own);
    let first = it.next().unwrap();
    it.fold(first, |acc, f| Scope::And(Box::new(acc), Box::new(f)))
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

#[test]
fn threshold_table_ignores_nan_constant_without_poisoning_order() {
    let mut rt = Runtime::new();
    let unit = entity(&mut rt, "Unit", vec![field("v", 0.0)]);
    let watch = singleton(
        &mut rt,
        "Watch",
        vec![field("gt10", 0), field("nan", 0), field("gt5", 0)],
    );
    let f_v = rt.field(unit, "v");
    let gt10 = rt.field(watch, "gt10");
    let nan = rt.field(watch, "nan");
    let gt5 = rt.field(watch, "gt5");

    for (name, threshold, out) in [
        ("gt10", 10.0, gt10),
        ("nan", f64::NAN, nan),
        ("gt5", 5.0, gt5),
    ] {
        register(
            &mut rt,
            name,
            watch,
            Predicate::new(
                type_scope(unit, f_v),
                Cond::Cmp(new_val(), CmpOp::Gt, val(threshold)),
                Delivery::Each(vec![]),
            ),
            &[out],
            move |ctx, _| {
                let n = ctx.read_own(out).as_i64().unwrap_or(0);
                ctx.write(out, n + 1);
            },
        )
        .unwrap();
    }

    let u = rt.spawn(unit, vec![]);
    let w = rt.alive(watch)[0];
    rt.step();
    rt.debug_write(u, f_v, Value::Float(20.0));
    rt.step();

    assert_eq!(rt.read(w, gt10), Value::Int(1));
    assert_eq!(rt.read(w, gt5), Value::Int(1));
    assert_eq!(rt.read(w, nan), Value::Int(0));
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

#[test]
fn fold_sum_treats_nan_as_no_contribution_and_recovers() {
    let mut rt = Runtime::new();
    let enemy = entity(&mut rt, "Enemy", vec![field("hp", 0.0)]);
    let bar = singleton(&mut rt, "Bar", vec![field("total", ())]);
    let f_hp = rt.field(enemy, "hp");
    let f_total = rt.field(bar, "total");

    register(
        &mut rt,
        "total",
        bar,
        Predicate::new(
            type_scope(enemy, f_hp),
            Cond::True,
            Delivery::Fold(FoldOp::Sum),
        ),
        &[f_total],
        move |ctx, input| ctx.write(f_total, input.agg().clone()),
    )
    .unwrap();

    let e = rt.spawn(enemy, vec![(f_hp, Value::Float(1.0))]);
    let b0 = rt.alive(bar)[0];
    rt.step();
    assert_eq!(rt.read(b0, f_total), Value::Float(1.0));

    rt.debug_write(e, f_hp, Value::Float(f64::NAN));
    rt.step();
    assert_eq!(rt.read(b0, f_total), Value::Float(0.0));

    rt.debug_write(e, f_hp, Value::Float(2.0));
    rt.step();
    assert_eq!(rt.read(b0, f_total), Value::Float(2.0));
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
/// （release 默认 Silent 静默折叠，§2 检测档位 Detect；原 §8 开放问题四，已解决）。
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

/// C1 Kernel 档：必须提供 kernel IR，才能把 D4 从契约升级为机检。
#[test]
fn kernel_tier_requires_kernel_ir() {
    let mut rt = Runtime::new();
    rt.set_detect(Detect::Strict);
    let unit = entity(&mut rt, "Unit", vec![field("v", 0)]);
    let f_v = rt.field(unit, "v");

    let err = register_opt(
        &mut rt,
        "bad_kernel",
        unit,
        Predicate::new(own(f_v), Cond::True, Delivery::Each(vec![])),
        &[f_v],
        CalcOptions {
            tier: Tier::Kernel,
            ..CalcOptions::default()
        },
        move |ctx, _| ctx.write(f_v, 1),
    )
    .unwrap_err();
    assert!(err.contains("kernel IR"), "{err}");
}

/// Kernel IR 默认 backend：与等价闭包语义一致；有 IR 时不调用闭包 fallback。
#[test]
fn kernel_ir_interpreter_matches_closure_path() {
    let mut rt = Runtime::new();
    let unit = entity(
        &mut rt,
        "Unit",
        vec![
            field("src", 0),
            field("factor", 3),
            field("closure_out", 0.0),
            field("closure_big", false),
            field("ir_out", 0.0),
            field("ir_big", false),
        ],
    );
    let src = rt.field(unit, "src");
    let factor = rt.field(unit, "factor");
    let closure_out = rt.field(unit, "closure_out");
    let closure_big = rt.field(unit, "closure_big");
    let ir_out = rt.field(unit, "ir_out");
    let ir_big = rt.field(unit, "ir_big");

    register(
        &mut rt,
        "closure_scale",
        unit,
        Predicate::new(own(src), Cond::True, Delivery::Each(vec![new_proj()])),
        &[closure_out, closure_big],
        move |ctx, input| {
            let scaled = input.arg(0).as_f64().unwrap() * ctx.read_own(factor).as_f64().unwrap();
            ctx.write(closure_out, scaled);
            ctx.write(closure_big, scaled > 10.0);
        },
    )
    .unwrap();

    register_opt(
        &mut rt,
        "ir_scale",
        unit,
        Predicate::new(own(src), Cond::True, Delivery::Each(vec![new_proj()])),
        &[ir_out, ir_big],
        CalcOptions {
            reads: Some(vec![factor]),
            kernel_ir: Some(KernelIr::new(vec![
                KernelWrite::new(
                    ir_out,
                    vec![
                        KernelOp::InputArg(0),
                        KernelOp::ReadOwn(factor),
                        KernelOp::Mul,
                    ],
                ),
                KernelWrite::new(
                    ir_big,
                    vec![
                        KernelOp::InputArg(0),
                        KernelOp::ReadOwn(factor),
                        KernelOp::Mul,
                        KernelOp::Const(Value::Float(10.0)),
                        KernelOp::Cmp(CmpOp::Gt),
                    ],
                ),
            ])),
            ..CalcOptions::default()
        },
        move |_, _| panic!("kernel IR path must not call closure fallback"),
    )
    .unwrap();

    let u = rt.spawn(unit, vec![(factor, Value::Int(3))]);
    rt.step();
    rt.debug_write(u, src, Value::Int(5));
    rt.step();

    assert_eq!(rt.read(u, closure_out), Value::Float(15.0));
    assert_eq!(rt.read(u, ir_out), rt.read(u, closure_out));
    assert_eq!(rt.read(u, closure_big), Value::Bool(true));
    assert_eq!(rt.read(u, ir_big), rt.read(u, closure_big));
}

struct CountingGpuBackend {
    calls: Arc<AtomicUsize>,
    lanes: Arc<AtomicUsize>,
}

struct KillBackend;

impl KernelBackend for KillBackend {
    fn name(&self) -> &'static str {
        "kill-backend"
    }

    fn run(&self, _ir: &KernelIr, batch: KernelBatch<'_>) -> KernelBatchOutput {
        KernelBatchOutput::new(vec![KernelColumnWrite::new(
            pce::entity::FIELD_ALIVE,
            KernelColumn::Bool(vec![false; batch.lane_count()]),
        )])
    }
}

impl KernelBackend for CountingGpuBackend {
    fn name(&self) -> &'static str {
        "counting-gpu"
    }

    fn supports_residency(&self, residency: Residency) -> bool {
        residency == Residency::Gpu
    }

    fn run(&self, ir: &KernelIr, batch: KernelBatch<'_>) -> KernelBatchOutput {
        assert_eq!(batch.residency(), Residency::Gpu);
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.lanes.fetch_add(batch.lane_count(), Ordering::SeqCst);
        let backend = ScalarKernelBackend;
        backend.run(ir, batch)
    }
}

/// Kernel IR 第 3+4 步：同一 calc 的触发按 SoA batch 交给可插拔 backend，
/// C3 residency pin 参与 backend 选择；默认 scalar backend 可作为精确 fallback。
#[test]
fn kernel_backend_receives_grouped_soa_batch() {
    let mut rt = Runtime::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let lanes = Arc::new(AtomicUsize::new(0));
    rt.register_kernel_backend(Box::new(CountingGpuBackend {
        calls: calls.clone(),
        lanes: lanes.clone(),
    }));
    assert_eq!(rt.kernel_backend_names()[0], "counting-gpu");
    assert!(rt.kernel_backend_names().contains(&"scalar-soa"));

    let unit = entity(
        &mut rt,
        "Unit",
        vec![field("src", 0), field("factor", 1), field("out", 0.0)],
    );
    let src = rt.field(unit, "src");
    let factor = rt.field(unit, "factor");
    let out = rt.field(unit, "out");

    register_opt(
        &mut rt,
        "gpu_scale",
        unit,
        Predicate::new(own(src), Cond::True, Delivery::Each(vec![new_proj()])),
        &[out],
        CalcOptions {
            reads: Some(vec![factor]),
            tier: Tier::Kernel,
            residency: Residency::Gpu,
            kernel_ir: Some(KernelIr::new(vec![KernelWrite::new(
                out,
                vec![
                    KernelOp::InputArg(0),
                    KernelOp::ReadOwn(factor),
                    KernelOp::Mul,
                ],
            )])),
        },
        move |_, _| panic!("kernel backend path must not call closure fallback"),
    )
    .unwrap();

    let a = rt.spawn(unit, vec![(factor, Value::Int(2))]);
    let b = rt.spawn(unit, vec![(factor, Value::Int(3))]);
    let c = rt.spawn(unit, vec![(factor, Value::Int(4))]);
    rt.step();

    rt.debug_write(a, src, Value::Int(5));
    rt.debug_write(b, src, Value::Int(6));
    rt.debug_write(c, src, Value::Int(7));
    rt.step();

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "one backend call per calc group"
    );
    assert_eq!(
        lanes.load(Ordering::SeqCst),
        3,
        "three triggers become three lanes"
    );
    assert_eq!(rt.read(a, out), Value::Float(10.0));
    assert_eq!(rt.read(b, out), Value::Float(18.0));
    assert_eq!(rt.read(c, out), Value::Float(28.0));
}

#[test]
#[should_panic(expected = "forbidden _alive")]
fn kernel_backend_cannot_smuggle_lifecycle_writes() {
    let mut rt = Runtime::new();
    rt.set_detect(Detect::Strict);
    rt.register_kernel_backend(Box::new(KillBackend));
    let unit = entity(&mut rt, "Unit", vec![field("src", 0), field("out", 0)]);
    let src = rt.field(unit, "src");
    let out = rt.field(unit, "out");
    register_opt(
        &mut rt,
        "legal_kernel",
        unit,
        Predicate::new(own(src), Cond::True, Delivery::Each(vec![])),
        &[out],
        CalcOptions::default()
            .tier(Tier::Kernel)
            .kernel_ir(KernelIr::new(vec![KernelWrite::new(
                out,
                vec![KernelOp::Const(Value::Int(1))],
            )])),
        move |_, _| panic!("kernel backend path must not call closure fallback"),
    )
    .unwrap();

    let u = rt.spawn(unit, vec![]);
    rt.step();
    rt.debug_write(u, src, Value::Int(1));
    rt.step();
}

#[test]
fn invalid_kernel_backend_output_falls_back_to_scalar_when_not_strict() {
    let mut rt = Runtime::new();
    rt.set_detect(Detect::Warn);
    rt.register_kernel_backend(Box::new(KillBackend));
    let unit = entity(&mut rt, "Unit", vec![field("src", 0), field("out", 0)]);
    let src = rt.field(unit, "src");
    let out = rt.field(unit, "out");
    register_opt(
        &mut rt,
        "legal_kernel",
        unit,
        Predicate::new(own(src), Cond::True, Delivery::Each(vec![])),
        &[out],
        CalcOptions::default()
            .tier(Tier::Kernel)
            .kernel_ir(KernelIr::new(vec![KernelWrite::new(
                out,
                vec![KernelOp::Const(Value::Int(7))],
            )])),
        move |_, _| panic!("kernel backend path must not call closure fallback"),
    )
    .unwrap();

    let u = rt.spawn(unit, vec![]);
    rt.step();
    rt.debug_write(u, src, Value::Int(1));
    rt.step();

    assert_eq!(rt.read(u, out), Value::Int(7));
    assert_eq!(
        rt.read(u, pce::entity::FIELD_ALIVE),
        Value::Bool(true),
        "bad backend _alive output was rejected instead of killing the lane"
    );
}

/// Kernel IR 注册期校验：写集与读集都能被机检。
#[test]
fn kernel_ir_validates_declared_writes_and_reads() {
    let mut rt = Runtime::new();
    let unit = entity(&mut rt, "Unit", vec![field("src", 0), field("out", 0)]);
    let src = rt.field(unit, "src");
    let out = rt.field(unit, "out");

    let err = register_opt(
        &mut rt,
        "bad_write_ir",
        unit,
        Predicate::new(own(src), Cond::True, Delivery::Each(vec![])),
        &[],
        CalcOptions {
            kernel_ir: Some(KernelIr::new(vec![KernelWrite::new(
                out,
                vec![KernelOp::Const(Value::Int(1))],
            )])),
            ..CalcOptions::default()
        },
        move |_, _| {},
    )
    .unwrap_err();
    assert!(err.contains("未声明字段"), "{err}");

    let err = register_opt(
        &mut rt,
        "bad_read_ir",
        unit,
        Predicate::new(own(src), Cond::True, Delivery::Each(vec![])),
        &[out],
        CalcOptions {
            reads: Some(vec![src]),
            kernel_ir: Some(KernelIr::new(vec![KernelWrite::new(
                out,
                vec![KernelOp::ReadOwn(out)],
            )])),
            ..CalcOptions::default()
        },
        move |_, _| {},
    )
    .unwrap_err();
    assert!(err.contains("读了未声明字段"), "{err}");

    let err = register_opt(
        &mut rt,
        "bad_arg_ir",
        unit,
        Predicate::new(own(src), Cond::True, Delivery::Each(vec![])),
        &[out],
        CalcOptions {
            kernel_ir: Some(KernelIr::new(vec![KernelWrite::new(
                out,
                vec![KernelOp::InputArg(0)],
            )])),
            ..CalcOptions::default()
        },
        move |_, _| {},
    )
    .unwrap_err();
    assert!(err.contains("InputArg") || err.contains("arg 0"), "{err}");
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

#[test]
fn profiler_counts_boxify_events() {
    let mut rt = Runtime::new();
    let unit = entity(&mut rt, "Unit", vec![field("v", 0)]);
    let f_v = rt.field(unit, "v");
    let u = rt.spawn(unit, vec![]);
    rt.step();

    rt.debug_write(u, f_v, Value::Float(1.5));

    assert_eq!(rt.profile().boxify_events, 1);
    assert_eq!(rt.read(u, f_v), Value::Float(1.5));
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

/// 注册失败必须是事务式失败：不能污染 D1 owner 表或路由索引。
#[test]
fn failed_registration_does_not_poison_runtime() {
    let mut rt = Runtime::new();
    let unit = entity(&mut rt, "Unit", vec![field("v", 0)]);
    let f_v = rt.field(unit, "v");

    let bad_owner = register(
        &mut rt,
        "bad_owner",
        unit,
        Predicate::new(
            Scope::And(Box::new(own(f_v)), Box::new(own(f_v))),
            Cond::True,
            Delivery::Batch(vec![]),
        ),
        &[f_v],
        |_, _| {},
    );
    assert!(bad_owner.is_err());
    assert!(
        register(
            &mut rt,
            "good_after_bad_owner",
            unit,
            Predicate::new(own(f_v), Cond::True, Delivery::Each(vec![])),
            &[f_v],
            move |ctx, _| ctx.write(f_v, 1),
        )
        .is_ok(),
        "失败注册不应占住 D1 owner"
    );

    let mut rt = Runtime::new();
    let unit = entity(&mut rt, "Unit", vec![field("v", 0)]);
    let f_v = rt.field(unit, "v");
    let bad_index = register(
        &mut rt,
        "bad_index",
        unit,
        Predicate::new(
            Scope::Or(Box::new(own(f_v)), Box::new(own(FieldId(999)))),
            Cond::True,
            Delivery::Each(vec![]),
        ),
        &[],
        |_, _| {},
    );
    assert!(bad_index.is_err());

    let u = rt.spawn(unit, vec![]);
    rt.step();
    rt.debug_write(u, f_v, Value::Int(1));
    rt.step(); // 若索引被污染，这里会路由到不存在的 CalcId 并 panic。
}

#[test]
fn try_field_reports_invalid_type_id_instead_of_panicking() {
    let rt = Runtime::new();
    let err = rt.try_field(EntityTypeId(999), "missing").unwrap_err();
    assert!(err.contains("无类型 id 999"), "{err}");
}

#[test]
fn conjunction_scope_allows_63_groups_and_fires_once() {
    let mut rt = Runtime::new();
    let mut defs: Vec<FieldDef> = (0..63).map(|i| field(&format!("g{i}"), 0)).collect();
    defs.push(field("hits", 0));
    let unit = entity(&mut rt, "Unit", defs);
    let groups: Vec<FieldId> = (1..=63).map(FieldId).collect();
    let hits = rt.field(unit, "hits");

    register(
        &mut rt,
        "wide_and",
        unit,
        Predicate::new(and_scope(&groups), Cond::True, Delivery::Each(vec![])),
        &[hits],
        move |ctx, _| {
            let n = ctx.read_own(hits).as_i64().unwrap_or(0);
            ctx.write(hits, n + 1);
        },
    )
    .unwrap();

    let u = rt.spawn(unit, vec![]);
    rt.step();
    for f in &groups {
        rt.debug_write(u, *f, Value::Int(1));
    }
    rt.step();
    assert_eq!(rt.read(u, hits), Value::Int(1));

    rt.debug_write(u, groups[0], Value::Int(2));
    rt.step();
    assert_eq!(rt.read(u, hits), Value::Int(1));
}

#[test]
fn conjunction_scope_rejects_more_than_63_groups() {
    let mut rt = Runtime::new();
    let defs: Vec<FieldDef> = (0..64).map(|i| field(&format!("g{i}"), 0)).collect();
    let unit = entity(&mut rt, "Unit", defs);
    let groups: Vec<FieldId> = (1..=64).map(FieldId).collect();
    let err = register(
        &mut rt,
        "too_wide_and",
        unit,
        Predicate::new(and_scope(&groups), Cond::True, Delivery::Each(vec![])),
        &[],
        |_, _| {},
    )
    .unwrap_err();
    assert!(err.contains("合取组数") && err.contains("63"), "{err}");
}

/// OR scope 中重复原子不应让同一 write 重复交付。
#[test]
fn duplicate_or_atom_delivers_once() {
    let mut rt = Runtime::new();
    let unit = entity(&mut rt, "Unit", vec![field("v", 0)]);
    let sink = singleton(&mut rt, "Sink", vec![field("rows", 0)]);
    let f_v = rt.field(unit, "v");
    let f_rows = rt.field(sink, "rows");

    register(
        &mut rt,
        "collect",
        sink,
        Predicate::new(
            Scope::Or(
                Box::new(type_scope(unit, f_v)),
                Box::new(type_scope(unit, f_v)),
            ),
            Cond::True,
            Delivery::Batch(vec![new_proj()]),
        ),
        &[f_rows],
        move |ctx, input| ctx.write(f_rows, input.rows().len() as i64),
    )
    .unwrap();

    let u = rt.spawn(unit, vec![]);
    let s0 = rt.alive(sink)[0];
    rt.step();
    rt.debug_write(u, f_v, Value::Int(7));
    rt.step();
    assert_eq!(rt.read(s0, f_rows), Value::Int(1));
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

/// OQ2：投影侧标量四则——封闭集（new/old/own/const）算术，投影发生在命中之后、
/// 不破成本不变量。常量臂 new*2、活字段臂 new*own(factor) 与直接 New 取值臂共用
/// 同一份条件 Expr 编译器（编译器单一真源）。
#[test]
fn projection_side_arithmetic_delivers_computed_values() {
    let mut rt = Runtime::new();
    let unit = entity(
        &mut rt,
        "Unit",
        vec![
            field("src", 0),
            field("factor", 3),
            field("dbl", 0),
            field("scaled", 0),
            field("plain", 0),
        ],
    );
    let src = rt.field(unit, "src");
    let factor = rt.field(unit, "factor");
    let dbl = rt.field(unit, "dbl");
    let scaled = rt.field(unit, "scaled");
    let plain = rt.field(unit, "plain");

    register(
        &mut rt,
        "scale",
        unit,
        Predicate::new(
            own(src),
            Cond::True,
            Delivery::Each(vec![
                Proj::Expr(Expr::Mul(Box::new(new_val()), Box::new(val(2)))),
                Proj::Expr(Expr::Mul(Box::new(new_val()), Box::new(own_field(factor)))),
                new_proj(),
            ]),
        ),
        &[dbl, scaled, plain],
        move |ctx, input| {
            ctx.write(dbl, input.arg(0).clone());
            ctx.write(scaled, input.arg(1).clone());
            ctx.write(plain, input.arg(2).clone());
        },
    )
    .unwrap();

    let u = rt.spawn(unit, vec![(src, Value::Int(0)), (factor, Value::Int(3))]);
    rt.step(); // 出生写路由
    rt.debug_write(u, src, Value::Int(5));
    rt.step();

    // 算术臂产出 Float（与条件侧四则同语义）；直接取值臂保持原值类型。
    assert_eq!(rt.read(u, dbl), Value::Float(10.0), "new*2");
    assert_eq!(rt.read(u, scaled), Value::Float(15.0), "new*own(factor)");
    assert_eq!(rt.read(u, plain), Value::Int(5), "直接 New 臂不变");
}
