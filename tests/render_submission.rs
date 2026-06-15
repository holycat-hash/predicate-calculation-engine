//! render runtime 渲染语义数据测试：结构化插值（Vec3Lerp / Slerp）、GPU 提交视图
//! 装配（transform / handle / 可见性剔除）、render 自管死亡淡出（延迟回收 + 重生夺
//! 回行）、动画状态切换 + 进度推进（三类原语组合，无第五概念）。
//!
//! 覆盖「让 render 给出良好的渲染语义数据」这条主线：sim 只动语义源，render 把它
//! 插值 / 装配 / 淡出 / 控动画，submit 产出后端可直接打包的 staging 包。

use pce::predicate::type_scope;
use pce::{
    Cond, Delivery, FieldDef, Interp, Predicate, Proj, Publisher, RFieldId, RenderBinding,
    RenderRuntime, Runtime, SimFrame, Value,
};

/// drain 全部未消费帧、顺序摄入，不推进 render 帧。
fn pump(rr: &mut RenderRuntime, publisher: &Publisher) {
    for sf in publisher.drain() {
        rr.ingest(&sf);
    }
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

fn approx_arr(a: &[f64], b: &[f64]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| approx(*x, *y))
}

// ---- 结构化插值原语（纯 sample，精确语义）----

#[test]
fn vec3lerp_interpolates_each_component() {
    let p = Value::vec3(0.0, 10.0, -4.0);
    let c = Value::vec3(10.0, 30.0, 4.0);
    assert_eq!(Interp::Vec3Lerp.sample(&p, &c, 0.0), p, "alpha=0 → prev");
    assert_eq!(Interp::Vec3Lerp.sample(&p, &c, 1.0), c, "alpha=1 → cur");
    assert_eq!(
        Interp::Vec3Lerp.sample(&p, &c, 0.5),
        Value::vec3(5.0, 20.0, 0.0),
        "半程逐分量线性"
    );
}

#[test]
fn slerp_endpoints_halfway_and_shortest_arc() {
    let s = 2f64.sqrt() / 2.0; // sin/cos 45°
    let id = Value::quat_identity(); // 0°
    let z90 = Value::quat(0.0, 0.0, s, s); // 绕 Z 90°
    // 端点。
    assert!(approx_arr(
        &Interp::Slerp.sample(&id, &z90, 0.0).as_quat().unwrap(),
        &[0.0, 0.0, 0.0, 1.0]
    ));
    assert!(approx_arr(
        &Interp::Slerp.sample(&id, &z90, 1.0).as_quat().unwrap(),
        &[0.0, 0.0, s, s]
    ));
    // 半程 = 绕 Z 45°：[0,0,sin22.5,cos22.5]。
    let half = Interp::Slerp.sample(&id, &z90, 0.5).as_quat().unwrap();
    let (s225, c225) = ((22.5f64).to_radians().sin(), (22.5f64).to_radians().cos());
    assert!(
        approx_arr(&half, &[0.0, 0.0, s225, c225]),
        "slerp 半程匀速到 45°：{half:?}"
    );
    // 最短弧：cur = −id（dot=−1，长弧 360°）应取短弧 0°，处处 ≈ 单位旋转。
    let neg = Value::quat(0.0, 0.0, 0.0, -1.0);
    let mid = Interp::Slerp.sample(&id, &neg, 0.5).as_quat().unwrap();
    assert!(
        mid[3].abs() > 0.999 && approx(mid[0], 0.0) && approx(mid[1], 0.0) && approx(mid[2], 0.0),
        "最短弧：−id 与 id 同旋转，插值留在单位旋转：{mid:?}"
    );
}

#[test]
fn lerp_misapplied_to_vec3_degrades_to_snap() {
    // 量纲错配（标量 Lerp 用在 Vec3）退化为 Snap（取 cur），不报错——视觉量容错。
    let p = Value::vec3(0.0, 0.0, 0.0);
    let c = Value::vec3(9.0, 9.0, 9.0);
    assert_eq!(Interp::Lerp.sample(&p, &c, 0.5), c, "Lerp 不识 Vec3 → Snap");
}

