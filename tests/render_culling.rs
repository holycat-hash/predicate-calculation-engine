//! render 侧空间索引 / 可见集剔除 / LOD（§6.1「物化为索引实体」的 render 对偶）。
//!
//! 端到端验证：render 自维护网格（喂自 tracked position 增量），相机每 render 帧查询得
//! 可见集，`continuous` 与 `submit` 收窄到视域内；LOD 距离作为派生 render 字段暴露。
//! 覆盖：近入远出、相机移动重算（render-rate）、距离 + lod_band、AABB 形状、即时死亡
//! 出网格、淡出窗口内仍可见 / 淡尽回收后出网格、淡出中同 id 重生不泄漏旧代际、相机缺席
//! 退化全可见、连续更新只对可见者跑、非剔除类型不受影响、注册期校验。

use pce::{
    Axes, CullShape, EntityTypeId, FieldDef, FieldId, InstanceId, Interp, Publisher, RFieldId,
    RenderBinding, RenderRuntime, Runtime, Value, lod_band,
};

/// 公共测试台：Unit（可渲染、平移走 Vec3Lerp track）+ Cam（相机实例，平移 track 成
/// render 字段供查询）。剔除形状 / dist / fade / continuous 由各测试自行启用。
struct Rig {
    rt: Runtime,
    rr: RenderRuntime,
    unit: EntityTypeId,
    f_upos: FieldId,
    f_cpos: FieldId,
    r_cpos: RFieldId,
    cam: InstanceId,
    publisher: Publisher,
}

fn rig() -> Rig {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type(
        "Unit",
        vec![FieldDef::new("pos", Value::vec3(0.0, 0.0, 0.0))],
        false,
    );
    let cam_ty = rt.register_entity_type(
        "Cam",
        vec![FieldDef::new("pos", Value::vec3(0.0, 0.0, 0.0))],
        false,
    );
    let f_upos = rt.field(unit, "pos");
    let f_cpos = rt.field(cam_ty, "pos");
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let r_upos = rr.track(unit, f_upos, Interp::Vec3Lerp).unwrap();
    let r_cpos = rr.track(cam_ty, f_cpos, Interp::Vec3Lerp).unwrap();
    rr.renderable(
        unit,
        RenderBinding {
            translation: Some(r_upos),
            ..Default::default()
        },
    )
    .unwrap();
    let cam = rt.spawn(cam_ty, vec![]); // 相机实例（默认位 (0,0,0)）
    let publisher = Publisher::new(rr.tracked_fields());
    Rig {
        rt,
        rr,
        unit,
        f_upos,
        f_cpos,
        r_cpos,
        cam,
        publisher,
    }
}

impl Rig {
    fn spawn_unit(&mut self, x: f64, y: f64) -> InstanceId {
        self.rt
            .spawn(self.unit, vec![(self.f_upos, Value::vec3(x, y, 0.0))])
    }
    fn move_cam(&mut self, x: f64, y: f64) {
        self.rt
            .debug_write(self.cam, self.f_cpos, Value::vec3(x, y, 0.0));
    }
    /// 推进一帧：sim step → publish → render sync（drain 摄入 + render_frame，alpha=1
    /// 取 cur，相机零延迟到位）。
    fn frame(&mut self) {
        self.frame_alpha(1.0);
    }
    fn frame_alpha(&mut self, alpha: f64) {
        self.rt.step();
        self.publisher.publish(&self.rt);
        self.rr.sync(&self.publisher, 0.016, alpha);
    }
    /// 本帧提交视图里的实例 id 列（升序确定，便于断言）。
    fn submitted_ids(&self) -> Vec<InstanceId> {
        let mut ids: Vec<InstanceId> = self.rr.submit().packets.iter().map(|p| p.inst).collect();
        ids.sort_by_key(|i| (i.ty.0, i.id));
        ids
    }
}

