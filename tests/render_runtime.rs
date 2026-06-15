//! 第二个 runtime（动态帧率 render 侧）集成测试。
//!
//! 覆盖：插值跨 alpha 正确（白送 A1：写日志即插值数据）、A8 稀疏性（静止物不进
//! 活动集）、Snap/Lerp 种类（Cr1）、事件反应（became/crossed，复用谓词代数）、
//! 动态帧率（dt/alpha 可变）、出生 snap（不从默认值滑入）、并发双线程握手
//! （Publisher 三缓冲，无数据竞争）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pce::predicate::type_scope;
use pce::{
    Cond, Delivery, FieldDef, Interp, Predicate, Proj, Publisher, RenderRuntime, Runtime, Value,
};

/// 规范消费：drain 全部未消费帧、顺序摄入（不丢生灭/事件），不推进 render 帧。
fn pump(rr: &mut RenderRuntime, publisher: &Publisher) {
    for sf in publisher.drain() {
        rr.ingest(&sf);
    }
}

/// 造一个会动的实体：Unit{pos, vel}，挂一个每帧 `pos += vel` 的 ECS mover。
fn sim_with_mover(vel: i64) -> (Runtime, pce::EntityTypeId, pce::FieldId, pce::FieldId) {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type(
        "Unit",
        vec![
            FieldDef::new("pos", Value::Int(0)),
            FieldDef::new("vel", Value::Int(vel)),
        ],
        false,
    );
    let f_pos = rt.field(unit, "pos");
    let f_vel = rt.field(unit, "vel");
    // ECS mover：type(Clock,frame) + True + each ≅ 经典 ECS system（白送 ECS 快路）。
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
            let pos = ctx.read_own(f_pos).as_i64().unwrap_or(0);
            let vel = ctx.read_own(f_vel).as_i64().unwrap_or(0);
            ctx.write(f_pos, pos + vel);
        }),
    )
    .unwrap();
    rt.enable_render_feed();
    (rt, unit, f_pos, f_vel)
}

#[test]
fn lerp_interpolates_between_sim_frames_across_alpha() {
    let (mut rt, unit, f_pos, _f_vel) = sim_with_mover(10);
    let mut rr = RenderRuntime::new(&rt);
    let r_pos = rr.track(unit, f_pos, Interp::Lerp).unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let u = rt.spawn(unit, vec![(f_pos, Value::Int(0))]);

    // sim 帧 1：出生 + mover 首次推进（store.pos 0→10，但本帧路由集是出生写）。
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 0.0);
    assert_eq!(rr.read(u, r_pos), Value::Float(0.0), "出生帧 snap 到初值 0");

    // sim 帧 2：路由集 = [pos 0→10]。render 在该区间插值 0→10。
    rt.step();
    publisher.publish(&rt);
    pump(&mut rr, &publisher);
    rr.render_frame(0.016, 0.0);
    assert_eq!(rr.read(u, r_pos), Value::Float(0.0), "alpha=0 → prev");
    rr.render_frame(0.016, 0.25);
    assert_eq!(
        rr.read(u, r_pos),
        Value::Float(2.5),
        "alpha=0.25 → 1/4 路程"
    );
    rr.render_frame(0.016, 0.5);
    assert_eq!(rr.read(u, r_pos), Value::Float(5.0), "alpha=0.5 → 半程");
    rr.render_frame(0.016, 1.0);
    assert_eq!(rr.read(u, r_pos), Value::Float(10.0), "alpha=1 → cur");

    // sim 帧 3：路由集 = [pos 10→20]。换区间，插值 10→20。
    rt.step();
    publisher.publish(&rt);
    pump(&mut rr, &publisher);
    rr.render_frame(0.016, 0.5);
    assert_eq!(rr.read(u, r_pos), Value::Float(15.0), "下一区间半程 = 15");
}

#[test]
fn snap_kind_ignores_alpha_takes_current() {
    let (mut rt, unit, f_pos, _v) = sim_with_mover(10);
    let mut rr = RenderRuntime::new(&rt);
    let r_pos = rr.track(unit, f_pos, Interp::Snap).unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let u = rt.spawn(unit, vec![(f_pos, Value::Int(0))]);
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 0.0);
    rt.step();
    publisher.publish(&rt);
    pump(&mut rr, &publisher);
    // Snap：无视 alpha，恒取 cur=10。
    rr.render_frame(0.016, 0.0);
    assert_eq!(rr.read(u, r_pos), Value::Int(10), "Snap alpha=0 仍取 cur");
    rr.render_frame(0.016, 0.5);
    assert_eq!(rr.read(u, r_pos), Value::Int(10), "Snap alpha=0.5 仍取 cur");
}