#[test]
fn slerp_normalizes_non_unit_quats_and_alpha_nan_is_safe() {
    let s = 2f64.sqrt() / 2.0;
    let non_unit_id = Value::quat(0.0, 0.0, 0.0, 2.0);
    let z90 = Value::quat(0.0, 0.0, s, s);
    let half = Interp::Slerp
        .sample(&non_unit_id, &z90, 0.5)
        .as_quat()
        .unwrap();
    let (s225, c225) = ((22.5f64).to_radians().sin(), (22.5f64).to_radians().cos());
    assert!(
        approx_arr(&half, &[0.0, 0.0, s225, c225]),
        "非单位输入先归一化：{half:?}"
    );

    let p = Value::vec3(1.0, 2.0, 3.0);
    let c = Value::vec3(4.0, 5.0, 6.0);
    assert_eq!(
        Interp::Vec3Lerp.sample(&p, &c, f64::NAN),
        p,
        "NaN alpha 归零"
    );

    let bad = Value::quat(f64::NAN, 0.0, 0.0, 1.0);
    let out = Interp::Slerp.sample(&bad, &z90, 0.5).as_quat().unwrap();
    assert!(
        out.iter().all(|v| v.is_finite()),
        "病态输入不外泄 NaN：{out:?}"
    );
}

#[test]
fn out_default_types_track_output_column_by_interp_kind() {
    // track() 用 out_default 给输出 render 字段定型：种类决定输出类型（与 sample 的产出
    // 一致），列即按该类型无装箱定型，而非恒 Null→Boxed 丢掉 render 最热 transform 插值
    // 输出的去装箱收益。Snap/Step 透传源类型；Lerp/Vec3Lerp/Slerp 各自定型（与源无关）。
    assert_eq!(Interp::Snap.out_default(&Value::Int(7)), Value::Int(7));
    assert_eq!(
        Interp::Step.out_default(&Value::Bool(true)),
        Value::Bool(true)
    );
    assert_eq!(Interp::Lerp.out_default(&Value::Int(3)), Value::Float(3.0));
    assert_eq!(
        Interp::Vec3Lerp.out_default(&Value::Null),
        Value::vec3(0.0, 0.0, 0.0),
        "源非 Vec3（量纲错配）退向量零元，仍是 Vec3 而非 Null"
    );
    assert_eq!(
        Interp::Slerp.out_default(&Value::Null),
        Value::quat_identity(),
        "源非 Quat 退单位四元数，仍是 Quat 而非 Null"
    );
    assert_eq!(
        Interp::Vec3Lerp.out_default(&Value::vec3(1.0, 2.0, 3.0)),
        Value::vec3(1.0, 2.0, 3.0),
        "源已是目标型则原样保留"
    );
}

#[test]
fn vec3_track_with_null_default_births_to_typed_fallback() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("pos", Value::Null)], false);
    let f_pos = rt.field(unit, "pos");
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let r_pos = rr.track(unit, f_pos, Interp::Vec3Lerp).unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let u = rt.spawn(unit, vec![]);
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 1.0);

    assert_eq!(
        rr.read(u, r_pos),
        Value::vec3(0.0, 0.0, 0.0),
        "量纲未知的 Vec3Lerp 出生输出应保持 Vec3 定型默认值，而非写 Null 打回 Boxed"
    );
}

// ---- track → submission 端到端 ----

/// Unit{pos:Vec3, rot:Quat, mesh:Int, mat:Int}，挂 pos += (10,0,0) 的 ECS mover。
fn sim_with_transform() -> (Runtime, pce::EntityTypeId, pce::FieldId) {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type(
        "Unit",
        vec![
            FieldDef::new("pos", Value::vec3(0.0, 0.0, 0.0)),
            FieldDef::new("rot", Value::quat_identity()),
            FieldDef::new("mesh", Value::Int(0)),
            FieldDef::new("mat", Value::Int(0)),
        ],
        false,
    );
    let f_pos = rt.field(unit, "pos");
    let (cty, cframe) = {
        let c = rt.clock();
        (c.ty, c.f_frame)
    };
    rt.register_calculation(
        "mover",
        unit,
        Predicate::new(type_scope(cty, cframe), Cond::True, Delivery::Each(vec![])),
        &[f_pos],
        Box::new(move |ctx, _| {
            let p = ctx.read_own(f_pos).as_vec3().unwrap_or([0.0; 3]);
            ctx.write(f_pos, Value::vec3(p[0] + 10.0, p[1], p[2]));
        }),
    )
    .unwrap();
    rt.enable_render_feed();
    (rt, unit, f_pos)
}