fn sorted(mut v: Vec<InstanceId>) -> Vec<InstanceId> {
    v.sort_by_key(|i| (i.ty.0, i.id));
    v
}

/// 相机在原点、半径 50：近实体入提交、远实体被剔除。
#[test]
fn near_unit_visible_far_unit_culled() {
    let mut r = rig();
    r.rr.enable_culling(20.0, Axes::XY, r.cam, r.r_cpos, CullShape::Radius(50.0))
        .unwrap();
    r.rr.cull_type(r.unit, r.f_upos, None).unwrap();
    let near = r.spawn_unit(10.0, 0.0);
    let _far = r.spawn_unit(1000.0, 0.0);
    r.frame();
    assert_eq!(r.submitted_ids(), vec![near], "近(10,0)入、远(1000,0)出");
}

/// 相机移动 → 可见集每 render 帧重算（render-rate 剔除）。
#[test]
fn moving_camera_recomputes_visible_set() {
    let mut r = rig();
    r.rr.enable_culling(20.0, Axes::XY, r.cam, r.r_cpos, CullShape::Radius(50.0))
        .unwrap();
    r.rr.cull_type(r.unit, r.f_upos, None).unwrap();
    let a = r.spawn_unit(0.0, 0.0);
    let b = r.spawn_unit(1000.0, 0.0);
    r.frame();
    assert_eq!(r.submitted_ids(), vec![a], "相机原点：a 入 b 出");
    r.move_cam(1000.0, 0.0);
    r.frame();
    assert_eq!(r.submitted_ids(), vec![b], "相机移到(1000,0)：b 入 a 出");
}

/// 可见性判定用 render 采样后的 translation，而不是 sim cur：alpha=0 时仍在旧位置。
#[test]
fn culling_uses_sampled_render_position_for_active_tracks() {
    let mut r = rig();
    r.rr.enable_culling(20.0, Axes::XY, r.cam, r.r_cpos, CullShape::Radius(50.0))
        .unwrap();
    r.rr.cull_type(r.unit, r.f_upos, None).unwrap();
    let u = r.spawn_unit(10.0, 0.0);
    r.frame();
    r.rt.debug_write(u, r.f_upos, Value::vec3(100.0, 0.0, 0.0));
    r.frame_alpha(0.0);
    assert_eq!(
        r.submitted_ids(),
        vec![u],
        "alpha=0 时 packet 仍在旧位置，剔除不能提前按 sim cur 丢包"
    );
}

/// LOD：距离写进 dist 字段，开发者经 lod_band 分档。
#[test]
fn distance_field_exposed_for_lod() {
    let mut r = rig();
    let dist = r.rr.add_render_field(r.unit, Value::Float(-1.0));
    r.rr.enable_culling(50.0, Axes::XY, r.cam, r.r_cpos, CullShape::Radius(100.0))
        .unwrap();
    r.rr.cull_type(r.unit, r.f_upos, Some(dist)).unwrap();
    let u = r.spawn_unit(30.0, 40.0); // 到原点距离 50（3-4-5）
    r.frame();
    let d = r.rr.read(u, dist).as_f64().unwrap();
    assert!((d - 50.0).abs() < 1e-9, "距离写入 dist 字段：{d}");
    assert_eq!(lod_band(d, &[10.0, 60.0, 200.0]), 1, "50 落入第 1 档");
}

/// 相机位姿不可投影（NaN/Inf）→ active=false，退化全可见，不能黑屏。
#[test]
fn non_finite_camera_pose_falls_back_to_all_visible() {
    let mut r = rig();
    r.rr.enable_culling(20.0, Axes::XY, r.cam, r.r_cpos, CullShape::Radius(50.0))
        .unwrap();
    r.rr.cull_type(r.unit, r.f_upos, None).unwrap();
    let near = r.spawn_unit(10.0, 0.0);
    let far = r.spawn_unit(1000.0, 0.0);
    r.frame();
    assert_eq!(r.submitted_ids(), vec![near]);
    r.rt.debug_write(r.cam, r.f_cpos, Value::vec3(f64::NAN, 0.0, 0.0));
    r.frame();
    assert_eq!(
        r.submitted_ids(),
        sorted(vec![near, far]),
        "NaN 相机位姿不可投影 → 退化全可见兜底"
    );
}