#[test]
fn static_entity_leaves_active_set_and_holds_value() {
    // 不挂 mover：实体出生后 pos 永不再被写 ⇒ 无增量 ⇒ A8 稀疏：活动集应清空。
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("pos", Value::Int(0))], false);
    let f_pos = rt.field(unit, "pos");
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let r_pos = rr.track(unit, f_pos, Interp::Lerp).unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let u = rt.spawn(unit, vec![(f_pos, Value::Int(42))]);
    rt.step(); // 出生帧
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 1.0);
    assert_eq!(rr.read(u, r_pos), Value::Float(42.0), "出生 snap 到 42");
    assert_eq!(rr.active_count(), 1, "出生区间内仍活动");

    // 后续帧无 pos 写入：活动集结算清空，输出稳定保持 42（不再每帧重算）。
    for _ in 2..=6 {
        rt.step();
        publisher.publish(&rt);
        pump(&mut rr, &publisher);
        for a in [0.0, 0.3, 0.7, 1.0] {
            rr.render_frame(0.016, a);
            assert_eq!(rr.read(u, r_pos), Value::Float(42.0), "静止物输出恒定");
        }
        assert_eq!(rr.active_count(), 0, "静止物已离开活动集（A8 稀疏）");
    }
}

#[test]
fn reaction_fires_on_sim_event_became_and_crossed() {
    // sim：Unit{hp}，被外部攻击降 hp；render 反应：hp 跌穿 0 → 标记死亡视觉态。
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("hp", Value::Int(30))], false);
    let f_hp = rt.field(unit, "hp");
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    // render 字段：death_fx（0 未触发 / 1 已起死亡特效）。
    let r_fx = rr.add_render_field(unit, Value::Int(0));
    rr.reaction(
        "death_fx",
        unit,
        f_hp,
        Cond::Crossed(
            pce::Expr::Val(pce::ValRef::Const(Value::Int(1))),
            pce::Dir::Down,
        ),
        vec![Proj::New(vec![]), Proj::Old(vec![])],
        false,
        &[r_fx],
        Box::new(move |ctx, input| {
            // 投影：new=当前 hp、old=上一 hp。
            let _new = input.arg(0);
            let _old = input.arg(1);
            ctx.write(r_fx, 1);
        }),
    )
    .unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let u = rt.spawn(unit, vec![(f_hp, Value::Int(30))]);
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 1.0);
    assert_eq!(rr.read(u, r_fx), Value::Int(0), "未掉血，无死亡特效");

    // 外部把 hp 砍到 0（跌穿阈值 1，向下）。
    rt.debug_write(u, f_hp, Value::Int(0));
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 1.0);
    assert_eq!(rr.read(u, r_fx), Value::Int(1), "hp 跌穿 0 → 死亡特效起");
}

#[test]
fn reaction_skips_writer_that_dies_in_same_sim_frame() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("hp", Value::Int(30))], false);
    let f_hp = rt.field(unit, "hp");
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let r_hit = rr.add_render_field(unit, Value::Int(0));
    let r_fade = rr.add_render_field(unit, Value::Float(1.0));
    rr.set_death_fade(unit, r_fade, 1.0).unwrap();
    rr.reaction(
        "hit_fx",
        unit,
        f_hp,
        Cond::Changed,
        vec![],
        false,
        &[r_hit],
        Box::new(move |ctx, _| ctx.write(r_hit, 1)),
    )
    .unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let u = rt.spawn(unit, vec![(f_hp, Value::Int(30))]);
    rt.step();
    publisher.publish(&rt);
    pump(&mut rr, &publisher);

    rt.debug_write(u, f_hp, Value::Int(0));
    rt.destroy(u);
    rt.step();
    publisher.publish(&rt);
    pump(&mut rr, &publisher);

    assert!(
        rr.is_present(u),
        "死亡淡出保留 render 行，便于观察 reaction 是否误写"
    );
    assert_eq!(
        rr.read(u, r_hit),
        Value::Int(0),
        "同帧死亡 writer 与 sim route 一致：不触发 writer-self render reaction"
    );
}

#[test]
fn continuous_calc_accumulates_with_dt() {
    // 连续更新：每 render 帧 timer += dt（动态帧率：按时间积分，不按帧数）。
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("x", Value::Int(0))], false);
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let r_timer = rr.add_render_field(unit, Value::Float(0.0));
    rr.continuous(
        "tick_timer",
        unit,
        &[r_timer],
        Box::new(move |ctx| {
            let t = ctx.read(r_timer).as_f64().unwrap_or(0.0);
            ctx.write(r_timer, t + ctx.dt());
        }),
    )
    .unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let u = rt.spawn(unit, vec![]);
    rt.step();
    publisher.publish(&rt);
    pump(&mut rr, &publisher);

    // 变帧率：dt 每帧不同，timer 累加真实经过时间。
    let dts = [0.016, 0.033, 0.008, 0.020];
    let mut expect = 0.0;
    for dt in dts {
        rr.render_frame(dt, 0.5);
        expect += dt;
    }
    let got = rr.read(u, r_timer).as_f64().unwrap();
    assert!(
        (got - expect).abs() < 1e-9,
        "timer 累加变 dt：{got} vs {expect}"
    );
}

