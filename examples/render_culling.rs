//! 演示：render 侧空间索引 / 可见集剔除 / LOD（§6.1「物化为索引实体」的 render 对偶）。
//!
//! 布局：5 个静止 Unit 沿 x 轴排开；一台相机每 sim 帧 +50 横扫世界，render 按 render
//! 帧率插值相机位（Vec3Lerp track）。render 每帧用相机查询自维护的网格得**可见集**：
//! - `submit()` 只装可见集里的实体（视域外不提交）；
//! - 引擎把每个可见实体到相机的距离写进 `dist` 字段；
//! - 一个 `continuous` 读 `dist`、经 `lod_band` 分档、据档选 mesh handle（LOD：引擎给
//!   距离，开发者定分档——这里 25 / 60 两道阈值切 3 档）。
//!
//! 全程铁律不破：render 只读 sim（经 SimFrame 冻结快照）、只写 render 字段；剔除是个
//! 索引 + 查询，不是第四注册概念。
//!
//! 运行：cargo run --example render_culling
//!
//! 看点：相机横扫时可见集随之滑动（近的进、远的出），同一实体随相机接近 LOD 档号下降
//! （越近越精细）；最远的 Unit（x=400）只有相机扫到附近才进可见集、且以最粗档提交。

use pce::predicate::type_scope;
use pce::{
    Axes, Cond, CullShape, Delivery, FieldDef, Interp, Predicate, Publisher, RenderBinding,
    RenderRuntime, Runtime, Value, lod_band,
};

/// LOD 分档阈值（升序）：dist <25 → 档 0（最精），<60 → 档 1，否则档 2（最粗）。
const LOD_BANDS: [f64; 2] = [25.0, 60.0];

fn main() {
    // ---- sim 侧（固定步长）：静止 Unit + 每帧横移的相机 ----
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

    // ECS：相机每帧 +50 横扫（订阅内建 Clock.frame = 每帧跑一次）。
    let (cty, cframe) = {
        let c = rt.clock();
        (c.ty, c.f_frame)
    };
    rt.register_calculation(
        "cam_advance",
        cam_ty,
        Predicate::new(type_scope(cty, cframe), Cond::True, Delivery::Each(vec![])),
        &[f_cpos],
        Box::new(move |ctx, _| {
            let p = ctx.read_own(f_cpos).as_vec3().unwrap_or([0.0; 3]);
            ctx.write(f_cpos, Value::vec3(p[0] + 50.0, p[1], p[2]));
        }),
    )
    .unwrap();
    rt.enable_render_feed();

    // ---- render 侧（动态帧率，消费者）----
    let mut rr = RenderRuntime::new(&rt);
    let r_upos = rr.track(unit, f_upos, Interp::Vec3Lerp).unwrap();
    let r_cpos = rr.track(cam_ty, f_cpos, Interp::Vec3Lerp).unwrap();

    // 派生字段：dist（引擎每帧写可见实体到相机的距离）、lod（continuous 分档）、mesh（按档选）。
    let dist = rr.add_render_field(unit, Value::Float(0.0));
    let r_lod = rr.add_render_field(unit, Value::Int(0));
    let r_mesh = rr.add_render_field(unit, Value::Int(0));

    // LOD 控制器：读 dist → lod_band → 写档号与 mesh handle（mesh = 300 + 档号）。
    rr.continuous(
        "lod",
        unit,
        &[r_lod, r_mesh],
        Box::new(move |ctx| {
            let d = ctx.read(dist).as_f64().unwrap_or(0.0);
            let band = lod_band(d, &LOD_BANDS) as i64;
            ctx.write(r_lod, Value::Int(band));
            ctx.write(r_mesh, Value::Int(300 + band));
        }),
    )
    .unwrap();

    rr.renderable(
        unit,
        RenderBinding {
            translation: Some(r_upos),
            mesh: Some(r_mesh),
            ..Default::default()
        },
    )
    .unwrap();

    // 相机实例 → 启用剔除（网格格 50、XY 平面、半径 100 视域）→ Unit opt-in 剔除 + 距离输出。
    let cam = rt.spawn(cam_ty, vec![]);
    rr.enable_culling(50.0, Axes::XY, cam, r_cpos, CullShape::Radius(100.0))
        .unwrap();
    rr.cull_type(unit, f_upos, Some(dist)).unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    // 5 个静止 Unit 沿 x 轴。
    let xs = [0.0, 40.0, 90.0, 160.0, 400.0];
    for x in xs {
        rt.spawn(unit, vec![(f_upos, Value::vec3(x, 0.0, 0.0))]);
    }

    // ---- 帧循环：相机每帧横移 50，render 重算可见集 + LOD ----
    println!("== render 侧剔除 + LOD（相机半径 100，每帧 +50 横扫）==");
    println!("Unit 静止于 x = {xs:?}（y=0）；相机从 x=0 起步\n");
    // 相机第 1 帧刚出生（时钟驱动的 cam_advance 下一帧才作用到它），故实际相机位
    // 滞后帧号一格：第 N 帧相机在 x=(N−1)·50。
    for sim_frame in 1..=8u64 {
        rt.step();
        publisher.publish(&rt);
        for sf in publisher.drain() {
            rr.ingest(&sf);
        }
        rr.render_frame(0.016, 1.0); // alpha=1：相机插值到位

        let cam_x = ((sim_frame - 1) * 50) as f64;
        let view = rr.submit();
        println!("相机 x≈{cam_x:>3.0}：可见 {} / 5", view.len());
        for p in &view.packets {
            let px = p.translation.as_vec3().map_or(0.0, |a| a[0]);
            let d = rr.read(p.inst, dist).as_f64().unwrap_or(0.0);
            let lod = rr.read(p.inst, r_lod).as_f64().unwrap_or(0.0) as i64;
            let mesh = match &p.mesh {
                Value::Int(m) => *m,
                _ => -1,
            };
            println!("    Unit@x={px:>5.0}   dist={d:>6.1}   LOD={lod}（mesh={mesh}）");
        }
    }

    println!(
        "\n相机扫过时可见集滑动（近进远出）、LOD 随距离变粗细；最远的 x=400 仅相机临近才进集。"
    );
    println!("剔除收窄了 continuous 与 submit 的扫描 N，未启用时退化为全扫——行为与今日逐字相同。");
}
