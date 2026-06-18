//! 演示：render-local 临时实体通道（粒子 / 飘字）。
//!
//! 共享实体的生杀只在 sim（§9「Shared lifecycle is owned by sim」）。但粒子、飘字、
//! 贴花这类**纯视觉**住户不该挤进 sim schema / 写日志 / 谓词——它们理想上是 render
//! 私有的池化实体，由 render runtime 自管生杀。本例展示这条通道的两种典型用法：
//!
//! 1. **render 自管寿命的粒子爆发**：`spawn_local` 起一簇粒子，`local_continuous` 每
//!    render 帧按真实 dt 积分运动 + 衰减 ttl + 推淡出，ttl 用尽 `destroy_self()`。释放的
//!    本地 id 进池复用（代际 +1，旧句柄不会误指新住户）。全程不碰 sim。
//! 2. **sim 事件喷飘字**：共享 reaction 在 Unit 掉血时 `ctx.spawn_local` 一条飘字 local
//!    实体——sim 侧零改动（不为飘字建 sim 类型），render 自己持有这些临时实体的生命周期。
//!
//! 运行：cargo run --example render_local

use pce::{
    Cond, FieldDef, Proj, Publisher, RFieldId, RenderBinding, RenderLocalFieldDef, RenderRuntime,
    Runtime, Value,
};

/// 淡出窗口（秒）：粒子剩余寿命在此窗口内由 1 线性推到 0。
const FADE_WINDOW: f64 = 0.5;

#[derive(Clone, Copy)]
struct Particle {
    pos: RFieldId,
    vel: RFieldId,
    ttl: RFieldId,
    fade: RFieldId,
    mesh: RFieldId,
}

fn main() {
    particle_burst();
    println!();
    floating_combat_text();
}

/// 用法 1：render 私有池化粒子，render 自管生杀。
fn particle_burst() {
    println!("== render-local 粒子爆发（render 自管寿命 + 池化复用）==");

    let rt = Runtime::new();
    let mut rr = RenderRuntime::new(&rt);

    // render-local 类型：字段全在 render 命名空间（RFieldId），不入 sim schema。
    let particle = rr.register_local_type(
        "Particle",
        vec![
            RenderLocalFieldDef::new("pos", Value::vec3(0.0, 0.0, 0.0)),
            RenderLocalFieldDef::new("vel", Value::vec3(0.0, 0.0, 0.0)),
            RenderLocalFieldDef::new("ttl", Value::Float(0.0)),
            RenderLocalFieldDef::new("fade", Value::Float(1.0)),
            RenderLocalFieldDef::new("mesh", Value::Int(0)),
        ],
    );
    let f = Particle {
        pos: rr.local_field(particle, "pos").unwrap(),
        vel: rr.local_field(particle, "vel").unwrap(),
        ttl: rr.local_field(particle, "ttl").unwrap(),
        fade: rr.local_field(particle, "fade").unwrap(),
        mesh: rr.local_field(particle, "mesh").unwrap(),
    };

    // local_continuous：render clock 每帧对每个存活粒子跑一次。可写自己的 local 字段、
    // 可 destroy_self()。按真实 dt 积分 ⇒ 行为与帧率无关（动态帧率下寿命稳定）。
    rr.local_continuous(
        "particle_tick",
        particle,
        &[f.pos, f.vel, f.ttl, f.fade],
        Box::new(move |ctx| {
            let dt = ctx.dt();
            let p = ctx.read(f.pos).as_vec3().unwrap_or([0.0; 3]);
            let v = ctx.read(f.vel).as_vec3().unwrap_or([0.0; 3]);
            // 重力下坠。
            let v = [v[0], v[1] - 9.8 * dt, v[2]];
            let ttl = ctx.read(f.ttl).as_f64().unwrap_or(0.0) - dt;
            ctx.write(
                f.pos,
                Value::vec3(p[0] + v[0] * dt, p[1] + v[1] * dt, p[2] + v[2] * dt),
            );
            ctx.write(f.vel, Value::vec3(v[0], v[1], v[2]));
            ctx.write(f.ttl, ttl);
            ctx.write(f.fade, (ttl / FADE_WINDOW).clamp(0.0, 1.0));
            if ttl <= 0.0 {
                ctx.destroy_self(); // render 自管生杀：寿命终结，本批次末提交销毁。
            }
        }),
    )
    .unwrap();

    // 提交绑定：哪些 local 字段填渲染槽（平移 / mesh handle / 淡出权重）。submit_local
    // 据此装配独立的 LocalSubmissionView，不伪造 sim InstanceId。
    rr.local_renderable(
        particle,
        RenderBinding {
            translation: Some(f.pos),
            mesh: Some(f.mesh),
            fade: Some(f.fade),
            ..Default::default()
        },
    )
    .unwrap();

    // 爆发：起 5 个粒子，向上 + 四散，各带不同寿命。
    let mut handles = vec![];
    for i in 0..5 {
        let angle = i as f64;
        let id = rr
            .spawn_local(
                particle,
                vec![
                    (f.pos, Value::vec3(0.0, 0.0, 0.0)),
                    (f.vel, Value::vec3(angle.cos() * 3.0, 6.0, angle.sin() * 3.0)),
                    (f.ttl, Value::Float(0.20 + 0.10 * i as f64)),
                    (f.mesh, Value::Int(7)),
                ],
            )
            .unwrap();
        handles.push(id);
    }
    println!("起爆 {} 个粒子", rr.local_count(particle));

    // 推进 render 帧（固定 50ms ≈ 20fps）直到爆发自然耗尽。每帧后看存活数 + 本帧提交
    // 包数衰减——粒子按各自 ttl 错峰自毁，render 无需任何外部回收逻辑。
    let first = handles[0];
    let mut frame = 0;
    while rr.local_count(particle) > 0 && frame < 30 {
        frame += 1;
        rr.render_frame(0.05, 1.0);
        let view = rr.submit_local();
        println!(
            "帧 {frame:>2}: 存活 {:>2}  提交 {:>2}  (帧首粒子 fade={:.2})",
            rr.local_count(particle),
            view.len(),
            rr.read_local(first, f.fade).as_f64().unwrap_or(0.0),
        );
    }
    assert_eq!(rr.local_count(particle), 0, "所有粒子按 ttl 自毁");
    assert!(rr.submit_local().is_empty());
    assert!(!rr.is_local_present(first), "旧句柄读到已回收");

    // 池化复用：再起一个粒子，它落在某个已释放的 slot（池不增长），但代际 +1 ⇒ 指向同
    // slot 的旧句柄永久失效，不会误指新住户（ABA 防护）。
    let reused = rr
        .spawn_local(particle, vec![(f.ttl, Value::Float(1.0))])
        .unwrap();
    let prior_tenant = handles[reused.id as usize]; // 复用之前住在该 slot 的旧句柄
    println!(
        "复用：新粒子落在 slot id={}（池未增长，仍 < {}）；同 slot 的旧句柄已失效：{}",
        reused.id,
        handles.len(),
        !rr.is_local_present(prior_tenant),
    );
    assert!(reused.id < handles.len() as u32, "复用已释放的 slot，未增长到新 id");
    assert_ne!(reused, prior_tenant, "generation 不同，旧句柄不会误指新住户");
    assert!(!rr.is_local_present(prior_tenant));
    assert!(rr.is_local_present(reused));
}

