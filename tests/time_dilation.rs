//! 22 顿帧 / 子弹时间 / 局部时间膨胀（docs/22-time-dilation.md）的可运行验证：
//! 本地时间轴单写者推进、时间戳守卫换轴（冷却、无敌帧）、冻结即停轴自动顺延、
//! 整数分数累加的慢放速率（无浮点漂移）。

use pce::predicate::{new_path, own, own_field, type_scope};
use pce::{
    CmpOp, Cond, Delivery, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, ValRef, Value,
};

/// 冷却时长（本地帧）。
const CD: i64 = 30;
/// 闪避无敌时长（本地帧）。
const IFRAMES: i64 = 5;

fn path(v: &Value, key: &str) -> Value {
    v.get_path(&[key.to_string()])
}

fn as_i64(v: &Value) -> i64 {
    v.as_f64().unwrap_or(0.0) as i64
}

#[derive(Clone, Copy)]
struct F {
    pace_req: FieldId,
    stop_req: FieldId,
    cast_req: FieldId,
    dodge_req: FieldId,
    attack_out: FieldId,
    local_time: FieldId,
    cast_count: FieldId,
    cd_ready_at: FieldId,
    invuln_until: FieldId,
    hp: FieldId,
}

fn setup() -> (Runtime, EntityTypeId, EntityTypeId, F) {
    let mut rt = Runtime::new();
    let body = rt.register_entity_type(
        "Body",
        vec![
            FieldDef::new("pace_req", Value::Null),
            FieldDef::new("stop_req", Value::Null),
            FieldDef::new("cast_req", Value::Null),
            FieldDef::new("dodge_req", Value::Null),
            FieldDef::new("local_time", Value::Int(0)),
            FieldDef::new("acc", Value::Int(0)),
            FieldDef::new("pace_num", Value::Int(1)),
            FieldDef::new("pace_den", Value::Int(1)),
            FieldDef::new("pace_seq", Value::Int(0)),
            FieldDef::new("freeze_left", Value::Int(0)),
            FieldDef::new("stop_seq", Value::Int(0)),
            FieldDef::new("cast_count", Value::Int(0)),
            FieldDef::new("cd_ready_at", Value::Int(0)),
            FieldDef::new("invuln_until", Value::Int(0)),
            FieldDef::new("hp", Value::Int(100)),
        ],
        false,
    );
    let attacker =
        rt.register_entity_type("Attacker", vec![FieldDef::new("attack_out", Value::Null)], false);
    let f = F {
        pace_req: rt.field(body, "pace_req"),
        stop_req: rt.field(body, "stop_req"),
        cast_req: rt.field(body, "cast_req"),
        dodge_req: rt.field(body, "dodge_req"),
        attack_out: rt.field(attacker, "attack_out"),
        local_time: rt.field(body, "local_time"),
        cast_count: rt.field(body, "cast_count"),
        cd_ready_at: rt.field(body, "cd_ready_at"),
        invuln_until: rt.field(body, "invuln_until"),
        hp: rt.field(body, "hp"),
    };
    let f_acc = rt.field(body, "acc");
    let f_num = rt.field(body, "pace_num");
    let f_den = rt.field(body, "pace_den");
    let f_pseq = rt.field(body, "pace_seq");
    let f_freeze = rt.field(body, "freeze_left");
    let f_sseq = rt.field(body, "stop_seq");

    // 1 时间轴：已付轮询的单写者（动画/物理本就每帧推进）；
    //   速率变更与冻结在同一次运行内原子吸收（seq 单调防重复采样）
    let clock_ty = rt.clock().ty;
    let clock_frame = rt.clock().f_frame;
    let (f_local, f_pace_req, f_stop_req) = (f.local_time, f.pace_req, f.stop_req);
    rt.register_calculation(
        "timekeeper",
        body,
        Predicate::new(type_scope(clock_ty, clock_frame), Cond::True, Delivery::Each(vec![])),
        &[f_local, f_acc, f_num, f_den, f_pseq, f_freeze, f_sseq],
        Box::new(move |ctx, _| {
            let mut local = as_i64(&ctx.read_own(f_local));
            let mut acc = as_i64(&ctx.read_own(f_acc));
            let mut num = as_i64(&ctx.read_own(f_num));
            let mut den = as_i64(&ctx.read_own(f_den));
            let mut pseq = as_i64(&ctx.read_own(f_pseq));
            let mut freeze = as_i64(&ctx.read_own(f_freeze));
            let mut sseq = as_i64(&ctx.read_own(f_sseq));

            let pr = ctx.read_own(f_pace_req);
            if as_i64(&path(&pr, "seq")) > pseq {
                pseq = as_i64(&path(&pr, "seq"));
                num = as_i64(&path(&pr, "num")).max(0);
                den = as_i64(&path(&pr, "den")).max(1);
                acc = 0;
            }
            let sr = ctx.read_own(f_stop_req);
            if as_i64(&path(&sr, "seq")) > sseq {
                sseq = as_i64(&path(&sr, "seq"));
                freeze = freeze.max(as_i64(&path(&sr, "frames"))); // 顿帧刷新取 max
            }
            if freeze > 0 {
                freeze -= 1; // 停轴：不触碰任何挂起戳，顺延是停轴的推论
            } else {
                // 整数分数累加（Bresenham）：acc < den 恒成立，无浮点漂移
                acc += num;
                local += acc / den;
                acc %= den;
            }
            ctx.write(f_local, Value::Int(local));
            ctx.write(f_acc, Value::Int(acc));
            ctx.write(f_num, Value::Int(num));
            ctx.write(f_den, Value::Int(den));
            ctx.write(f_pseq, Value::Int(pseq));
            ctx.write(f_freeze, Value::Int(freeze));
            ctx.write(f_sseq, Value::Int(sseq));
        }),
    )
    .unwrap();

    // 2 冷却：时间戳守卫（手法 4）换轴——形不变，轴从 Clock.frame 换成 own.local_time；
    //   冷却中的施放请求零触发拒绝
    let (f_count, f_cd) = (f.cast_count, f.cd_ready_at);
    rt.register_calculation(
        "skill",
        body,
        Predicate::new(
            own(f.cast_req),
            Cond::Cmp(own_field(f_local), CmpOp::Ge, own_field(f_cd)),
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[f_count, f_cd],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            if rows.is_empty() {
                return;
            }
            let local = as_i64(&ctx.read_own(f_local));
            ctx.write(f_count, Value::Int(as_i64(&ctx.read_own(f_count)) + 1));
            ctx.write(f_cd, Value::Int(local + CD)); // 盖戳也在本地轴
        }),
    )
    .unwrap();

    // 3 闪避：无敌窗以本地轴盖戳
    let f_inv = f.invuln_until;
    rt.register_calculation(
        "dodge",
        body,
        Predicate::new(own(f.dodge_req), Cond::True, Delivery::Each(vec![])),
        &[f_inv],
        Box::new(move |ctx, _| {
            let local = as_i64(&ctx.read_own(f_local));
            ctx.write(f_inv, Value::Int(local + IFRAMES));
        }),
    )
    .unwrap();

    // 4 受击：跨实体命中用**被判定者**的轴（i 帧是受击者的属性）；无敌中零触发
    let f_hp = f.hp;
    rt.register_calculation(
        "take_damage",
        body,
        Predicate::new(
            type_scope(attacker, f.attack_out),
            Cond::And(
                Box::new(Cond::Cmp(new_path(&["target"]), CmpOp::Eq, Expr::Val(ValRef::SelfRef))),
                Box::new(Cond::Cmp(own_field(f_local), CmpOp::Ge, own_field(f_inv))),
            ),
            Delivery::Batch(vec![Proj::New(vec!["dmg".to_string()])]),
        ),
        &[f_hp],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let dmg: i64 = rows.iter().map(|r| as_i64(&r[0])).sum();
            ctx.write(f_hp, Value::Int(as_i64(&ctx.read_own(f_hp)) - dmg));
        }),
    )
    .unwrap();

    (rt, body, attacker, f)
}