#[test]
fn concurrent_two_thread_handoff_no_data_race() {
    // 并发双线程：sim 线程 step+publish；render 线程消费 latest()。
    // 不可变 SimFrame + Arc 即并发安全（A7）；断言无 panic、最终值正确。
    const FRAMES: u64 = 200;
    let (mut rt, unit, f_pos, _v) = sim_with_mover(10);
    let mut rr = RenderRuntime::new(&rt);
    let r_pos = rr.track(unit, f_pos, Interp::Lerp).unwrap();
    let publisher = Arc::new(Publisher::new(rr.tracked_fields()));
    let done = Arc::new(AtomicBool::new(false));

    let u = rt.spawn(unit, vec![(f_pos, Value::Int(0))]);

    let sim_pub = Arc::clone(&publisher);
    let sim_done = Arc::clone(&done);
    let sim = std::thread::spawn(move || {
        for _ in 1..=FRAMES {
            rt.step();
            sim_pub.publish(&rt);
            std::thread::yield_now();
        }
        sim_done.store(true, Ordering::Release);
    });

    let ren_pub = Arc::clone(&publisher);
    let ren_done = Arc::clone(&done);
    let render = std::thread::spawn(move || {
        // 动态帧率：render 在自己的节奏上插值。每 tick 顺序 drain 全部未消费帧
        // （不丢出生/事件），插值落在最后一帧；直到 sim 收工后再排空一次。
        let mut alpha = 0.0;
        loop {
            for sf in ren_pub.drain() {
                rr.ingest(&sf);
            }
            rr.render_frame(0.016, alpha);
            alpha = if alpha >= 1.0 { 0.0 } else { alpha + 0.34 };
            if ren_done.load(Ordering::Acquire) {
                break;
            }
            std::thread::yield_now();
        }
        // 收工后排空剩余帧，最后 alpha=1 应等于该区间 cur。
        for sf in ren_pub.drain() {
            rr.ingest(&sf);
        }
        rr.render_frame(0.016, 1.0);
        (rr.last_ingested(), rr.read(u, r_pos), rr.is_present(u))
    });

    sim.join().unwrap();
    let (ingested, final_pos, present) = render.join().unwrap();

    assert!(present, "并发结束后实体在 render 侧仍在场");
    assert!(ingested >= 1, "render 至少摄入了一帧（握手生效）");
    // 最终帧 alpha=1 → cur。cur = 上一 sim 帧的 pos 推进值，必为 10 的整数倍且有限。
    let p = final_pos.as_f64().expect("插值输出为数值");
    assert!(p.is_finite() && p >= 0.0, "最终插值输出有限且非负：{p}");
    assert!(
        (p / 10.0).fract().abs() < 1e-9,
        "alpha=1 落在 sim 帧整点 {p}"
    );
}

// ---- 审查发现的回归锁定 ----

#[test]
fn multiple_tracks_on_same_field_all_update() {
    // 回归（审查 #3/#5）：同一 sim 字段可被多个 track 镜像；旧实现 track_of 覆盖，
    // 除最后注册者外全停在 Null。现两者都应更新。
    let (mut rt, unit, f_pos, _v) = sim_with_mover(10);
    let mut rr = RenderRuntime::new(&rt);
    let r_lerp = rr.track(unit, f_pos, Interp::Lerp).unwrap();
    let r_snap = rr.track(unit, f_pos, Interp::Snap).unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let u = rt.spawn(unit, vec![(f_pos, Value::Int(0))]);
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 1.0);
    rt.step();
    publisher.publish(&rt);
    pump(&mut rr, &publisher);
    rr.render_frame(0.016, 0.5);
    // 区间 0→10：Lerp 半程 = 5，Snap = cur = 10。两个输出都活着。
    assert_eq!(rr.read(u, r_lerp), Value::Float(5.0), "Lerp 轨道更新");
    assert_eq!(
        rr.read(u, r_snap),
        Value::Int(10),
        "Snap 轨道同样更新（不再被覆盖成 Null）"
    );
}