#[test]
fn transform_track_flows_into_submission_packet() {
    let (mut rt, unit, f_pos) = sim_with_transform();
    let (f_rot, f_mesh, f_mat) = (
        rt.field(unit, "rot"),
        rt.field(unit, "mesh"),
        rt.field(unit, "mat"),
    );

    let mut rr = RenderRuntime::new(&rt);
    let r_pos = rr.track(unit, f_pos, Interp::Vec3Lerp).unwrap();
    let r_rot = rr.track(unit, f_rot, Interp::Slerp).unwrap();
    let r_mesh = rr.track(unit, f_mesh, Interp::Snap).unwrap();
    let r_mat = rr.track(unit, f_mat, Interp::Snap).unwrap();
    rr.renderable(
        unit,
        RenderBinding {
            translation: Some(r_pos),
            rotation: Some(r_rot),
            mesh: Some(r_mesh),
            material: Some(r_mat),
            ..Default::default()
        },
    )
    .unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let u = rt.spawn(unit, vec![(f_mesh, Value::Int(7)), (f_mat, Value::Int(3))]);
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 1.0); // 出生帧 snap

    rt.step();
    publisher.publish(&rt);
    pump(&mut rr, &publisher);
    rr.render_frame(0.016, 0.5); // pos 区间 [0,0,0]→[10,0,0]，半程

    let view = rr.submit();
    assert_eq!(view.len(), 1, "一个可见实体一个提交包");
    let pkt = &view.packets[0];
    assert_eq!(pkt.inst, u);
    assert_eq!(pkt.translation, Value::vec3(5.0, 0.0, 0.0), "平移插值半程");
    assert_eq!(
        pkt.rotation,
        Value::quat_identity(),
        "无旋转写入 → 静止于单位四元数"
    );
    assert_eq!(pkt.mesh, Value::Int(7), "mesh handle 装配");
    assert_eq!(pkt.material, Value::Int(3), "material handle 装配");
    assert!(approx(pkt.fade, 1.0), "存活实体实心 fade=1");
}

#[test]
fn same_frame_tracked_writes_fold_to_first_old_and_last_new() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("pos", Value::Int(0))], false);
    let f_pos = rt.field(unit, "pos");
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let r_pos = rr.track(unit, f_pos, Interp::Lerp).unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let u = rt.spawn(unit, vec![(f_pos, Value::Int(0))]);
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 1.0);

    rt.debug_write(u, f_pos, Value::Int(10));
    rt.debug_write(u, f_pos, Value::Int(20));
    rt.step();
    publisher.publish(&rt);
    pump(&mut rr, &publisher);
    assert_eq!(
        rr.active_count(),
        1,
        "同一 tracked cell 同帧多写只产生一个 active 项"
    );

    rr.render_frame(0.016, 0.0);
    assert_eq!(
        rr.read(u, r_pos),
        Value::Float(0.0),
        "alpha=0 取上一帧提交值"
    );
    rr.render_frame(0.016, 0.5);
    assert_eq!(
        rr.read(u, r_pos),
        Value::Float(10.0),
        "alpha=0.5 走 0→20 的半程"
    );
    rr.render_frame(0.016, 1.0);
    assert_eq!(
        rr.read(u, r_pos),
        Value::Float(20.0),
        "alpha=1 取当前帧最终值"
    );
}