fn local(rt: &Runtime, p: InstanceId, f: &F) -> i64 {
    as_i64(&rt.read(p, f.local_time))
}

fn step_n(rt: &mut Runtime, n: usize) {
    for _ in 0..n {
        rt.step();
    }
}

#[test]
fn hitstop_suspends_cooldown_on_local_axis() {
    let (mut rt, body, _, f) = setup();
    let p = rt.spawn(body, vec![]);
    step_n(&mut rt, 2); // local = 2

    rt.debug_write(p, f.cast_req, Value::map([("seq", Value::Int(1))]));
    rt.step(); // 守卫 local ≥ cd_ready_at(0)：放行
    assert_eq!(as_i64(&rt.read(p, f.cast_count)), 1);
    assert_eq!(as_i64(&rt.read(p, f.cd_ready_at)), 2 + CD);

    // 顿帧 20 帧：本地轴停摆
    rt.debug_write(
        p,
        f.stop_req,
        Value::map([("frames", Value::Int(20)), ("seq", Value::Int(1))]),
    );
    step_n(&mut rt, 30); // 全局过 30 帧，本地只走了 10
    assert_eq!(local(&rt, p, &f), 13);

    // 全局帧差早已超过 CD，但本地轴未到：守卫零触发拒绝
    rt.debug_write(p, f.cast_req, Value::map([("seq", Value::Int(2))]));
    rt.step();
    assert_eq!(as_i64(&rt.read(p, f.cast_count)), 1);

    // 本地轴走满 CD 后放行——冷却被顿帧自动顺延，无人改过 cd_ready_at
    while local(&rt, p, &f) < 2 + CD {
        rt.step();
    }
    rt.debug_write(p, f.cast_req, Value::map([("seq", Value::Int(3))]));
    rt.step();
    assert_eq!(as_i64(&rt.read(p, f.cast_count)), 2);
}

