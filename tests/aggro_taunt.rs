//! Executable checks for docs/13-aggro-taunt.md:
//! hate bookkeeping is a single batch consumer, and dead-row clearing wins over
//! same-frame taunts without relying on delivery order.

use std::collections::{BTreeMap, BTreeSet};

use pce::entity::FIELD_ALIVE;
use pce::predicate::{new_path, type_scope};
use pce::{
    CmpOp, Cond, Delivery, EntityTypeId, Expr, FieldDef, FieldId, Input, InstanceId, Predicate,
    Proj, Runtime, Scope, ValRef, Value,
};

fn target_is_self() -> Cond {
    Cond::Cmp(new_path(&["target"]), CmpOp::Eq, Expr::Val(ValRef::SelfRef))
}

fn scope_or(scopes: Vec<Scope>) -> Scope {
    scopes
        .into_iter()
        .reduce(|a, b| Scope::Or(Box::new(a), Box::new(b)))
        .unwrap()
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

fn ref_key(v: &Value) -> String {
    match v {
        Value::Ref(inst) => format!("{}:{}", inst.ty.0, inst.id),
        _ => "null".to_string(),
    }
}

fn row(hate: i64, taunt_until: i64, salt: String, who: Value) -> Value {
    Value::map([
        ("hate", Value::Int(hate)),
        ("taunt_until", Value::Int(taunt_until)),
        ("salt", Value::Str(salt)),
        ("who", who),
    ])
}

#[derive(Clone, Copy)]
struct F {
    player_ty: EntityTypeId,
    enemy_ty: EntityTypeId,
    attack_out: FieldId,
    taunt_out: FieldId,
    hate_book: FieldId,
    current_target: FieldId,
}

fn setup() -> (Runtime, F) {
    let mut rt = Runtime::new();
    let player_ty = rt.register_entity_type(
        "Player",
        vec![
            FieldDef::new("attack_out", Value::Null),
            FieldDef::new("taunt_out", Value::Null),
        ],
        false,
    );
    let enemy_ty = rt.register_entity_type(
        "Enemy",
        vec![
            FieldDef::new("hate_book", Value::Map(BTreeMap::new())),
            FieldDef::new("book_stamp", Value::Int(0)),
            FieldDef::reference("current_target"),
        ],
        false,
    );
    let f = F {
        player_ty,
        enemy_ty,
        attack_out: rt.field(player_ty, "attack_out"),
        taunt_out: rt.field(player_ty, "taunt_out"),
        hate_book: rt.field(enemy_ty, "hate_book"),
        current_target: rt.field(enemy_ty, "current_target"),
    };
    let book_stamp = rt.field(enemy_ty, "book_stamp");

    rt.register_calculation(
        "hate_book",
        enemy_ty,
        Predicate::new(
            scope_or(vec![
                type_scope(player_ty, f.attack_out),
                type_scope(player_ty, f.taunt_out),
                type_scope(player_ty, FIELD_ALIVE),
            ]),
            Cond::Or(
                Box::new(target_is_self()),
                Box::new(Cond::Became(Value::Bool(false))),
            ),
            Delivery::Batch(vec![Proj::New(vec![]), Proj::WriterId]),
        ),
        &[f.hate_book, book_stamp, f.current_target],
        Box::new(move |ctx, input| {
            let Input::Batch(rows) = input else { return };
            let mut book = map_of(&ctx.read_own(f.hate_book));
            let mut dead = BTreeSet::new();
            let mut now = as_i64(&ctx.read_own(book_stamp));

            for rowv in rows {
                let new = &rowv[0];
                let writer = &rowv[1];
                if *new == Value::Bool(false) {
                    dead.insert(ref_key(writer));
                    continue;
                }

                now = now.max(as_i64(&path(new, "frame")));
                let source = path(new, "source");
                let key = ref_key(&source);
                let old = map_of(book.get(&key).unwrap_or(&Value::Null));
                let old_hate = as_i64(old.get("hate").unwrap_or(&Value::Int(0)));
                let old_until = as_i64(old.get("taunt_until").unwrap_or(&Value::Int(0)));
                let old_salt = as_str(old.get("salt").unwrap_or(&Value::Str(String::new())));

                match as_str(&path(new, "kind")).as_str() {
                    "damage" => {
                        book.insert(
                            key,
                            row(
                                old_hate + as_i64(&path(new, "amount")),
                                old_until,
                                old_salt,
                                source,
                            ),
                        );
                    }
                    "taunt" => {
                        let until = as_i64(&path(new, "frame")) + 120;
                        let salt = as_str(&path(new, "salt"));
                        let (until, salt) = if (until, salt.clone()) > (old_until, old_salt.clone())
                        {
                            (until, salt)
                        } else {
                            (old_until, old_salt)
                        };
                        book.insert(key, row(old_hate, until, salt, source));
                    }
                    _ => {}
                }
            }

            for key in dead {
                book.remove(&key);
            }

            let current = ctx.read_own(f.current_target);
            let current_key = ref_key(&current);
            let best = book.iter().max_by_key(|(_, v)| {
                let m = map_of(v);
                let active = (as_i64(m.get("taunt_until").unwrap_or(&Value::Int(0))) > now) as i64;
                (
                    active,
                    as_i64(m.get("hate").unwrap_or(&Value::Int(0))),
                    as_str(m.get("salt").unwrap_or(&Value::Str(String::new()))),
                )
            });
            let next = match best {
                None => Value::Null,
                Some((key, v)) => {
                    let best_map = map_of(v);
                    let best_hate = as_i64(best_map.get("hate").unwrap_or(&Value::Int(0)));
                    let best_taunted =
                        as_i64(best_map.get("taunt_until").unwrap_or(&Value::Int(0))) > now;
                    let current_hate = book
                        .get(&current_key)
                        .map(map_of)
                        .map(|m| as_i64(m.get("hate").unwrap_or(&Value::Int(0))))
                        .unwrap_or(0);
                    let switch_target = best_taunted
                        || current == Value::Null
                        || *key == current_key
                        || best_hate * 10 > current_hate * 11;
                    if switch_target {
                        best_map.get("who").cloned().unwrap_or(Value::Null)
                    } else {
                        current
                    }
                }
            };

            ctx.write(f.hate_book, Value::Map(book));
            ctx.write(book_stamp, Value::Int(now));
            ctx.write(f.current_target, next);
        }),
    )
    .unwrap();

    (rt, f)
}

fn damage(target: InstanceId, source: InstanceId, amount: i64, frame: i64, salt: &str) -> Value {
    Value::map([
        ("kind", Value::str("damage")),
        ("target", Value::Ref(target)),
        ("source", Value::Ref(source)),
        ("amount", Value::Int(amount)),
        ("frame", Value::Int(frame)),
        ("salt", Value::str(salt)),
    ])
}

fn taunt(target: InstanceId, source: InstanceId, frame: i64, salt: &str) -> Value {
    Value::map([
        ("kind", Value::str("taunt")),
        ("target", Value::Ref(target)),
        ("source", Value::Ref(source)),
        ("frame", Value::Int(frame)),
        ("salt", Value::str(salt)),
    ])
}

#[test]
fn same_frame_taunt_and_death_clears_dead_row_before_targeting() {
    let (mut rt, f) = setup();
    let tank = rt.spawn(f.player_ty, vec![]);
    let rogue = rt.spawn(f.player_ty, vec![]);
    let enemy = rt.spawn(f.enemy_ty, vec![]);

    rt.debug_write(
        tank,
        f.attack_out,
        damage(enemy, tank, 100, rt.frame() as i64, "tank-hit"),
    );
    rt.step();
    assert_eq!(rt.read(enemy, f.current_target), Value::Ref(tank));

    rt.debug_write(
        rogue,
        f.taunt_out,
        taunt(enemy, rogue, rt.frame() as i64, "rogue-taunt"),
    );
    rt.destroy(rogue);
    rt.step();

    assert_eq!(rt.read(enemy, f.current_target), Value::Ref(tank));
    let book = map_of(&rt.read(enemy, f.hate_book));
    assert!(book.contains_key(&ref_key(&Value::Ref(tank))));
    assert!(!book.contains_key(&ref_key(&Value::Ref(rogue))));
}
