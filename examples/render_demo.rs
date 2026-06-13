//! 演示：第二个 runtime（动态帧率 render 侧）。
//!
//! 固定步长 sim 每帧把 Unit 推进 10 像素（ECS mover）；动态帧率 render 在两个 sim
//! 帧之间按 alpha 线性插值，画出亚帧的平滑位置；hp 跌穿 0 时 render 事件反应起死亡
//! 特效。全程：render 只读 sim（经 SimFrame 冻结快照）、只写 render 字段，sim 永不
//! 读 render——并发解耦的结构强制。
//!
//! 运行：cargo run --example render_demo

use pce::predicate::type_scope;
use pce::{
    Cond, Delivery, Dir, Expr, FieldDef, Interp, Predicate, Proj, Publisher, RenderRuntime,
    Runtime, ValRef, Value,
};

fn main() {
    // ---- sim 侧（固定步长）----
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type(
        "Unit",
        vec![
            FieldDef::new("pos", Value::Int(0)),
            FieldDef::new("vel", Value::Int(10)),
            FieldDef::new("hp", Value::Int(30)),
        ],
        false,
    );
    let (f_pos, f_vel, f_hp) = (rt.field(unit, "pos"), rt.field(unit, "vel"), rt.field(unit, "hp"));
    let (cty, cframe) = {
        let c = rt.clock();
        (c.ty, c.f_frame)
    };
    // ECS mover：每帧 pos += vel。
    rt.register_calculation(
        "mover",
        unit,
        Predicate::new(type_scope(cty, cframe), Cond::True, Delivery::Each(vec![])),
        &[f_pos],
        Box::new(move |ctx, _| {
            let pos = ctx.read_own(f_pos).as_i64().unwrap_or(0);
            let vel = ctx.read_own(f_vel).as_i64().unwrap_or(0);
            ctx.write(f_pos, pos + vel);
        }),
    )
    .unwrap();
    rt.enable_render_feed();

    // ---- render 侧（动态帧率）----
    let mut rr = RenderRuntime::new(&rt);
    // track：pos 镜像进 render，Lerp 维护插值输出字段（fold 的 render 对偶）。
    let r_pos = rr.track(unit, f_pos, Interp::Lerp).unwrap();
    // 纯 render 字段：死亡特效标记。
    let r_fx = rr.add_render_field(unit, Value::Int(0));
    // 事件反应：hp 跌穿 1（向下）→ 起死亡特效。复用谓词代数路由 sim 写日志。
    rr.reaction(
        "death_fx",
        unit,
        f_hp,
        Cond::Crossed(Expr::Val(ValRef::Const(Value::Int(1))), Dir::Down),
        vec![Proj::Old(vec![])],
        false,
        &[r_fx],
        Box::new(move |ctx, input| {
            println!("    [render 反应] hp {:?}→0，起死亡特效", input.arg(0));
            ctx.write(r_fx, 1);
        }),
    )
    .unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let u = rt.spawn(unit, vec![(f_pos, Value::Int(0))]);

    // ---- 帧循环：每个 sim 帧后，render 在该区间内画 3 个动态帧（alpha 0/0.33/0.66）----
    for sim_frame in 1..=4u64 {
        rt.step();
        publisher.publish(&rt, sim_frame);
        for sf in publisher.drain() {
            rr.ingest(&sf);
        }
        println!("sim 帧 {sim_frame}：sim.pos = {:?}（render 落后一帧做插值）", rt.read(u, f_pos));
        for k in 0..3 {
            let alpha = k as f64 / 3.0;
            rr.render_frame(0.016, alpha);
            println!(
                "    render α={alpha:.2}  插值 pos = {:?}  活动集 = {}",
                rr.read(u, r_pos),
                rr.active_count(),
            );
        }
    }

    // ---- 触发死亡反应：外部把 hp 砍到 0 ----
    println!("\n-- 外部把 hp 砍到 0 --");
    rt.debug_write(u, f_hp, Value::Int(0));
    rt.step();
    publisher.publish(&rt, 5);
    for sf in publisher.drain() {
        rr.ingest(&sf);
    }
    rr.render_frame(0.016, 1.0);
    println!("death_fx = {:?}（1 = 已起特效）", rr.read(u, r_fx));
}