/// 实体位姿不可投影时从网格移除：不 panic，也不沿用旧坐标提交幽灵 transform。
#[test]
fn invalid_entity_position_removes_stale_grid_entry() {
    let mut r = rig();
    r.rr.enable_culling(20.0, Axes::XY, r.cam, r.r_cpos, CullShape::Radius(50.0))
        .unwrap();
    r.rr.cull_type(r.unit, r.f_upos, None).unwrap();
    let u = r.spawn_unit(10.0, 0.0);
    r.frame();
    assert_eq!(r.submitted_ids(), vec![u]);
    r.rt.debug_write(u, r.f_upos, Value::Null);
    r.frame();
    assert!(
        r.submitted_ids().is_empty(),
        "position 变 Null 后须移除旧网格坐标，而不是按旧位置继续可见"
    );

    let v = r.spawn_unit(10.0, 0.0);
    r.frame();
    assert_eq!(r.submitted_ids(), vec![v]);
    r.rt.debug_write(v, r.f_upos, Value::vec3(f64::NAN, 0.0, 0.0));
    r.frame();
    assert!(
        r.submitted_ids().is_empty(),
        "NaN position 不应传进 SpatialGrid::update panic，也不应残留旧坐标"
    );
}

/// AABB 剔除形状：y 超半高被剔除。
#[test]
fn aabb_cull_shape() {
    let mut r = rig();
    r.rr.enable_culling(
        20.0,
        Axes::XY,
        r.cam,
        r.r_cpos,
        CullShape::Aabb { hx: 50.0, hy: 10.0 },
    )
    .unwrap();
    r.rr.cull_type(r.unit, r.f_upos, None).unwrap();
    let inside = r.spawn_unit(40.0, 5.0); // |x|≤50,|y|≤10 → 入
    let _out_y = r.spawn_unit(40.0, 50.0); // |y|>10 → 出
    r.frame();
    assert_eq!(r.submitted_ids(), vec![inside], "AABB 框：y 超界剔除");
}

/// 即时死亡（无淡出）→ 从网格移除 → 不再可见。
#[test]
fn immediate_death_removes_from_grid() {
    let mut r = rig();
    r.rr.enable_culling(20.0, Axes::XY, r.cam, r.r_cpos, CullShape::Radius(50.0))
        .unwrap();
    r.rr.cull_type(r.unit, r.f_upos, None).unwrap();
    let u = r.spawn_unit(10.0, 0.0);
    r.frame();
    assert_eq!(r.submitted_ids(), vec![u]);
    r.rt.destroy(u);
    r.frame();
    assert!(
        r.submitted_ids().is_empty(),
        "死亡即时回收 → 出网格 → 不可见"
    );
}

/// 淡出窗口内尸体仍在网格（淡出照画）；淡尽回收后出网格。
#[test]
fn fading_corpse_stays_in_grid_until_reclaim() {
    let mut r = rig();
    let fade = r.rr.add_render_field(r.unit, Value::Float(1.0));
    r.rr.set_death_fade(r.unit, fade, 0.05).unwrap(); // dt 0.016 → 约 4 帧回收
    r.rr.enable_culling(20.0, Axes::XY, r.cam, r.r_cpos, CullShape::Radius(50.0))
        .unwrap();
    r.rr.cull_type(r.unit, r.f_upos, None).unwrap();
    let u = r.spawn_unit(10.0, 0.0);
    r.frame();
    r.rt.destroy(u);
    r.frame(); // 进入淡出，仍在网格
    assert_eq!(r.submitted_ids(), vec![u], "淡出窗口内尸体仍进可见集");
    for _ in 0..6 {
        r.frame();
    }
    assert!(r.submitted_ids().is_empty(), "淡尽回收后出网格");
}

