//! 演示：PCE文档 §7 示例 1（血量跌穿 30% 边沿触发）+ 示例 2（攻击经数据流）
//! + 示例 4（Boss 血条增量聚合），全程使用纯库核心 API。
//!
//! 数据流：Attacker 写 own(attack_out) → Unit 的 take_damage 经
//! `type(Attacker, attack_out) where new.target = self` 嗅探 → 写 own(hp)
//! → flee 经 `own(hp) where crossed_down(0.3 * own.hp_max)` 边沿触发 → 写 own(state)
//! → boss_bar 经 `type(Unit, hp) fold sum` 增量维护总血量。
//!
//! 运行：cargo run --example demo

use pce::predicate::{own, own_field, type_scope};
use pce::{
    CalcOptions, CmpOp, Cond, Delivery, Detect, Dir, Expr, FieldDef, FoldOp, Predicate, Proj,
    RowPolicy, Runtime, Tier, ValRef, Value,
};

fn field(name: &str, default: impl Into<Value>) -> FieldDef {
    FieldDef::new(name, default.into())
}

fn val(v: impl Into<Value>) -> Expr {
    pce::predicate::lit(v.into())
}

fn new_path(path: &[&str]) -> Expr {
    Expr::Val(ValRef::New(path.iter().map(|s| s.to_string()).collect()))
}

fn new_proj() -> Proj {
    Proj::New(vec![])
}

fn old_proj() -> Proj {
    Proj::Old(vec![])
}

fn new_path_proj(path: &[&str]) -> Proj {
    Proj::New(path.iter().map(|s| s.to_string()).collect())
}

fn main() {
    let mut rt = Runtime::new();
    rt.set_detect(Detect::Warn);

    let unit = rt.register_entity_type_with(
        "Unit",
        vec![field("hp", 0), field("hp_max", 100), field("state", "idle")],
        false,
        RowPolicy::Compact,
    );
    let attacker = rt.register_entity_type("Attacker", vec![field("attack_out", ())], false);
    let bar = rt.register_entity_type("BossBar", vec![field("total_hp", 0.0)], true);

    let (f_hp, f_hp_max, f_state) = (
        rt.field(unit, "hp"),
        rt.field(unit, "hp_max"),
        rt.field(unit, "state"),
    );
    let f_attack_out = rt.field(attacker, "attack_out");
    let f_total = rt.field(bar, "total_hp");

    // §7 示例 2：攻击——跨实体交互的唯一通道：写自己，被对方嗅探。
    // `new.target = self` 在注册期识别为等值快路：按 ref 点查，无全实例扇出。
    rt.register_calculation_opt(
        "take_damage",
        unit,
        Predicate::new(
            type_scope(attacker, f_attack_out),
            Cond::Cmp(new_path(&["target"]), CmpOp::Eq, Expr::Val(ValRef::SelfRef)),
            Delivery::Each(vec![new_path_proj(&["dmg"])]),
        ),
        &[f_hp],
        CalcOptions {
            reads: Some(vec![f_hp]),
            tier: Tier::General,
            ..CalcOptions::default()
        },
        Box::new(move |ctx, input| {
            let hp = ctx.read_own(f_hp).as_i64().unwrap_or(0);
            let dmg = input.arg(0).as_i64().unwrap_or(0);
            ctx.write(f_hp, (hp - dmg).max(0));
        }),
    )
    .unwrap();

    // §7 示例 1：血量跌穿 30%——crossed 边沿触发，不会每帧重复。
    // 阈值引用 own 字段（活阈值），代价进 §4 诚实退化条款。
    rt.register_calculation(
        "flee",
        unit,
        Predicate::new(
            own(f_hp),
            Cond::Crossed(
                Expr::Mul(Box::new(own_field(f_hp_max)), Box::new(val(0.3))),
                Dir::Down,
            ),
            Delivery::Each(vec![new_proj(), old_proj()]),
        ),
        &[f_state],
        Box::new(move |ctx, input| {
            println!("  flee 触发：hp {:?} → {:?}", input.arg(1), input.arg(0));
            ctx.write(f_state, "fleeing");
        }),
    )
    .unwrap();

    // §7 示例 4：Boss 血条——fold sum 增量聚合，每写 ±delta。
    rt.register_calculation(
        "boss_bar",
        bar,
        Predicate::new(
            type_scope(unit, f_hp),
            Cond::True,
            Delivery::Fold(FoldOp::Sum),
        ),
        &[f_total],
        Box::new(move |ctx, input| ctx.write(f_total, input.agg().clone())),
    )
    .unwrap();

    let u = rt.spawn(unit, vec![(f_hp, Value::Int(100))]);
    let a = rt.spawn(attacker, vec![]);
    let bar0 = rt.alive(bar)[0];
    rt.step();

    // 连续攻击：每帧 Attacker 写 attack_out = {target, dmg}（D2 写即事件）
    for i in 1..=9 {
        rt.debug_write(
            a,
            f_attack_out,
            Value::map([("target", Value::Ref(u)), ("dmg", Value::Int(10))]),
        );
        rt.step(); // 路由 attack_out → take_damage 执行（写 hp 入帧缓冲）
        rt.step(); // hp 提交并路由 → flee 判定 / boss_bar 聚合
        println!(
            "第 {i} 击后: hp={:?} state={:?} total={:?}",
            rt.read(u, f_hp),
            rt.read(u, f_state),
            rt.read(bar0, f_total),
        );
    }

    // 免费 profiler（D2 送的遥测）：写频与触发计数
    let p = rt.profile();
    println!(
        "\nprofiler：帧数={} |W|={} |F|={} 热 cell（前 3）：",
        p.frames, p.last_writes, p.last_triggers
    );
    for ((ty, f), n) in p.hot_cells().into_iter().take(3) {
        println!("  type{}.field{} 写 {n} 次", ty.0, f.0);
    }
}
