//! 14 连击窗口 / 取消帧 / 输入缓冲（docs/14-combo-cancel-buffer.md）的可运行验证：
//! 意图寄存批内 max(seq)（D3 合规）、动作驱动器已付轮询统一双边沿、
//! consumed_seq 单调恰好一次、动作重启原子、缓冲期限过期退役。

use pce::predicate::{own, type_scope};
use pce::{
    Cond, Delivery, EntityTypeId, FieldDef, FieldId, Input, InstanceId, Predicate, Proj, Runtime,
    Value,
};

/// 缓冲期限（全局帧）：太早的输入不得在窗口开启时复活。
const BUFFER_LEN: i64 = 10;
/// idle 的「常开窗口」端点。
const OPEN: i64 = 1 << 40;

/// 动作表：button → (总时长, 取消窗口起, 取消窗口止)。
fn move_data(button: &str) -> (i64, i64, i64) {
    match button {
        "heavy" => (30, 12, 20),
        _ => (24, 8, 14), // light
    }
}

fn path(v: &Value, key: &str) -> Value {
    v.get_path(&[key.to_string()])
}

fn as_i64(v: &Value) -> i64 {
    v.as_f64().unwrap_or(0.0) as i64
}

fn as_str(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        _ => String::new(),
    }
}

#[derive(Clone, Copy)]
struct F {
    input_req: FieldId,
    intent: FieldId,
    action_id: FieldId,
    action_frame: FieldId,
    cancel_from: FieldId,
    cancel_to: FieldId,
    duration: FieldId,
    combo_count: FieldId,
    consumed_seq: FieldId,
}

fn setup() -> (Runtime, EntityTypeId, F) {
    let mut rt = Runtime::new();
    let fighter = rt.register_entity_type(
        "Fighter",
        vec![
            FieldDef::new("input_req", Value::Null),
            FieldDef::new("intent", Value::Null),
            FieldDef::new("action_id", Value::str("idle")),
            FieldDef::new("action_frame", Value::Int(0)),
            FieldDef::new("cancel_from", Value::Int(0)),
            FieldDef::new("cancel_to", Value::Int(OPEN)),
            FieldDef::new("duration", Value::Int(OPEN)),
            FieldDef::new("combo_count", Value::Int(0)),
            FieldDef::new("consumed_seq", Value::Int(0)),
        ],
        false,
    );
    let f = F {
        input_req: rt.field(fighter, "input_req"),
        intent: rt.field(fighter, "intent"),
        action_id: rt.field(fighter, "action_id"),
        action_frame: rt.field(fighter, "action_frame"),
        cancel_from: rt.field(fighter, "cancel_from"),
        cancel_to: rt.field(fighter, "cancel_to"),
        duration: rt.field(fighter, "duration"),
        combo_count: rt.field(fighter, "combo_count"),
        consumed_seq: rt.field(fighter, "consumed_seq"),
    };

    // 1 意图寄存：异源输入塌缩为单调 seq 的水位 cell；批内取 max(seq)（D3：到达序不可用）
    let intent_f = f.intent;
    rt.register_calculation(
        "intent_register",
        fighter,
        Predicate::new(own(f.input_req), Cond::True, Delivery::Batch(vec![Proj::New(vec![])])),
        &[intent_f],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let cur = as_i64(&path(&ctx.read_own(intent_f), "seq"));
            let best = rows
                .iter()
                .map(|r| &r[0])
                .filter(|v| as_i64(&path(v, "seq")) > cur) // 寄存值只升不降（覆盖语义）
                .max_by_key(|v| as_i64(&path(v, "seq")));
            if let Some(v) = best {
                ctx.write(intent_f, v.clone());
            }
        }),
    )
    .unwrap();

    // 2 动作驱动器：已付的每帧轮询（动画帧必须推进）；
    //   推进、判窗、消费、动作重启在同一次运行内原子提交（§2 一个写集）
    let clock_ty = rt.clock().ty;
    let clock_frame = rt.clock().f_frame;
    let (f_action, f_af) = (f.action_id, f.action_frame);
    let (f_from, f_to, f_dur) = (f.cancel_from, f.cancel_to, f.duration);
    let (f_combo, f_consumed) = (f.combo_count, f.consumed_seq);
    rt.register_calculation(
        "action_drive",
        fighter,
        Predicate::new(
            type_scope(clock_ty, clock_frame),
            Cond::True,
            Delivery::Each(vec![Proj::New(vec![])]),
        ),
        &[f_action, f_af, f_from, f_to, f_dur, f_combo, f_consumed],
        Box::new(move |ctx, input| {
            let Input::Each(row) = input else { return };
            let now = as_i64(&row[0]);
            let mut action = as_str(&ctx.read_own(f_action));
            let mut af = as_i64(&ctx.read_own(f_af)) + 1;
            let mut from = as_i64(&ctx.read_own(f_from));
            let mut to = as_i64(&ctx.read_own(f_to));
            let mut dur = as_i64(&ctx.read_own(f_dur));
            let mut combo = as_i64(&ctx.read_own(f_combo));
            let mut consumed = as_i64(&ctx.read_own(f_consumed));

            // 动作自然结束 → idle（常开窗口），缓存的输入在此获得下一次机会
            if action != "idle" && af > dur {
                action = "idle".to_string();
                af = 0;
                from = 0;
                to = OPEN;
                dur = OPEN;
            }
            // 采样意图：恰好一次 = consumed_seq 单调一行不等式；过期退役走同一通道
            let intent = ctx.read_own(intent_f);
            let seq = as_i64(&path(&intent, "seq"));
            if seq > consumed {
                if now - as_i64(&path(&intent, "frame")) > BUFFER_LEN {
                    consumed = seq; // 过期：作废但不出招，不会日后复活
                } else if af >= from && af <= to {
                    let button = as_str(&path(&intent, "button"));
                    let (d, wf, wt) = move_data(&button);
                    action = button;
                    af = 0;
                    from = wf;
                    to = wt;
                    dur = d;
                    combo += 1;
                    consumed = seq;
                }
            }
            ctx.write(f_action, Value::Str(action));
            ctx.write(f_af, Value::Int(af));
            ctx.write(f_from, Value::Int(from));
            ctx.write(f_to, Value::Int(to));
            ctx.write(f_dur, Value::Int(dur));
            ctx.write(f_combo, Value::Int(combo));
            ctx.write(f_consumed, Value::Int(consumed));
        }),
    )
    .unwrap();

    (rt, fighter, f)
}

