//! 16 经验/掉落同帧分配（docs/16-xp-loot-split.md）的可运行验证：
//! 整除分账余数守恒、轮替指针批内推进、掷点窗口仲裁与迟到 roll 守卫。

use std::collections::BTreeMap;

use pce::predicate::{lit, new_path, own, type_scope};
use pce::{
    CmpOp, Cond, Delivery, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, ValRef, Value,
};

fn target_is_self(path: &[&str]) -> Cond {
    Cond::Cmp(new_path(path), CmpOp::Eq, Expr::Val(ValRef::SelfRef))
}

fn and(a: Cond, b: Cond) -> Cond {
    Cond::And(Box::new(a), Box::new(b))
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

fn map_of(v: &Value) -> BTreeMap<String, Value> {
    match v {
        Value::Map(m) => m.clone(),
        _ => BTreeMap::new(),
    }
}

#[derive(Clone, Copy)]
struct W {
    member_ty: EntityTypeId,
    party_ty: EntityTypeId,
    corpse_ty: EntityTypeId,
    loot_ty: EntityTypeId,
    m_xp: FieldId,
    m_bag: FieldId,
    m_roll_out: FieldId,
    p_roster: FieldId,
    p_rr_cursor: FieldId,
    p_kill_in: FieldId,
    c_drop_out: FieldId,
    l_rolls: FieldId,
    l_closed: FieldId,
    l_winner: FieldId,
}

fn setup() -> (Runtime, W) {
    let mut rt = Runtime::new();
    let member_ty = rt.register_entity_type(
        "Member",
        vec![
            FieldDef::new("xp", Value::Int(0)),
            FieldDef::new("bag", Value::Map(BTreeMap::new())),
            FieldDef::new("roll_out", Value::Null),
        ],
        false,
    );
    let party_ty = rt.register_entity_type(
        "Party",
        vec![
            FieldDef::new("roster", Value::Map(BTreeMap::new())),
            FieldDef::new("rr_cursor", Value::Int(0)),
            FieldDef::new("kill_in", Value::Null),
        ],
        false,
    );
    let corpse_ty = rt.register_entity_type(
        "Corpse",
        vec![FieldDef::new("drop_out", Value::Null)],
        false,
    );
    let loot_ty = rt.register_entity_type(
        "Loot",
        vec![
            FieldDef::new("rolls", Value::Map(BTreeMap::new())),
            FieldDef::new("closed", Value::Bool(false)),
            FieldDef::new("winner", Value::Null),
        ],
        false,
    );
    // 事件实体（06）：出生即广播，一帧自决
    let award_ty =
        rt.register_entity_type("Award", vec![FieldDef::new("grant", Value::Null)], false);
    let grant_ty =
        rt.register_entity_type("Grant", vec![FieldDef::new("give", Value::Null)], false);

    let w = W {
        member_ty,
        party_ty,
        corpse_ty,
        loot_ty,
        m_xp: rt.field(member_ty, "xp"),
        m_bag: rt.field(member_ty, "bag"),
        m_roll_out: rt.field(member_ty, "roll_out"),
        p_roster: rt.field(party_ty, "roster"),
        p_rr_cursor: rt.field(party_ty, "rr_cursor"),
        p_kill_in: rt.field(party_ty, "kill_in"),
        c_drop_out: rt.field(corpse_ty, "drop_out"),
        l_rolls: rt.field(loot_ty, "rolls"),
        l_closed: rt.field(loot_ty, "closed"),
        l_winner: rt.field(loot_ty, "winner"),
    };
    let award_grant = rt.field(award_ty, "grant");
    let grant_give = rt.field(grant_ty, "give");

    // 1 整除分账：total = Σxp；按 slot 升序前 rem 名 +1；逐成员 spawn Award
    let roster_f = w.p_roster;
    rt.register_calculation(
        "xp_split",
        party_ty,
        Predicate::new(
            own(w.p_kill_in),
            Cond::True,
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let total: i64 = rows.iter().map(|r| as_i64(&path(&r[0], "xp"))).sum();
            let roster = map_of(&ctx.read_own(roster_f));
            let n = roster.len() as i64;
            if n == 0 || total <= 0 {
                return;
            }
            let (per, rem) = (total / n, total % n);
            for (i, target) in roster.values().enumerate() {
                let amount = per + if (i as i64) < rem { 1 } else { 0 };
                ctx.spawn(
                    award_ty,
                    vec![(
                        award_grant,
                        Value::map([("target", target.clone()), ("amount", Value::Int(amount))]),
                    )],
                );
            }
        }),
    )
    .unwrap();

    // 2 领取：batch 求和（03 纪律），事件实体出生写 = 恰好一次
    let m_xp = w.m_xp;
    rt.register_calculation(
        "xp_recv",
        member_ty,
        Predicate::new(
            type_scope(award_ty, award_grant),
            target_is_self(&["target"]),
            Delivery::Batch(vec![Proj::New(vec!["amount".to_string()])]),
        ),
        &[m_xp],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let sum: i64 = rows.iter().map(|r| as_i64(&r[0])).sum();
            let xp = as_i64(&ctx.read_own(m_xp));
            ctx.write(m_xp, Value::Int(xp + sum));
        }),
    )
    .unwrap();

    // 5 轮替分派：同帧 k 件批内按 salt 全序逐件推进，cursor 一帧一写
    let rr_cursor = w.p_rr_cursor;
    rt.register_calculation(
        "rr_assign",
        party_ty,
        Predicate::new(
            type_scope(corpse_ty, w.c_drop_out),
            target_is_self(&["party"]),
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[rr_cursor],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let mut drops: Vec<Value> = rows.iter().map(|r| r[0].clone()).collect();
            drops.sort_by_key(|v| as_str(&path(v, "salt")));
            let roster = map_of(&ctx.read_own(roster_f));
            let n = roster.len();
            if n == 0 || drops.is_empty() {
                return;
            }
            let cursor = as_i64(&ctx.read_own(rr_cursor));
            for (i, drop) in drops.iter().enumerate() {
                let idx = ((cursor as usize) + i) % n;
                let target = roster.values().nth(idx).unwrap().clone();
                ctx.spawn(
                    grant_ty,
                    vec![(
                        grant_give,
                        Value::map([("target", target), ("item", path(drop, "item"))]),
                    )],
                );
            }
            ctx.write(rr_cursor, Value::Int(cursor + drops.len() as i64));
        }),
    )
    .unwrap();

    // 领取掉落：bag 键控合并（多重集函数）
    let m_bag = w.m_bag;
    rt.register_calculation(
        "item_recv",
        member_ty,
        Predicate::new(
            type_scope(grant_ty, grant_give),
            target_is_self(&["target"]),
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[m_bag],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let mut bag = map_of(&ctx.read_own(m_bag));
            for row in rows {
                if let Value::Str(item) = path(&row[0], "item") {
                    bag.insert(item, Value::Int(1));
                }
            }
            ctx.write(m_bag, Value::Map(bag));
        }),
    )
    .unwrap();

    // 3 掷点收集：own.closed 活阈值守卫，迟到 roll 静默拒绝（手法 8）
    let l_rolls = w.l_rolls;
    rt.register_calculation(
        "roll_collect",
        loot_ty,
        Predicate::new(
            type_scope(member_ty, w.m_roll_out),
            and(
                target_is_self(&["loot"]),
                Cond::Cmp(
                    Expr::Val(ValRef::Own(w.l_closed)),
                    CmpOp::Eq,
                    lit(Value::Bool(false)),
                ),
            ),
            Delivery::Batch(vec![Proj::New(vec![])]),
        ),
        &[l_rolls],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let mut rolls = map_of(&ctx.read_own(l_rolls));
            for row in rows {
                rolls.insert(as_str(&path(&row[0], "salt")), row[0].clone());
            }
            ctx.write(l_rolls, Value::Map(rolls));
        }),
    )
    .unwrap();

    // 4 关窗与开奖：alarm 到点；winner = max by (roll, salt)，多重集函数
    let (l_closed, l_winner) = (w.l_closed, w.l_winner);
    let clock_ty = rt.clock().ty;
    let clock_alarm = rt.clock().f_alarm;
    rt.register_calculation(
        "roll_award",
        loot_ty,
        Predicate::new(
            type_scope(clock_ty, clock_alarm),
            target_is_self(&["loot"]),
            Delivery::Each(vec![]),
        ),
        &[l_closed, l_winner],
        Box::new(move |ctx, _| {
            let rolls = map_of(&ctx.read_own(l_rolls));
            let mut best: Option<(i64, String, Value)> = None;
            for (salt, entry) in rolls {
                let key = (as_i64(&path(&entry, "roll")), salt);
                if best
                    .as_ref()
                    .is_none_or(|(r, s, _)| (key.0, &key.1) > (*r, s))
                {
                    best = Some((key.0, key.1, path(&entry, "member")));
                }
            }
            ctx.write(l_closed, Value::Bool(true));
            if let Some((_, _, member)) = best {
                ctx.write(l_winner, member);
            }
        }),
    )
    .unwrap();

    // 事件实体一帧自决（06）
    rt.register_calculation(
        "award_die",
        award_ty,
        Predicate::new(own(award_grant), Cond::True, Delivery::Each(vec![])),
        &[],
        Box::new(|ctx, _| ctx.destroy_self()),
    )
    .unwrap();
    rt.register_calculation(
        "grant_die",
        grant_ty,
        Predicate::new(own(grant_give), Cond::True, Delivery::Each(vec![])),
        &[],
        Box::new(|ctx, _| ctx.destroy_self()),
    )
    .unwrap();

    (rt, w)
}