/// 淡出回收发生在本 render 帧内时，submit 不能消费回收前算出的 stale visible。
#[test]
fn fading_reclaim_frame_does_not_submit_reclaimed_entity() {
    let mut r = rig();
    let fade = r.rr.add_render_field(r.unit, Value::Float(1.0));
    r.rr.set_death_fade(r.unit, fade, 0.016).unwrap();
    r.rr.enable_culling(20.0, Axes::XY, r.cam, r.r_cpos, CullShape::Radius(50.0))
        .unwrap();
    r.rr.cull_type(r.unit, r.f_upos, None).unwrap();
    let u = r.spawn_unit(10.0, 0.0);
    r.frame();
    assert_eq!(r.submitted_ids(), vec![u]);
    r.rt.destroy(u);
    r.frame();
    assert!(
        r.submitted_ids().is_empty(),
        "淡出到期当帧已回收，visible/submit 不能再吐旧 InstanceId"
    );
}

/// 淡出中同 id 重生：旧代际须从网格移除，否则成幽灵住户（无 death 事件、永不回收）。
#[test]
fn respawn_same_id_during_fade_does_not_leak_old_generation() {
    let mut r = rig();
    let fade = r.rr.add_render_field(r.unit, Value::Float(1.0));
    r.rr.set_death_fade(r.unit, fade, 10.0).unwrap(); // 长淡出，确保重生时旧代际仍在淡出
    r.rr.enable_culling(20.0, Axes::XY, r.cam, r.r_cpos, CullShape::Radius(50.0))
        .unwrap();
    r.rr.cull_type(r.unit, r.f_upos, None).unwrap();
    let u1 = r.spawn_unit(10.0, 0.0); // 近
    r.frame();
    r.rt.destroy(u1); // sim 杀死 → 释放 id；render 进入淡出（近位仍在网格）
    r.frame();
    let u2 = r.spawn_unit(1000.0, 0.0); // 复用 u1 的 id，生在远处
    assert_eq!(u1.id, u2.id, "sim 复用同一 id 槽");
    assert_ne!(u1, u2, "同 id 不同代际（ABA）：整体不等");
    r.frame(); // u2 出生 → 清 u1 淡出残项 + 从网格移除旧近代际；u2 远被剔除
    assert!(
        r.submitted_ids().is_empty(),
        "u2 远被剔除、u1 旧近代际不泄漏为幽灵提交包"
    );
}

/// death_fade + 同一 sim 帧 destroy/spawn 复用 id：旧代际必须从 grid 移除。
#[test]
fn same_frame_respawn_with_fade_does_not_leave_old_generation_in_grid() {
    let mut r = rig();
    let fade = r.rr.add_render_field(r.unit, Value::Float(1.0));
    r.rr.set_death_fade(r.unit, fade, 10.0).unwrap();
    r.rr.enable_culling(20.0, Axes::XY, r.cam, r.r_cpos, CullShape::Radius(50.0))
        .unwrap();
    r.rr.cull_type(r.unit, r.f_upos, None).unwrap();
    let old = r.spawn_unit(10.0, 0.0);
    r.frame();
    r.rt.destroy(old);
    let new = r.spawn_unit(1000.0, 0.0);
    assert_eq!(old.id, new.id, "测试需要复用同一个 id 槽");
    assert_ne!(old, new, "旧/新代际必须不同");
    r.frame();
    assert!(
        r.submitted_ids().is_empty(),
        "新代际在远处被剔除，旧代际不能残留为近处幽灵"
    );
}