/// 外部输入层：writer 侧盖帧戳（手法 4），seq 由输入系统单调分配。
fn press(rt: &mut Runtime, actor: InstanceId, f: &F, button: &str, seq: i64) {
    let frame = rt.frame() as i64;
    rt.debug_write(
        actor,
        f.input_req,
        Value::map([
            ("button", Value::str(button)),
            ("seq", Value::Int(seq)),
            ("frame", Value::Int(frame)),
        ]),
    );
}

fn combo(rt: &Runtime, actor: InstanceId, f: &F) -> i64 {
    as_i64(&rt.read(actor, f.combo_count))
}

fn af(rt: &Runtime, actor: InstanceId, f: &F) -> i64 {
    as_i64(&rt.read(actor, f.action_frame))
}

fn action(rt: &Runtime, actor: InstanceId, f: &F) -> String {
    as_str(&rt.read(actor, f.action_id))
}

fn step_n(rt: &mut Runtime, n: usize) {
    for _ in 0..n {
        rt.step();
    }
}

#[test]
fn buffered_input_fires_exactly_at_window_open_edge() {
    let (mut rt, fighter, f) = setup();
    let actor = rt.spawn(fighter, vec![]);

    // idle 常开窗：输入两帧后被消费（寄存一帧 + 驱动器一帧）
    press(&mut rt, actor, &f, "light", 1);
    step_n(&mut rt, 2);
    assert_eq!(combo(&rt, actor, &f), 1);
    assert_eq!(action(&rt, actor, &f), "light");

    // light 窗口 [8,14] 开启前缓存 heavy：af 走到 8 的那一帧才消费，早一帧都不行
    press(&mut rt, actor, &f, "heavy", 2);
    step_n(&mut rt, 7);
    assert_eq!(af(&rt, actor, &f), 7);
    assert_eq!(combo(&rt, actor, &f), 1);
    rt.step(); // af 跨进 8：开窗即消费，动作重启原子（af 归零、窗口换表）
    assert_eq!(combo(&rt, actor, &f), 2);
    assert_eq!(action(&rt, actor, &f), "heavy");
    assert_eq!(af(&rt, actor, &f), 0);
}