fn spawn_party(rt: &mut Runtime, w: W) -> (InstanceId, Vec<InstanceId>) {
    let members: Vec<InstanceId> = (0..3).map(|_| rt.spawn(w.member_ty, vec![])).collect();
    let roster = Value::Map(
        members
            .iter()
            .enumerate()
            .map(|(i, m)| (i.to_string(), Value::Ref(*m)))
            .collect(),
    );
    let party = rt.spawn(w.party_ty, vec![(w.p_roster, roster)]);
    (party, members)
}

#[test]
fn integer_split_conserves_total_with_remainder() {
    let (mut rt, w) = setup();
    let (party, members) = spawn_party(&mut rt, w);

    rt.debug_write(party, w.p_kill_in, Value::map([("xp", Value::Int(100))]));
    rt.step(); // 分账 + spawn Award
    rt.step(); // 成员领取

    let xs: Vec<i64> = members
        .iter()
        .map(|m| as_i64(&rt.read(*m, w.m_xp)))
        .collect();
    // 100 = 33×3 + 1：余数按 slot 全序落给第一名，不丢不复制
    assert_eq!(xs, vec![34, 33, 33]);
    assert_eq!(xs.iter().sum::<i64>(), 100);
}

#[test]
fn round_robin_cursor_advances_k_slots_in_one_batch() {
    let (mut rt, w) = setup();
    let (party, members) = spawn_party(&mut rt, w);
    let c1 = rt.spawn(w.corpse_ty, vec![]);
    let c2 = rt.spawn(w.corpse_ty, vec![]);

    // 同帧两件掉落：一次 batch 内按 salt 全序逐件分派，指针一写推两格
    rt.debug_write(
        c1,
        w.c_drop_out,
        Value::map([
            ("party", Value::Ref(party)),
            ("item", Value::str("sword")),
            ("salt", Value::str("a")),
        ]),
    );
    rt.debug_write(
        c2,
        w.c_drop_out,
        Value::map([
            ("party", Value::Ref(party)),
            ("item", Value::str("shield")),
            ("salt", Value::str("b")),
        ]),
    );
    rt.step();
    rt.step();

    assert!(map_of(&rt.read(members[0], w.m_bag)).contains_key("sword"));
    assert!(map_of(&rt.read(members[1], w.m_bag)).contains_key("shield"));
    assert!(map_of(&rt.read(members[2], w.m_bag)).is_empty());
    assert_eq!(as_i64(&rt.read(party, w.p_rr_cursor)), 2);

    // 下一件从推进后的指针继续：归第三名，指针回绕
    rt.debug_write(
        c1,
        w.c_drop_out,
        Value::map([
            ("party", Value::Ref(party)),
            ("item", Value::str("potion")),
            ("salt", Value::str("c")),
        ]),
    );
    rt.step();
    rt.step();
    assert!(map_of(&rt.read(members[2], w.m_bag)).contains_key("potion"));
    assert_eq!(as_i64(&rt.read(party, w.p_rr_cursor)), 3);
}

