//! 演示：render 的「良好渲染语义数据」全流程 → GPU 提交 staging 包。
//!
//! 固定步长 sim 每帧推进 Unit 的位置（+10 像素）与朝向（+30°）；动态帧率 render：
//! - `track` 把 pos（Vec3Lerp）/ rot（Slerp）/ mesh·mat（Snap）镜像并按 alpha 插值；
//! - 一个 `continuous` 动画控制器读 sim `action` 的镜像，切动画态、按 dt 推进进度；
//! - `set_death_fade` 让死亡实体在 render 侧淡出（延迟回收）而非瞬间消失；
//! - `submit()` 每 render 帧装配出有序的逐实体提交包（transform / handle / 动画态 /
//!   淡出权重），后端据此打包顶点 / 实例缓冲（光追 / 3D 纹理是后端的未来事）。
//!
//! 全程：render 只读 sim（经 SimFrame 冻结快照）、只写 render 字段，sim 永不读 render。
//!
//! 运行：cargo run --example render_staging

use pce::predicate::type_scope;
use pce::{
    Cond, Delivery, FieldDef, Interp, Predicate, Publisher, RenderBinding, RenderRuntime, Runtime,
    Value,
};

fn main() {
    // ---- sim 侧（固定步长）----
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type(
        "Unit",
        vec![
            FieldDef::new("pos", Value::vec3(0.0, 0.0, 0.0)),
            FieldDef::new("rot", Value::quat_identity()),
            FieldDef::new("angle", Value::Float(0.0)),
            FieldDef::new("mesh", Value::Int(101)),
            FieldDef::new("mat", Value::Int(7)),
            FieldDef::new("action", Value::Int(0)),
        ],
        false,
    );
    let f_pos = rt.field(unit, "pos");
    let f_rot = rt.field(unit, "rot");
    let f_angle = rt.field(unit, "angle");
    let f_action = rt.field(unit, "action");
    let (cty, cframe) = {
        let c = rt.clock();
        (c.ty, c.f_frame)
    };
    // ECS：每帧推进 transform（pos +10，绕 Z +30°）。
    rt.register_calculation(
        "advance",
        unit,
        Predicate::new(type_scope(cty, cframe), Cond::True, Delivery::Each(vec![])),
        &[f_pos, f_rot, f_angle],
        Box::new(move |ctx, _| {
            let p = ctx.read_own(f_pos).as_vec3().unwrap_or([0.0; 3]);
            ctx.write(f_pos, Value::vec3(p[0] + 10.0, p[1], p[2]));
            let a = ctx.read_own(f_angle).as_f64().unwrap_or(0.0) + 30f64.to_radians();
            ctx.write(f_angle, a);
            ctx.write(
                f_rot,
                Value::quat(0.0, 0.0, (a / 2.0).sin(), (a / 2.0).cos()),
            );
        }),
    )
    .unwrap();
    rt.enable_render_feed();

    // ---- render 侧（动态帧率）----
    let mut rr = RenderRuntime::new(&rt);
    let r_pos = rr.track(unit, f_pos, Interp::Vec3Lerp).unwrap();
    let r_rot = rr.track(unit, f_rot, Interp::Slerp).unwrap();
    let r_mesh = rr
        .track(unit, rt.field(unit, "mesh"), Interp::Snap)
        .unwrap();
    let r_mat = rr.track(unit, rt.field(unit, "mat"), Interp::Snap).unwrap();

    // 动画控制器（单写者 owns state+phase）：镜像 sim action，变则切态归零、否则按 dt 推进。
    let r_action = rr.track(unit, f_action, Interp::Snap).unwrap();
    let r_state = rr.add_render_field(unit, Value::Int(0));
    let r_phase = rr.add_render_field(unit, Value::Float(0.0));
    rr.continuous(
        "anim",
        unit,
        &[r_state, r_phase],
        Box::new(move |ctx| {
            let (mirror, state) = (ctx.read(r_action), ctx.read(r_state));
            if mirror != state {
                ctx.write(r_state, mirror);
                ctx.write(r_phase, 0.0);
            } else {
                let ph = ctx.read(r_phase).as_f64().unwrap_or(0.0);
                ctx.write(r_phase, ph + ctx.dt());
            }
        }),
    )
    .unwrap();

    // 死亡淡出：0.5 秒（按真实 dt，render 接管寿命）。
    let r_fade = rr.add_render_field(unit, Value::Float(1.0));
    rr.set_death_fade(unit, r_fade, 0.5).unwrap();

    rr.renderable(
        unit,
        RenderBinding {
            translation: Some(r_pos),
            rotation: Some(r_rot),
            mesh: Some(r_mesh),
            material: Some(r_mat),
            anim_state: Some(r_state),
            anim_phase: Some(r_phase),
            fade: Some(r_fade),
            ..Default::default()
        },
    )
    .unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let u = rt.spawn(unit, vec![]);

    // ---- 帧循环：每 sim 帧后，render 画 3 个动态子帧（alpha 0/0.33/0.66）----
    println!("== transform 插值（每 sim 区间 3 个 render 子帧）==");
    for sim_frame in 1..=3u64 {
        if sim_frame == 2 {
            rt.debug_write(u, f_action, Value::Int(4)); // 第 2 帧切动作 → 动画态切换
        }
        rt.step();
        publisher.publish(&rt, sim_frame);
        for sf in publisher.drain() {
            rr.ingest(&sf);
        }
        for k in 0..3 {
            let alpha = k as f64 / 3.0;
            rr.render_frame(0.016, alpha);
            print_submission(&rr, sim_frame, alpha);
        }
    }

    // ---- 死亡淡出：杀死 u，render 在 0.5 秒内淡出（dt=0.125 → 4 帧后回收）----
    println!("\n== 死亡淡出（render 接管寿命，延迟回收）==");
    rt.destroy(u);
    rt.step();
    publisher.publish(&rt, 4);
    for sf in publisher.drain() {
        rr.ingest(&sf);
    }
    for frame in 1..=5 {
        rr.render_frame(0.125, 1.0);
        let view = rr.submit();
        match view.packets.first() {
            Some(p) => println!(
                "  淡出帧 {frame}: fade={:.2}  仍在提交（dying={}）",
                p.fade,
                rr.dying_count()
            ),
            None => println!(
                "  淡出帧 {frame}: 已淡尽回收，提交为空（alive={}）",
                rr.alive(u)
            ),
        }
    }
}

fn print_submission(rr: &RenderRuntime, sim_frame: u64, alpha: f64) {
    let view = rr.submit();
    for p in view.iter() {
        let t = p.translation.as_vec3().unwrap_or([0.0; 3]);
        let q = p.rotation.as_quat().unwrap_or([0.0, 0.0, 0.0, 1.0]);
        let deg = 2.0 * q[2].asin().to_degrees(); // 绕 Z 角（演示用）
        println!(
            "  sim{sim_frame} α={alpha:.2}  pos=({:.1},{:.1})  rotZ={deg:>5.1}°  \
             mesh={:?} mat={:?}  anim(state={:?},phase={:.3})",
            t[0], t[1], p.mesh, p.material, p.anim_state, p.anim_phase
        );
    }
}