#[test]
fn renderable_and_death_fade_reject_unknown_or_aliased_fields() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("hp", Value::Int(1))], false);
    rt.enable_render_feed();
    let mut rr = RenderRuntime::new(&rt);
    let bad = RFieldId(999);

    assert!(
        rr.renderable(
            unit,
            RenderBinding {
                translation: Some(bad),
                ..Default::default()
            }
        )
        .is_err()
    );
    assert!(rr.set_death_fade(unit, bad, 1.0).is_err());

    let r = rr.add_render_field(unit, Value::Int(0));
    let dup = RenderBinding {
        mesh: Some(r),
        material: Some(r),
        ..Default::default()
    };
    assert!(
        rr.renderable(unit, dup).is_err(),
        "一个字段不能扮演两个提交槽位"
    );
}

// ---- 可见性剔除 ----

#[test]
fn submission_culls_invisible_entities() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("hp", Value::Int(30))], false);
    let f_hp = rt.field(unit, "hp");
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    // 可见性 render 字段（默认可见）；hp 跌到 0 的反应把它写 false（隐藏）。
    let r_vis = rr.add_render_field(unit, Value::Bool(true));
    rr.reaction(
        "hide_on_death",
        unit,
        f_hp,
        Cond::Became(Value::Int(0)),
        vec![Proj::New(vec![])],
        false,
        &[r_vis],
        Box::new(move |ctx, _| ctx.write(r_vis, false)),
    )
    .unwrap();
    rr.renderable(
        unit,
        RenderBinding {
            visibility: Some(r_vis),
            ..Default::default()
        },
    )
    .unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let a = rt.spawn(unit, vec![(f_hp, Value::Int(30))]);
    let b = rt.spawn(unit, vec![(f_hp, Value::Int(30))]);
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 1.0);
    assert_eq!(rr.submit().len(), 2, "两个实体都可见");

    rt.debug_write(a, f_hp, Value::Int(0)); // a 死，反应隐藏 a
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 1.0);
    let view = rr.submit();
    assert_eq!(view.len(), 1, "a 被剔除，仅 b 进提交");
    assert_eq!(view.packets[0].inst, b);
}

// ---- render 自管死亡淡出 ----

#[test]
fn death_fade_defers_reclaim_then_collects() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("hp", Value::Int(30))], false);
    let f_hp = rt.field(unit, "hp");
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let r_fade = rr.add_render_field(unit, Value::Float(1.0));
    rr.set_death_fade(unit, r_fade, 1.0).unwrap(); // 1 秒淡出
    rr.renderable(
        unit,
        RenderBinding {
            fade: Some(r_fade),
            ..Default::default()
        },
    )
    .unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let u = rt.spawn(unit, vec![(f_hp, Value::Int(30))]);
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.1, 1.0);
    assert_eq!(rr.submit().len(), 1, "存活：进提交");

    // sim 杀死 u：render 不即时回收，进入淡出。
    rt.destroy(u);
    rt.step();
    publisher.publish(&rt);
    pump(&mut rr, &publisher); // 仅摄入：dying 入列、剩余=1.0、fade 仍默认 1.0
    assert!(rr.is_present(u), "淡出期内 render 侧仍在场");
    assert_eq!(rr.dying_count(), 1);

    // 每帧 dt=0.25：fade 1→0.75→0.5→0.25→0（第 4 帧回收）。
    for (i, want) in [0.75, 0.5, 0.25].into_iter().enumerate() {
        rr.render_frame(0.25, 1.0);
        let f = rr.read(u, r_fade).as_f64().unwrap();
        assert!(approx(f, want), "第 {} 帧 fade={f} 期望 {want}", i + 1);
        let view = rr.submit();
        assert_eq!(view.len(), 1, "淡出中仍进提交");
        assert!(approx(view.packets[0].fade, want), "提交包带淡出权重");
        assert!(rr.is_present(u), "未淡尽仍在场");
    }
    rr.render_frame(0.25, 1.0); // 剩余 0 → 回收
    assert!(!rr.is_present(u), "淡尽：render 行回收");
    assert_eq!(rr.dying_count(), 0);
    assert_eq!(rr.submit().len(), 0, "回收后不再进提交");
}

