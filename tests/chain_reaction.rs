//! Executable checks for docs/08-chain-reaction.md.

use pce::predicate::{inst, lit, new_path, own, type_scope};
use pce::{
    CmpOp, Cond, Delivery, Dir, EntityTypeId, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, Value,
};

const DMG: i64 = 50;

#[derive(Clone, Copy)]
struct W {
    barrel_ty: EntityTypeId,
    cell_ty: EntityTypeId,
    b_hp: FieldId,
    b_my_cell: FieldId,
    b_explosion_out: FieldId,
    c_splash: FieldId,
    c_seq: FieldId,
}

fn as_i64(v: &Value) -> i64 {
    v.as_f64().unwrap_or(0.0) as i64
}

fn path(v: &Value, key: &str) -> Value {
    v.get_path(&[key.to_string()])
}

fn setup() -> (Runtime, W) {
    let mut rt = Runtime::new();
    let cell_ty = rt.register_entity_type(
        "Cell",
        vec![
            FieldDef::new("splash", Value::Null),
            FieldDef::new("seq", Value::Int(0)),
        ],
        false,
    );
    let barrel_ty = rt.register_entity_type(
        "Barrel",
        vec![
            FieldDef::new("hp", Value::Int(0)),
            FieldDef::reference("my_cell"),
            FieldDef::new("explosion_out", Value::Null),
        ],
        false,
    );
    let w = W {
        barrel_ty,
        cell_ty,
        b_hp: rt.field(barrel_ty, "hp"),
        b_my_cell: rt.field(barrel_ty, "my_cell"),
        b_explosion_out: rt.field(barrel_ty, "explosion_out"),
        c_splash: rt.field(cell_ty, "splash"),
        c_seq: rt.field(cell_ty, "seq"),
    };

    let explosion_out = w.b_explosion_out;
    let my_cell = w.b_my_cell;
    rt.register_calculation(
        "explode",
        barrel_ty,
        Predicate::new(
            own(w.b_hp),
            Cond::Crossed(lit(Value::Int(0)), Dir::Down),
            Delivery::Each(vec![]),
        ),
        &[explosion_out],
        Box::new(move |ctx, _| {
            ctx.write(
                explosion_out,
                Value::map([("cell", ctx.read_own(my_cell)), ("dmg", Value::Int(DMG))]),
            );
            ctx.destroy_self();
        }),
    )
    .unwrap();

    let splash = w.c_splash;
    let seq = w.c_seq;
    rt.register_calculation(
        "splash",
        cell_ty,
        Predicate::new(
            type_scope(barrel_ty, w.b_explosion_out),
            Cond::Cmp(
                new_path(&["cell"]),
                CmpOp::Eq,
                pce::Expr::Val(pce::ValRef::SelfRef),
            ),
            Delivery::Batch(vec![Proj::New(vec!["dmg".to_string()])]),
        ),
        &[splash, seq],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let total: i64 = rows.iter().map(|row| as_i64(&row[0])).sum();
            let next_seq = as_i64(&ctx.read_own(seq)) + 1;
            ctx.write(
                splash,
                Value::map([("dmg", Value::Int(total)), ("seq", Value::Int(next_seq))]),
            );
            ctx.write(seq, Value::Int(next_seq));
        }),
    )
    .unwrap();

    let hp = w.b_hp;
    rt.register_calculation(
        "take_splash",
        barrel_ty,
        Predicate::new(
            inst(w.b_my_cell, w.c_splash),
            Cond::True,
            Delivery::Each(vec![Proj::New(vec!["dmg".to_string()])]),
        ),
        &[hp],
        Box::new(move |ctx, input| {
            let Input::Each(row) = input else { return };
            let hp_now = as_i64(&ctx.read_own(hp));
            ctx.write(hp, Value::Int(hp_now - as_i64(&row[0])));
        }),
    )
    .unwrap();

    (rt, w)
}

fn spawn_barrel(rt: &mut Runtime, w: W, cell: InstanceId, hp: i64) -> InstanceId {
    rt.spawn(
        w.barrel_ty,
        vec![(w.b_hp, Value::Int(hp)), (w.b_my_cell, Value::Ref(cell))],
    )
}

fn splash_seq(rt: &Runtime, cell: InstanceId, w: W) -> i64 {
    as_i64(&path(&rt.read(cell, w.c_splash), "seq"))
}

#[test]
fn explosion_chain_expands_one_frame_per_hop_and_stops() {
    let (mut rt, w) = setup();
    let cell = rt.spawn(w.cell_ty, vec![]);
    let a = spawn_barrel(&mut rt, w, cell, 10);
    let b = spawn_barrel(&mut rt, w, cell, 40);
    let c = spawn_barrel(&mut rt, w, cell, 100);

    rt.debug_write(a, w.b_hp, Value::Int(-1));

    rt.step();
    assert_eq!(rt.read(a, w.b_hp), Value::Null);
    assert_eq!(as_i64(&rt.read(b, w.b_hp)), 40);
    assert_eq!(as_i64(&rt.read(c, w.b_hp)), 100);

    rt.step();
    assert_eq!(splash_seq(&rt, cell, w), 1);
    assert_eq!(as_i64(&path(&rt.read(cell, w.c_splash), "dmg")), DMG);
    assert_eq!(as_i64(&rt.read(b, w.b_hp)), 40);
    assert_eq!(as_i64(&rt.read(c, w.b_hp)), 100);

    rt.step();
    assert_eq!(as_i64(&rt.read(b, w.b_hp)), -10);
    assert_eq!(as_i64(&rt.read(c, w.b_hp)), 50);

    rt.step();
    assert_eq!(rt.read(b, w.b_hp), Value::Null);
    assert_eq!(splash_seq(&rt, cell, w), 1);
    assert_eq!(as_i64(&rt.read(c, w.b_hp)), 50);

    rt.step();
    assert_eq!(splash_seq(&rt, cell, w), 2);
    assert_eq!(as_i64(&rt.read(c, w.b_hp)), 50);

    rt.step();
    assert_eq!(as_i64(&rt.read(c, w.b_hp)), 0);

    for _ in 0..3 {
        rt.step();
    }
    assert_eq!(as_i64(&rt.read(c, w.b_hp)), 0);
    assert_eq!(splash_seq(&rt, cell, w), 2);
}
