//! Executable checks for docs/07-global-order-topk.md.

use std::collections::BTreeMap;

use pce::predicate::type_scope;
use pce::{
    Cond, Delivery, EntityTypeId, FieldDef, FieldId, Input, InstanceId, Predicate, Proj, Runtime,
    Value,
};

const TOP_K: usize = 3;

#[derive(Clone, Copy)]
struct W {
    player_ty: EntityTypeId,
    board_ty: EntityTypeId,
    hud_ty: EntityTypeId,
    p_score: FieldId,
    b_rank_state: FieldId,
    b_top: FieldId,
    h_refresh_count: FieldId,
    h_last_top: FieldId,
}

fn path(v: &Value, key: &str) -> Value {
    v.get_path(&[key.to_string()])
}

fn as_i64(v: &Value) -> i64 {
    v.as_f64().unwrap_or(0.0) as i64
}

fn map_of(v: &Value) -> BTreeMap<String, Value> {
    match v {
        Value::Map(m) => m.clone(),
        _ => BTreeMap::new(),
    }
}

fn setup() -> (Runtime, W) {
    let mut rt = Runtime::new();
    let player_ty =
        rt.register_entity_type("Player", vec![FieldDef::new("score", Value::Int(0))], false);
    let board_ty = rt.register_entity_type(
        "Board",
        vec![
            FieldDef::new("rank_state", Value::Map(BTreeMap::new())),
            FieldDef::new("top10", Value::Map(BTreeMap::new())),
        ],
        false,
    );
    let hud_ty = rt.register_entity_type(
        "Hud",
        vec![
            FieldDef::new("refresh_count", Value::Int(0)),
            FieldDef::new("last_top", Value::Null),
        ],
        false,
    );
    let w = W {
        player_ty,
        board_ty,
        hud_ty,
        p_score: rt.field(player_ty, "score"),
        b_rank_state: rt.field(board_ty, "rank_state"),
        b_top: rt.field(board_ty, "top10"),
        h_refresh_count: rt.field(hud_ty, "refresh_count"),
        h_last_top: rt.field(hud_ty, "last_top"),
    };

    let rank_state = w.b_rank_state;
    let top = w.b_top;
    rt.register_calculation(
        "rank",
        board_ty,
        Predicate::new(
            type_scope(player_ty, w.p_score),
            Cond::True,
            Delivery::Batch(vec![Proj::WriterId, Proj::New(vec![]), Proj::Old(vec![])]),
        ),
        &[rank_state, top],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let mut ranks = map_of(&ctx.read_own(rank_state));
            for row in rows {
                let Some(player) = row[0].as_ref_id() else {
                    continue;
                };
                ranks.insert(
                    player.id.to_string(),
                    Value::map([("player", Value::Ref(player)), ("score", row[1].clone())]),
                );
            }

            let mut entries: Vec<Value> = ranks.values().cloned().collect();
            entries.sort_by(|a, b| {
                let sa = as_i64(&path(a, "score"));
                let sb = as_i64(&path(b, "score"));
                let ia = path(a, "player").as_ref_id().map(|p| p.id).unwrap_or(0);
                let ib = path(b, "player").as_ref_id().map(|p| p.id).unwrap_or(0);
                sb.cmp(&sa).then_with(|| ia.cmp(&ib))
            });

            let top_map = entries
                .into_iter()
                .take(TOP_K)
                .enumerate()
                .map(|(i, entry)| ((i + 1).to_string(), entry))
                .collect();
            ctx.write(rank_state, Value::Map(ranks));
            ctx.write(top, Value::Map(top_map));
        }),
    )
    .unwrap();

    let refresh_count = w.h_refresh_count;
    let last_top = w.h_last_top;
    rt.register_calculation(
        "hud_refresh",
        hud_ty,
        Predicate::new(
            type_scope(board_ty, w.b_top),
            Cond::Changed,
            Delivery::Each(vec![Proj::New(vec![])]),
        ),
        &[refresh_count, last_top],
        Box::new(move |ctx, input| {
            let Input::Each(row) = input else { return };
            let count = as_i64(&ctx.read_own(refresh_count));
            ctx.write(refresh_count, Value::Int(count + 1));
            ctx.write(last_top, row[0].clone());
        }),
    )
    .unwrap();

    (rt, w)
}

fn top_players(rt: &Runtime, board: InstanceId, w: W) -> Vec<InstanceId> {
    let top = map_of(&rt.read(board, w.b_top));
    (1..=top.len())
        .map(|i| {
            let entry = top.get(&i.to_string()).unwrap();
            path(entry, "player").as_ref_id().unwrap()
        })
        .collect()
}

#[test]
fn board_materializes_topk_and_hud_refreshes_only_on_view_change() {
    let (mut rt, w) = setup();
    let board = rt.spawn(w.board_ty, vec![]);
    let hud = rt.spawn(w.hud_ty, vec![]);
    let p1 = rt.spawn(w.player_ty, vec![(w.p_score, Value::Int(50))]);
    let p2 = rt.spawn(w.player_ty, vec![(w.p_score, Value::Int(80))]);
    let p3 = rt.spawn(w.player_ty, vec![(w.p_score, Value::Int(30))]);
    let p4 = rt.spawn(w.player_ty, vec![(w.p_score, Value::Int(70))]);

    rt.step();
    assert_eq!(top_players(&rt, board, w), vec![p2, p4, p1]);
    rt.step();
    assert_eq!(as_i64(&rt.read(hud, w.h_refresh_count)), 1);
    assert_eq!(rt.read(hud, w.h_last_top), rt.read(board, w.b_top));

    rt.debug_write(p3, w.p_score, Value::Int(40));
    rt.step();
    assert_eq!(top_players(&rt, board, w), vec![p2, p4, p1]);
    rt.step();
    assert_eq!(as_i64(&rt.read(hud, w.h_refresh_count)), 1);

    rt.debug_write(p3, w.p_score, Value::Int(90));
    rt.debug_write(p1, w.p_score, Value::Int(20));
    rt.step();
    assert_eq!(top_players(&rt, board, w), vec![p3, p2, p4]);
    rt.step();
    assert_eq!(as_i64(&rt.read(hud, w.h_refresh_count)), 2);
    assert_eq!(rt.read(hud, w.h_last_top), rt.read(board, w.b_top));
}