/// 用法 2：sim 事件 → render 飘字。sim 侧零改动，render 自己持有飘字临时实体。
fn floating_combat_text() {
    println!("== sim 掉血事件喷 render-local 飘字 ==");

    // sim：一个 Unit，只有 hp。它不知道「飘字」的存在。
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("hp", Value::Int(30))], false);
    let f_hp = rt.field(unit, "hp");
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let text = rr.register_local_type(
        "FloatingText",
        vec![
            RenderLocalFieldDef::new("pos", Value::vec3(0.0, 0.0, 0.0)),
            RenderLocalFieldDef::new("text", Value::str("")),
            RenderLocalFieldDef::new("ttl", Value::Float(0.75)),
            RenderLocalFieldDef::new("mesh", Value::Int(0)),
        ],
    );
    let r_pos = rr.local_field(text, "pos").unwrap();
    let r_text = rr.local_field(text, "text").unwrap();
    let r_ttl = rr.local_field(text, "ttl").unwrap();
    let r_mesh = rr.local_field(text, "mesh").unwrap();
    rr.local_renderable(
        text,
        RenderBinding {
            translation: Some(r_pos),
            mesh: Some(r_mesh),
            ..Default::default()
        },
    )
    .unwrap();

    // 共享 reaction：Unit.hp 变化时，按 (old - new) 伤害喷一条飘字 local 实体。
    // 投影只取 new/old（render 反应 v1 子集），spawn_local 排队、本帧末提交到本地池。
    rr.reaction(
        "damage_text",
        unit,
        f_hp,
        Cond::Changed,
        vec![Proj::New(vec![]), Proj::Old(vec![])],
        false,
        &[],
        Box::new(move |ctx, input| {
            let new = input.arg(0).as_i64().unwrap_or(0);
            let old = input.arg(1).as_i64().unwrap_or(new);
            let damage = old - new;
            if damage <= 0 {
                return; // 治疗 / 无变化不喷伤害字。
            }
            ctx.spawn_local(
                text,
                vec![
                    (r_pos, Value::vec3(3.0, 4.0, 0.0)),
                    (r_text, Value::from(format!("-{damage}"))),
                    (r_ttl, Value::Float(0.75)),
                    (r_mesh, Value::Int(99)),
                ],
            );
        }),
    )
    .unwrap();

    let publisher = Publisher::new(rr.tracked_fields());

    // 出生 hp=30，不算掉血 → 不喷飘字。
    let u = rt.spawn(unit, vec![(f_hp, Value::Int(30))]);
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 1.0);
    println!("出生：飘字数 {}", rr.submit_local().len());
    assert!(rr.submit_local().is_empty());

    // 连挨两刀：30→20→12，每刀喷一条飘字。
    for hp in [20, 12] {
        rt.debug_write(u, f_hp, Value::Int(hp));
        rt.step();
        publisher.publish(&rt);
        rr.sync(&publisher, 0.016, 1.0);
    }
    let view = rr.submit_local();
    println!("挨两刀后：飘字数 {}", view.len());
    for packet in view.iter() {
        println!(
            "  飘字 \"{}\" @ {:?}  mesh={:?}",
            rr.read_local(packet.local, r_text).as_str().unwrap_or(""),
            packet.translation.as_vec3().unwrap_or([0.0; 3]),
            packet.mesh,
        );
    }
    assert_eq!(view.len(), 2, "两刀各喷一条飘字");
}