/// 已摄入的静止实体之后再 opt-in culling：cull_type 必须回填当前 live rows。
#[test]
fn late_cull_type_backfills_existing_live_entities() {
    let mut r = rig();
    let u = r.spawn_unit(10.0, 0.0);
    r.frame();
    assert_eq!(r.submitted_ids(), vec![u], "未启用剔除时正常提交");
    r.rr.enable_culling(20.0, Axes::XY, r.cam, r.r_cpos, CullShape::Radius(50.0))
        .unwrap();
    r.rr.cull_type(r.unit, r.f_upos, None).unwrap();
    r.rr.render_frame(0.016, 1.0);
    assert_eq!(
        r.submitted_ids(),
        vec![u],
        "late cull_type 应回填已有静止实体，而不是空桶剔除"
    );
}

/// 相机缺席（被销毁）→ 退化为全可见（避免黑屏），远实体也提交。
#[test]
fn camera_absent_falls_back_to_all_visible() {
    let mut r = rig();
    r.rr.enable_culling(20.0, Axes::XY, r.cam, r.r_cpos, CullShape::Radius(50.0))
        .unwrap();
    r.rr.cull_type(r.unit, r.f_upos, None).unwrap();
    let near = r.spawn_unit(10.0, 0.0);
    let far = r.spawn_unit(1000.0, 0.0);
    r.frame();
    assert_eq!(r.submitted_ids(), vec![near], "有相机：仅近可见");
    r.rt.destroy(r.cam);
    r.frame();
    assert_eq!(
        r.submitted_ids(),
        sorted(vec![near, far]),
        "相机缺席 → 退化全可见（far 也提交）"
    );
}

/// 连续更新只对可见实体跑：近实体计数器每帧推进，远（剔除）实体冻结。
#[test]
fn continuous_runs_only_for_visible() {
    let mut r = rig();
    let counter = r.rr.add_render_field(r.unit, Value::Int(0));
    r.rr.continuous(
        "tick",
        r.unit,
        &[counter],
        Box::new(move |ctx| {
            let c = ctx.read(counter).as_f64().unwrap_or(0.0) as i64;
            ctx.write(counter, Value::Int(c + 1));
        }),
    )
    .unwrap();
    r.rr.enable_culling(20.0, Axes::XY, r.cam, r.r_cpos, CullShape::Radius(50.0))
        .unwrap();
    r.rr.cull_type(r.unit, r.f_upos, None).unwrap();
    let near = r.spawn_unit(10.0, 0.0);
    let far = r.spawn_unit(1000.0, 0.0);
    r.frame();
    r.frame();
    r.frame();
    assert!(
        r.rr.read(near, counter).as_f64().unwrap() >= 3.0,
        "近实体 continuous 每帧推进"
    );
    assert_eq!(
        r.rr.read(far, counter).as_f64().unwrap(),
        0.0,
        "远（剔除）实体 continuous 冻结"
    );
}

/// 非剔除类型不受可见集影响：Prop 未 cull_type，远 Prop 照常全扫提交。
#[test]
fn non_culled_type_scans_all() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type(
        "Unit",
        vec![FieldDef::new("pos", Value::vec3(0.0, 0.0, 0.0))],
        false,
    );
    let prop = rt.register_entity_type(
        "Prop",
        vec![FieldDef::new("pos", Value::vec3(0.0, 0.0, 0.0))],
        false,
    );
    let cam_ty = rt.register_entity_type(
        "Cam",
        vec![FieldDef::new("pos", Value::vec3(0.0, 0.0, 0.0))],
        false,
    );
    let (f_upos, f_ppos, f_cpos) = (
        rt.field(unit, "pos"),
        rt.field(prop, "pos"),
        rt.field(cam_ty, "pos"),
    );
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let r_upos = rr.track(unit, f_upos, Interp::Vec3Lerp).unwrap();
    let r_ppos = rr.track(prop, f_ppos, Interp::Vec3Lerp).unwrap();
    let r_cpos = rr.track(cam_ty, f_cpos, Interp::Vec3Lerp).unwrap();
    rr.renderable(
        unit,
        RenderBinding {
            translation: Some(r_upos),
            ..Default::default()
        },
    )
    .unwrap();
    rr.renderable(
        prop,
        RenderBinding {
            translation: Some(r_ppos),
            ..Default::default()
        },
    )
    .unwrap();
    let cam = rt.spawn(cam_ty, vec![]);
    let publisher = Publisher::new(rr.tracked_fields());

    // 只剔除 Unit；Prop 不入剔除。
    rr.enable_culling(20.0, Axes::XY, cam, r_cpos, CullShape::Radius(50.0))
        .unwrap();
    rr.cull_type(unit, f_upos, None).unwrap();

    let _far_unit = rt.spawn(unit, vec![(f_upos, Value::vec3(1000.0, 0.0, 0.0))]);
    let far_prop = rt.spawn(prop, vec![(f_ppos, Value::vec3(1000.0, 0.0, 0.0))]);
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 1.0);

    let ids: Vec<InstanceId> = rr.submit().packets.iter().map(|p| p.inst).collect();
    assert_eq!(
        ids,
        vec![far_prop],
        "远 Unit 被剔除、远 Prop（非剔除类型）照常提交"
    );
}