#[test]
fn duplicate_deaths_do_not_duplicate_or_reset_fade() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("hp", Value::Int(30))], false);
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let r_fade = rr.add_render_field(unit, Value::Float(1.0));
    rr.set_death_fade(unit, r_fade, 1.0).unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let u = rt.spawn(unit, vec![]);
    rt.step();
    publisher.publish(&rt);
    pump(&mut rr, &publisher);

    rr.ingest(&SimFrame {
        sim_frame: 2,
        deaths: vec![u, u],
        ..Default::default()
    });
    assert_eq!(rr.dying_count(), 1, "同一实例重复 death 只占一个淡出槽");
    rr.render_frame(0.25, 1.0);
    assert!(approx(rr.read(u, r_fade).as_f64().unwrap(), 0.75));

    rr.ingest(&SimFrame {
        sim_frame: 3,
        deaths: vec![u],
        ..Default::default()
    });
    assert_eq!(rr.dying_count(), 1, "淡出中重复 death 不重置时长");
    rr.render_frame(0.25, 1.0);
    assert!(
        approx(rr.read(u, r_fade).as_f64().unwrap(), 0.5),
        "fade 继续单调下降"
    );
}

#[test]
fn death_fade_rejects_non_finite_or_non_positive_duration() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("hp", Value::Int(30))], false);
    rt.enable_render_feed();

    for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut rr = RenderRuntime::new(&rt);
        let r_fade = rr.add_render_field(unit, Value::Float(1.0));
        assert!(
            rr.set_death_fade(unit, r_fade, bad).is_err(),
            "非法淡出时长 {bad:?} 必须在注册期被拒绝"
        );
    }
}

#[test]
fn same_frame_respawn_does_not_leave_old_generation_dying() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("hp", Value::Int(30))], false);
    let f_hp = rt.field(unit, "hp");
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let r_fade = rr.add_render_field(unit, Value::Float(1.0));
    rr.set_death_fade(unit, r_fade, 1.0).unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let a = rt.spawn(unit, vec![(f_hp, Value::Int(30))]);
    rt.step();
    publisher.publish(&rt);
    pump(&mut rr, &publisher);

    rt.destroy(a);
    let b = rt.spawn(unit, vec![(f_hp, Value::Int(30))]);
    assert_eq!(a.id, b.id, "sim 同帧复用 id");
    rt.step();
    publisher.publish(&rt);
    pump(&mut rr, &publisher);

    assert_eq!(
        rr.dying_count(),
        0,
        "旧代 death 被同帧新生夺回，不留空转尸体"
    );
    assert!(!rr.is_present(a));
    assert!(rr.is_present(b));
}

#[test]
fn same_frame_respawn_does_not_keep_old_generation_active_track() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("pos", Value::Int(0))], false);
    let f_pos = rt.field(unit, "pos");
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let r_pos = rr.track(unit, f_pos, Interp::Lerp).unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let a = rt.spawn(unit, vec![(f_pos, Value::Int(0))]);
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 1.0);

    rt.debug_write(a, f_pos, Value::Int(10));
    rt.destroy(a);
    let b = rt.spawn(unit, vec![(f_pos, Value::Int(20))]);
    assert_eq!(a.id, b.id, "sim 同帧复用 id");
    rt.step();
    publisher.publish(&rt);
    pump(&mut rr, &publisher);

    assert_eq!(
        rr.active_count(),
        1,
        "只有新 generation 的 track 留在 active"
    );
    rr.render_frame(0.016, 1.0);
    assert_eq!(rr.read(b, r_pos), Value::Float(20.0));
    assert_eq!(rr.read(a, r_pos), Value::Null, "旧 generation 不可读");
}

#[test]
fn death_fade_reclaim_prunes_active_tracks() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("pos", Value::Int(0))], false);
    let f_pos = rt.field(unit, "pos");
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let _r_pos = rr.track(unit, f_pos, Interp::Lerp).unwrap();
    let r_fade = rr.add_render_field(unit, Value::Float(1.0));
    rr.set_death_fade(unit, r_fade, 0.5).unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let u = rt.spawn(unit, vec![(f_pos, Value::Int(0))]);
    rt.step();
    publisher.publish(&rt);
    pump(&mut rr, &publisher);

    rt.debug_write(u, f_pos, Value::Int(10));
    rt.step();
    publisher.publish(&rt);
    pump(&mut rr, &publisher);
    assert_eq!(rr.active_count(), 1);

    rr.ingest(&SimFrame {
        sim_frame: 3,
        deaths: vec![u],
        ..Default::default()
    });
    rr.render_frame(0.5, 1.0);
    assert!(!rr.is_present(u));
    assert_eq!(rr.dying_count(), 0);
    assert_eq!(rr.active_count(), 0, "淡尽回收同步清 active 稀疏集");
}

