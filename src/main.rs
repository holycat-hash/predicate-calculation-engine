//! 演示：PCE文档 §7 示例 1（血量跌穿 30% 边沿触发）+ 示例 2（攻击经数据流）。
//!
//! 数据流：Attacker 写 own(attack_out) → Unit 的 take_damage 经
//! `type(Attacker, attack_out) where new.target = self` 嗅探 → 写 own(hp)
//! → flee 经 `own(hp) where crossed(0.3 * own.hp_max, ↓)` 边沿触发 → 写 own(state)。

use pce::predicate::{lit, new_path, own, own_field, type_scope};
use pce::{CmpOp, Cond, Delivery, Dir, Expr, Input, Predicate, Proj, Runtime, ValRef, Value};

fn main() {
    let mut rt = Runtime::new();

    let unit = rt.register_entity_type(
        "Unit",
        vec![
            pce::FieldDef::new("hp", Value::Int(100)),
            pce::FieldDef::new("hp_max", Value::Int(100)),
            pce::FieldDef::new("state", Value::str("idle")),
        ],
        false,
    );
    let attacker = rt.register_entity_type(
        "Attacker",
        vec![pce::FieldDef::new("attack_out", Value::Null)],
        false,
    );

    let (f_hp, f_hp_max, f_state) =
        (rt.field(unit, "hp"), rt.field(unit, "hp_max"), rt.field(unit, "state"));
    let f_attack_out = rt.field(attacker, "attack_out");

    // 示例 2：攻击——跨实体交互的唯一通道：写自己，被对方嗅探
    rt.register_calculation(
        "take_damage_calc",
        unit,
        Predicate::new(
            type_scope(attacker, f_attack_out),
            Cond::Cmp(new_path(&["target"]), CmpOp::Eq, Expr::Val(ValRef::SelfRef)),
            // 单攻击者用 each 足够；多攻击者同帧命中属帧内聚合，应改 fold sum
            // （D3 推论一禁止 each 读-改-写累加），见 reference/04。
            Delivery::Each(vec![Proj::New(vec!["dmg".into()])]),
        ),
        &[f_hp],
        Box::new(move |ctx, input| {
            let Input::Each(row) = input else { return };
            let dmg = row[0].as_f64().unwrap_or(0.0);
            let hp = ctx.read_own(f_hp).as_f64().unwrap_or(0.0);
            ctx.write(f_hp, Value::Float(hp - dmg));
        }),
    )
    .unwrap();

    // 示例 1：血量跌穿 30%——边沿触发，不会每帧重复
    rt.register_calculation(
        "flee_calc",
        unit,
        Predicate::new(
            own(f_hp),
            Cond::Crossed(
                Expr::Mul(Box::new(lit(Value::Float(0.3))), Box::new(own_field(f_hp_max))),
                Dir::Down,
            ),
            Delivery::Each(vec![Proj::New(vec![]), Proj::Old(vec![])]),
        ),
        &[f_state],
        Box::new(move |ctx, input| {
            let Input::Each(row) = input else { return };
            println!("    flee_calc 触发：hp {:?} → {:?}", row[1], row[0]);
            ctx.write(f_state, Value::str("flee"));
        }),
    )
    .unwrap();

    let u = rt.spawn(unit, vec![]);
    let a = rt.spawn(attacker, vec![]);

    for round in 1..=3 {
        // 外部激励模拟攻击方 calculation 的写出
        rt.debug_write(
            a,
            f_attack_out,
            Value::map([("target", Value::Ref(u)), ("dmg", Value::Int(40))]),
        );
        rt.step(); // 路由 attack_out → take_damage 执行
        rt.step(); // 路由 hp 写 → crossed 判定
        println!(
            "第 {round} 轮后(帧 {}): hp={:?} state={:?}",
            rt.frame(),
            rt.read(u, f_hp),
            rt.read(u, f_state),
        );
    }
}