/// 注册期校验：须先启用、不可重复启用 / 重复 cull、平移字段须已 track。
#[test]
fn culling_registration_validates() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type(
        "Unit",
        vec![
            FieldDef::new("pos", Value::vec3(0.0, 0.0, 0.0)),
            FieldDef::new("hp", Value::Int(1)), // 未 track 的字段
        ],
        false,
    );
    let cam_ty = rt.register_entity_type(
        "Cam",
        vec![FieldDef::new("pos", Value::vec3(0.0, 0.0, 0.0))],
        false,
    );
    let (f_upos, f_hp, f_cpos) = (
        rt.field(unit, "pos"),
        rt.field(unit, "hp"),
        rt.field(cam_ty, "pos"),
    );
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let _r_upos = rr.track(unit, f_upos, Interp::Vec3Lerp).unwrap();
    let r_cpos = rr.track(cam_ty, f_cpos, Interp::Vec3Lerp).unwrap();
    let cam = rt.spawn(cam_ty, vec![]);

    // 未启用就 cull_type → 错。
    assert!(
        rr.cull_type(unit, f_upos, None).is_err(),
        "须先 enable_culling"
    );

    rr.enable_culling(20.0, Axes::XY, cam, r_cpos, CullShape::Radius(50.0))
        .unwrap();
    // 重复启用 → 错。
    assert!(
        rr.enable_culling(20.0, Axes::XY, cam, r_cpos, CullShape::Radius(50.0))
            .is_err(),
        "enable_culling 只能调一次"
    );
    // cull 未 track 的字段 → 错。
    assert!(
        rr.cull_type(unit, f_hp, None).is_err(),
        "平移字段须已 track 才能喂网格"
    );

    rr.cull_type(unit, f_upos, None).unwrap();
    // 重复 cull 同类型 → 错。
    assert!(
        rr.cull_type(unit, f_upos, None).is_err(),
        "重复 cull 同类型即错"
    );
}

/// 无效形状 / cell_size 在 enable_culling 即拒。
#[test]
fn enable_culling_rejects_invalid_params() {
    let mut r = rig();
    assert!(
        r.rr.enable_culling(0.0, Axes::XY, r.cam, r.r_cpos, CullShape::Radius(50.0))
            .is_err(),
        "cell_size 须正"
    );
    assert!(
        r.rr.enable_culling(20.0, Axes::XY, r.cam, r.r_cpos, CullShape::Radius(-1.0))
            .is_err(),
        "半径须正"
    );
    // 不存在的相机字段。
    let bogus = RFieldId(999);
    assert!(
        r.rr.enable_culling(20.0, Axes::XY, r.cam, bogus, CullShape::Radius(50.0))
            .is_err(),
        "相机位姿字段须存在"
    );
}