#[test]
fn respawn_during_fade_reclaims_corpse_immediately() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("hp", Value::Int(30))], false);
    let f_hp = rt.field(unit, "hp");
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let r_fade = rr.add_render_field(unit, Value::Float(1.0));
    rr.set_death_fade(unit, r_fade, 1.0).unwrap();
    rr.renderable(
        unit,
        RenderBinding {
            fade: Some(r_fade),
            ..Default::default()
        },
    )
    .unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let a = rt.spawn(unit, vec![(f_hp, Value::Int(30))]);
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.1, 1.0);

    rt.destroy(a);
    rt.step();
    publisher.publish(&rt);
    pump(&mut rr, &publisher);
    rr.render_frame(0.3, 1.0); // a 淡到 0.7，仍在淡出
    assert_eq!(rr.dying_count(), 1);
    assert!(rr.is_present(a));

    // sim 复用 a 的 id 重生 b（新代际）。render 出生摄入应即时夺回行、清淡出残项。
    let b = rt.spawn(unit, vec![(f_hp, Value::Int(30))]);
    assert_eq!(b.id, a.id, "sim 复用了同一 id");
    rt.step();
    publisher.publish(&rt);
    pump(&mut rr, &publisher);
    assert_eq!(rr.dying_count(), 0, "重生清除了淡出尸体");
    assert!(rr.is_present(b), "新住户在场");
    assert!(!rr.is_present(a), "旧代际尸体不再可达");
    assert!(
        approx(rr.read(b, r_fade).as_f64().unwrap(), 1.0),
        "新住户实心（出生重置 fade）"
    );
}

// ---- 动画状态切换 + 进度推进（三类组合）----

#[test]
fn animation_state_switch_resets_phase_and_advances() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("action", Value::Int(0))], false);
    let f_action = rt.field(unit, "action");
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    // 镜像 sim 信号（Snap），动画控制器读它检测状态变化。
    let r_action = rr.track(unit, f_action, Interp::Snap).unwrap();
    let r_state = rr.add_render_field(unit, Value::Int(0));
    let r_phase = rr.add_render_field(unit, Value::Float(0.0));
    // 单写者动画控制器（owns state+phase）：镜像≠当前态则切换并重置进度，否则按 dt 推进。
    rr.continuous(
        "anim",
        unit,
        &[r_state, r_phase],
        Box::new(move |ctx| {
            let mirror = ctx.read(r_action);
            let state = ctx.read(r_state);
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
    rr.renderable(
        unit,
        RenderBinding {
            anim_state: Some(r_state),
            anim_phase: Some(r_phase),
            ..Default::default()
        },
    )
    .unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let u = rt.spawn(unit, vec![(f_action, Value::Int(0))]);
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.1, 1.0); // mirror=0=state → 推进：phase 0→0.1
    rr.render_frame(0.1, 1.0); //                       phase 0.1→0.2
    assert!(
        approx(rr.read(u, r_phase).as_f64().unwrap(), 0.2),
        "同态下进度按 dt 累加"
    );

    // sim 切动作 → 状态切换、进度归零。
    rt.debug_write(u, f_action, Value::Int(2));
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.1, 1.0); // mirror=2≠state(0) → 切换 state=2、phase=0
    let view = rr.submit();
    assert_eq!(view.packets[0].anim_state, Value::Int(2), "动画态切到 2");
    assert!(approx(view.packets[0].anim_phase, 0.0), "切换帧进度归零");

    rr.render_frame(0.1, 1.0); // mirror=2=state → 推进 0→0.1
    assert!(
        approx(rr.read(u, r_phase).as_f64().unwrap(), 0.1),
        "新态下重新累加"
    );
}