#[test]
fn slow_motion_advances_local_axis_fractionally() {
    let (mut rt, body, _, f) = setup();
    let p = rt.spawn(body, vec![]);

    rt.debug_write(
        p,
        f.pace_req,
        Value::map([("num", Value::Int(1)), ("den", Value::Int(2)), ("seq", Value::Int(1))]),
    );
    step_n(&mut rt, 20); // 半速：整数分数累加，恰好走 10
    assert_eq!(local(&rt, p, &f), 10);

    rt.debug_write(
        p,
        f.pace_req,
        Value::map([("num", Value::Int(1)), ("den", Value::Int(1)), ("seq", Value::Int(2))]),
    );
    step_n(&mut rt, 10); // 复速只改导数（未来推进），不重写历史戳
    assert_eq!(local(&rt, p, &f), 20);
}

#[test]
fn iframes_judged_on_victims_axis_not_global_frames() {
    let (mut rt, body, attacker_ty, f) = setup();
    let victim = rt.spawn(body, vec![]);
    let attacker = rt.spawn(attacker_ty, vec![]);
    step_n(&mut rt, 2); // victim local = 2

    rt.debug_write(victim, f.dodge_req, Value::map([("seq", Value::Int(1))]));
    rt.step(); // invuln_until = 2 + 5 = 7（受击者本地轴）
    assert_eq!(as_i64(&rt.read(victim, f.invuln_until)), 7);

    // 冻结受击者：无敌窗随轴一起停摆
    rt.debug_write(
        victim,
        f.stop_req,
        Value::map([("frames", Value::Int(20)), ("seq", Value::Int(1))]),
    );
    step_n(&mut rt, 7); // 全局已过 7+ 帧——按全局轴无敌早该结束

    let hit = |seq: i64| {
        Value::map([
            ("target", Value::Ref(victim)),
            ("dmg", Value::Int(10)),
            ("seq", Value::Int(seq)),
        ])
    };
    rt.debug_write(attacker, f.attack_out, hit(1));
    rt.step(); // 受击者本地轴停在 3 < 7：守卫零触发挡掉
    assert_eq!(as_i64(&rt.read(victim, f.hp)), 100);

    // 解冻后本地轴走到 7：同样的攻击命中
    while local(&rt, victim, &f) < 7 {
        rt.step();
    }
    rt.debug_write(attacker, f.attack_out, hit(2));
    rt.step();
    assert_eq!(as_i64(&rt.read(victim, f.hp)), 90);
}
