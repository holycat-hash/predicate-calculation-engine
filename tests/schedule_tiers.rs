//! C1/C2/C3 上游集成：C 档位标注解析为每帧执行计划（Schedule）——按 calc 分组
//! （kernel 批 / 读集局部性的结构前提）、解析驻留分区（C3，含 Auto 的 profile 建议）。

use pce::predicate::{own, type_scope};
use pce::{
    CalcOptions, Cond, Delivery, FieldDef, Predicate, Proj, Residency, Runtime, Tier, Value,
};

fn field(name: &str, default: impl Into<Value>) -> FieldDef {
    FieldDef::new(name, default.into())
}

/// type 扇出：同一 calc 的 N 次触发被归为一个连续组（count = N）。
#[test]
fn schedule_groups_triggers_by_calc() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![field("v", 0)], false);
    let watch = rt.register_entity_type("Watch", vec![field("hits", 0)], true);
    let f_v = rt.field(unit, "v");
    let f_hits = rt.field(watch, "hits");

    // own(v) each：每个 unit 自己订阅自己的 v。三个 unit 同帧写 → 三次触发同一 calc。
    rt.register_calculation(
        "on_v",
        unit,
        Predicate::new(own(f_v), Cond::True, Delivery::Each(vec![])),
        &[],
        Box::new(|_, _| {}),
    )
    .unwrap();
    let _ = (watch, f_hits);

    let a = rt.spawn(unit, vec![]);
    let b = rt.spawn(unit, vec![]);
    let c = rt.spawn(unit, vec![]);
    rt.step();

    rt.debug_write(a, f_v, Value::Int(1));
    rt.debug_write(b, f_v, Value::Int(1));
    rt.debug_write(c, f_v, Value::Int(1));
    rt.step();

    let sched = rt.last_schedule();
    assert_eq!(sched.groups.len(), 1, "三次触发归为一个 calc 组");
    assert_eq!(sched.groups[0].count, 3);
    assert_eq!(sched.groups[0].tier, Tier::General);
}

/// C3 驻留 pin 解析进计划：GPU-pin 的 calc 落 GPU 分区，默认 calc 落 CPU 分区。
/// C2 声明读集随组暴露（热列），C1 档位随组暴露。
#[test]
fn schedule_resolves_residency_partition() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type(
        "Unit",
        vec![
            field("t1", 0),
            field("o1", 0),
            field("t2", 0),
            field("o2", 0),
        ],
        false,
    );
    let (f_t1, f_o1, f_t2, f_o2) = (
        rt.field(unit, "t1"),
        rt.field(unit, "o1"),
        rt.field(unit, "t2"),
        rt.field(unit, "o2"),
    );

    // calc 0：默认（CPU），声明读集 [o1]（C2）。
    let cpu_calc = rt
        .register_calculation_opt(
            "cpu_work",
            unit,
            Predicate::new(own(f_t1), Cond::True, Delivery::Each(vec![])),
            &[f_o1],
            CalcOptions {
                reads: Some(vec![f_o1]),
                ..CalcOptions::default()
            },
            Box::new(move |ctx, _| {
                let n = ctx.read_own(f_o1).as_i64().unwrap();
                ctx.write(f_o1, n + 1);
            }),
        )
        .unwrap();

    // calc 1：Kernel 档（C1）+ GPU pin（C3）。
    let gpu_calc = rt
        .register_calculation_opt(
            "gpu_work",
            unit,
            Predicate::new(own(f_t2), Cond::True, Delivery::Each(vec![])),
            &[f_o2],
            CalcOptions {
                tier: Tier::Kernel,
                residency: Residency::Gpu,
                ..CalcOptions::default()
            },
            Box::new(move |ctx, _| ctx.write(f_o2, ctx.read_own(f_o2).as_i64().unwrap() + 1)),
        )
        .unwrap();

    let u = rt.spawn(unit, vec![]);
    rt.step();
    rt.debug_write(u, f_t1, Value::Int(1));
    rt.debug_write(u, f_t2, Value::Int(1));
    rt.step();

    let sched = rt.last_schedule();
    assert_eq!(sched.groups.len(), 2);

    let (cpu, gpu) = sched.residency_partition();
    assert_eq!(gpu, vec![gpu_calc], "GPU-pin 的 calc 落 GPU 分区");
    assert_eq!(cpu, vec![cpu_calc], "默认 calc 落 CPU 分区");

    // C1/C2 标注随组可观测。
    let cpu_group = sched.groups.iter().find(|g| g.calc == cpu_calc).unwrap();
    assert_eq!(cpu_group.tier, Tier::General);
    assert_eq!(cpu_group.reads.as_deref(), Some([f_o1].as_slice()));
    let gpu_group = sched.groups.iter().find(|g| g.calc == gpu_calc).unwrap();
    assert_eq!(gpu_group.tier, Tier::Kernel);
    assert_eq!(gpu_group.residency, Residency::Gpu);
}

/// 执行计划的分组重排不改语义：跨 calc 写集互斥（D1），任意执行序结果等价。
#[test]
fn schedule_grouping_preserves_semantics() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![field("src", 0)], false);
    let sink = rt.register_entity_type("Sink", vec![field("a", 0), field("b", 0)], true);
    let f_src = rt.field(unit, "src");
    let (f_a, f_b) = (rt.field(sink, "a"), rt.field(sink, "b"));

    // 两个不同 calc 写不同字段（D1 互斥），都订阅同一 src 流。
    rt.register_calculation(
        "to_a",
        sink,
        Predicate::new(
            type_scope(unit, f_src),
            Cond::True,
            Delivery::Each(vec![Proj::New(vec![])]),
        ),
        &[f_a],
        Box::new(move |ctx, input| ctx.write(f_a, input.arg(0).clone())),
    )
    .unwrap();
    rt.register_calculation(
        "to_b",
        sink,
        Predicate::new(
            type_scope(unit, f_src),
            Cond::True,
            Delivery::Each(vec![Proj::New(vec![])]),
        ),
        &[f_b],
        Box::new(move |ctx, input| {
            let v = input.arg(0).as_i64().unwrap();
            ctx.write(f_b, v * 2);
        }),
    )
    .unwrap();

    let u = rt.spawn(unit, vec![]);
    let s0 = rt.alive(sink)[0];
    rt.step();
    rt.debug_write(u, f_src, Value::Int(21));
    rt.step();

    assert_eq!(rt.read(s0, f_a), Value::Int(21));
    assert_eq!(rt.read(s0, f_b), Value::Int(42));
    // 两个 calc 各成一组。
    assert_eq!(rt.last_schedule().groups.len(), 2);
}