#[test]
fn birth_snaps_uninitialized_tracked_field_to_sim_default() {
    // 回归（审查 #4）：spawn 未初始化的 tracked 字段，旧实现 out 永停 Null；
    // 现应 snap 到该字段的 sim schema 默认值。
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type(
        "Unit",
        vec![
            FieldDef::new("pos", Value::Int(0)),
            FieldDef::new("facing", Value::Int(7)),
        ],
        false,
    );
    let f_facing = rt.field(unit, "facing");
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let r_facing = rr.track(unit, f_facing, Interp::Snap).unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    // 只初始化 pos，facing 取 schema 默认值 7（不产生写日志增量）。
    let u = rt.spawn(unit, vec![(rt.field(unit, "pos"), Value::Int(3))]);
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 1.0);
    assert_eq!(
        rr.read(u, r_facing),
        Value::Int(7),
        "出生 snap 到 sim 默认值 7，非 Null"
    );
}

#[test]
fn render_slower_than_sim_drains_all_no_ghost() {
    // 回归（审查 #1/#2）：render 比 sim 慢、一次 drain 多帧。旧 latest() 路径会跳过
    // 中间帧丢死亡 → 永不回收的幽灵。现 drain 顺序摄入全部，死亡不丢。
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("pos", Value::Int(0))], false);
    let f_pos = rt.field(unit, "pos");
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let _r = rr.track(unit, f_pos, Interp::Lerp).unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let b = rt.spawn(unit, vec![(f_pos, Value::Int(0))]);
    rt.step();
    publisher.publish(&rt); // 出生 B（render 尚未消费）
    rt.destroy(b);
    rt.step();
    publisher.publish(&rt); // B 死亡（render 仍未消费）
    rt.step();
    publisher.publish(&rt); // 又一帧

    // render 现在一次性 drain 三帧，顺序摄入：出生 → 死亡 → 第三帧。
    pump(&mut rr, &publisher);
    assert!(
        !rr.is_present(b),
        "死亡帧未被跳过：B 在 render 侧已回收，无幽灵"
    );
}

#[test]
fn render_d1_collision_and_duplicate_writes_error() {
    // 回归（审查 #5/#7）：render 字段单写者；重复声明 / 跨注册冲突注册期报错。
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("x", Value::Int(0))], false);
    rt.enable_render_feed();
    let mut rr = RenderRuntime::new(&rt);
    let rf = rr.add_render_field(unit, Value::Int(0));

    // 第一个 continuous 占有 rf。
    rr.continuous("a", unit, &[rf], Box::new(|_| {})).unwrap();
    // 第二个想写 rf → D1 冲突。
    let dup = rr.continuous("b", unit, &[rf], Box::new(|_| {}));
    assert!(dup.is_err(), "render 字段已归属 → 冲突应报错");

    // 一次声明里重复同字段 → 报错（且不留半截脏归属）。
    let rg = rr.add_render_field(unit, Value::Int(0));
    let dupself = rr.continuous("c", unit, &[rg, rg], Box::new(|_| {}));
    assert!(dupself.is_err(), "片内重复声明应报错");
    // rg 既然注册失败，应仍可被后续合法注册者占有（无幽灵归属）。
    assert!(
        rr.continuous("d", unit, &[rg], Box::new(|_| {})).is_ok(),
        "失败注册不留脏归属"
    );
}

#[test]
#[should_panic(expected = "折叠序未定义")]
fn render_detect_strict_panics_on_conflicting_fold() {
    // 新增能力（与 sim C5 Detect 对齐）：同一 render calc 一次运行对同字段写不同值，
    // Strict 档 panic——render 不再独有「永远静默 last-wins」的弱策略。
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("x", Value::Int(0))], false);
    rt.enable_render_feed();
    let mut rr = RenderRuntime::new(&rt);
    rr.set_detect(pce::Detect::Strict);
    let rf = rr.add_render_field(unit, Value::Int(0));
    rr.continuous(
        "conflict",
        unit,
        &[rf],
        Box::new(move |ctx| {
            ctx.write(rf, 1);
            ctx.write(rf, 2); // 同字段写不同值 → Strict 应 panic
        }),
    )
    .unwrap();
    let publisher = Publisher::new(rr.tracked_fields());
    let _u = rt.spawn(unit, vec![]);
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 0.5); // render_frame 跑 continuous → 折叠冲突 panic
}

#[test]
fn render_detect_silent_folds_last_wins() {
    // Silent 档（与 sim 同纪律）：同字段多写不同值静默折叠为 last-wins，不 panic。
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("x", Value::Int(0))], false);
    rt.enable_render_feed();
    let mut rr = RenderRuntime::new(&rt);
    rr.set_detect(pce::Detect::Silent);
    let rf = rr.add_render_field(unit, Value::Int(0));
    rr.continuous(
        "conflict",
        unit,
        &[rf],
        Box::new(move |ctx| {
            ctx.write(rf, 1);
            ctx.write(rf, 2);
        }),
    )
    .unwrap();
    let publisher = Publisher::new(rr.tracked_fields());
    let u = rt.spawn(unit, vec![]);
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 0.5);
    assert_eq!(rr.read(u, rf), Value::Int(2), "Silent 折叠为 last-wins=2");
}