#[test]
fn roll_window_arbitrates_and_rejects_late_rolls() {
    let (mut rt, w) = setup();
    let (_party, members) = spawn_party(&mut rt, w);
    let loot = rt.spawn(w.loot_ty, vec![]);
    rt.set_alarm(6, Value::map([("loot", Value::Ref(loot))]));

    let roll = |m: InstanceId, r: i64, salt: &str| {
        Value::map([
            ("loot", Value::Ref(loot)),
            ("member", Value::Ref(m)),
            ("roll", Value::Int(r)),
            ("salt", Value::str(salt)),
        ])
    };
    rt.debug_write(members[0], w.m_roll_out, roll(members[0], 55, "a"));
    rt.step(); // 帧 1：收 a
    rt.debug_write(members[1], w.m_roll_out, roll(members[1], 80, "b"));
    rt.step(); // 帧 2：收 b
    while rt.frame() < 5 {
        rt.step();
    }
    // 与 alarm 同帧抵达的 roll 不在开奖快照里：截止边界明确
    rt.debug_write(members[2], w.m_roll_out, roll(members[2], 99, "c"));
    rt.step(); // 帧 6：alarm 开奖（快照 = {a, b}），c 同帧记账但不参与

    assert_eq!(rt.read(loot, w.l_closed), Value::Bool(true));
    assert_eq!(rt.read(loot, w.l_winner), Value::Ref(members[1]));

    // 关窗后的迟到 roll 被 own.closed 守卫静默拒绝
    rt.debug_write(members[2], w.m_roll_out, roll(members[2], 100, "d"));
    rt.step();
    let rolls = map_of(&rt.read(loot, w.l_rolls));
    assert!(rolls.contains_key("c"));
    assert!(!rolls.contains_key("d"));
    assert_eq!(rt.read(loot, w.l_winner), Value::Ref(members[1]));
}