#[test]
fn input_inside_open_window_consumed_promptly_and_exactly_once() {
    let (mut rt, fighter, f) = setup();
    let actor = rt.spawn(fighter, vec![]);
    press(&mut rt, actor, &f, "light", 1);
    step_n(&mut rt, 2); // light 起手
    step_n(&mut rt, 9); // af = 9，已在窗内 [8,14]
    assert_eq!(af(&rt, actor, &f), 9);

    press(&mut rt, actor, &f, "heavy", 2);
    rt.step(); // 寄存帧：意图本帧尚不可见（快照读）
    assert_eq!(combo(&rt, actor, &f), 1);
    rt.step(); // 驱动器消费
    assert_eq!(combo(&rt, actor, &f), 2);
    assert_eq!(action(&rt, actor, &f), "heavy");

    // 恰好一次：heavy 整个窗口 [12,20] 乃至收招回 idle，陈旧意图（seq 已退役）不再触发
    step_n(&mut rt, 40);
    assert_eq!(combo(&rt, actor, &f), 2);
    assert_eq!(as_i64(&rt.read(actor, f.consumed_seq)), 2);
}

#[test]
fn input_after_window_waits_for_recovery_end() {
    let (mut rt, fighter, f) = setup();
    let actor = rt.spawn(fighter, vec![]);
    press(&mut rt, actor, &f, "light", 1);
    step_n(&mut rt, 2); // light 起手：窗 [8,14]，时长 24
    step_n(&mut rt, 16); // af = 16，窗口已关
    assert_eq!(af(&rt, actor, &f), 16);

    press(&mut rt, actor, &f, "heavy", 2);
    step_n(&mut rt, 8); // af 走到 24 = duration：动作未结束，窗外不消费
    assert_eq!(combo(&rt, actor, &f), 1);
    rt.step(); // af 越过 duration → idle 常开窗，同一次运行内消费（期限内）
    assert_eq!(combo(&rt, actor, &f), 2);
    assert_eq!(action(&rt, actor, &f), "heavy");
}

#[test]
fn stale_buffered_input_expires_instead_of_popping_out() {
    let (mut rt, fighter, f) = setup();
    let actor = rt.spawn(fighter, vec![]);
    press(&mut rt, actor, &f, "heavy", 1);
    step_n(&mut rt, 2); // heavy 起手：窗 [12,20]，离开窗 > BUFFER_LEN
    press(&mut rt, actor, &f, "light", 2);

    // 窗口开启前意图已过期退役：整个窗口走完都不出招
    step_n(&mut rt, 30);
    assert_eq!(combo(&rt, actor, &f), 1);
    assert_eq!(as_i64(&rt.read(actor, f.consumed_seq)), 2); // 退役而非滞留

    // 退役不污染后续：新输入照常消费
    press(&mut rt, actor, &f, "light", 3);
    step_n(&mut rt, 3);
    assert_eq!(combo(&rt, actor, &f), 2);
    assert_eq!(action(&rt, actor, &f), "light");
}

#[test]
fn same_frame_inputs_resolve_by_seq_not_arrival_order() {
    let (mut rt, fighter, f) = setup();
    let a1 = rt.spawn(fighter, vec![]);
    let a2 = rt.spawn(fighter, vec![]);

    // 同帧双键，两种写入次序：结局必须同为 max(seq) 的 heavy（D3：消费是多重集的确定函数）
    press(&mut rt, a1, &f, "light", 5);
    press(&mut rt, a1, &f, "heavy", 6);
    press(&mut rt, a2, &f, "heavy", 6);
    press(&mut rt, a2, &f, "light", 5);
    step_n(&mut rt, 2);
    for actor in [a1, a2] {
        assert_eq!(combo(&rt, actor, &f), 1);
        assert_eq!(action(&rt, actor, &f), "heavy");
        assert_eq!(as_i64(&rt.read(actor, f.consumed_seq)), 6);
    }
}
