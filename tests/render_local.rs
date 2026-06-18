//! render-local temporary entity channel tests.
//!
//! Covers render-owned pooled ids, local continuous lifecycle, local submission,
//! and spawning visual-only entities from shared render reactions.

use pce::{
    Cond, FieldDef, Proj, Publisher, RFieldId, RenderBinding, RenderLocalFieldDef,
    RenderLocalTypeId, RenderRuntime, Runtime, Value,
};

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

fn local_particle_type(rr: &mut RenderRuntime) -> (pce::RenderLocalTypeId, Fields) {
    let ty = rr.register_local_type(
        "Particle",
        vec![
            RenderLocalFieldDef::new("pos", Value::vec3(0.0, 0.0, 0.0)),
            RenderLocalFieldDef::new("vel", Value::vec3(0.0, 0.0, 0.0)),
            RenderLocalFieldDef::new("ttl", Value::Float(0.0)),
            RenderLocalFieldDef::new("fade", Value::Float(1.0)),
            RenderLocalFieldDef::new("mesh", Value::Int(0)),
        ],
    );
    let f = Fields {
        pos: rr.local_field(ty, "pos").unwrap(),
        vel: rr.local_field(ty, "vel").unwrap(),
        ttl: rr.local_field(ty, "ttl").unwrap(),
        fade: rr.local_field(ty, "fade").unwrap(),
        mesh: rr.local_field(ty, "mesh").unwrap(),
    };
    (ty, f)
}

#[derive(Clone, Copy)]
struct Fields {
    pos: RFieldId,
    vel: RFieldId,
    ttl: RFieldId,
    fade: RFieldId,
    mesh: RFieldId,
}

#[test]
fn local_particle_updates_submits_expires_and_reuses_pool_slot() {
    let rt = Runtime::new();
    let mut rr = RenderRuntime::new(&rt);
    let (particle, f) = local_particle_type(&mut rr);

    rr.local_continuous(
        "particle_tick",
        particle,
        &[f.pos, f.ttl, f.fade],
        Box::new(move |ctx| {
            let p = ctx.read(f.pos).as_vec3().unwrap_or([0.0; 3]);
            let v = ctx.read(f.vel).as_vec3().unwrap_or([0.0; 3]);
            let ttl = ctx.read(f.ttl).as_f64().unwrap_or(0.0) - ctx.dt();
            ctx.write(
                f.pos,
                Value::vec3(
                    p[0] + v[0] * ctx.dt(),
                    p[1] + v[1] * ctx.dt(),
                    p[2] + v[2] * ctx.dt(),
                ),
            );
            ctx.write(f.ttl, ttl);
            ctx.write(f.fade, (ttl / 0.5).clamp(0.0, 1.0));
            if ttl <= 0.0 {
                ctx.destroy_self();
            }
        }),
    )
    .unwrap();
    rr.local_renderable(
        particle,
        RenderBinding {
            translation: Some(f.pos),
            mesh: Some(f.mesh),
            fade: Some(f.fade),
            ..Default::default()
        },
    )
    .unwrap();

    let a = rr
        .spawn_local(
            particle,
            vec![
                (f.pos, Value::vec3(0.0, 0.0, 0.0)),
                (f.vel, Value::vec3(4.0, 0.0, 0.0)),
                (f.ttl, Value::Float(0.5)),
                (f.mesh, Value::Int(7)),
            ],
        )
        .unwrap();
    assert_eq!(rr.local_count(particle), 1);

    rr.render_frame(0.25, 1.0);
    assert!(rr.is_local_present(a));
    assert_eq!(rr.read_local(a, f.pos), Value::vec3(1.0, 0.0, 0.0));
    assert!(approx(rr.read_local(a, f.fade).as_f64().unwrap(), 0.5));
    let view = rr.submit_local();
    assert_eq!(view.len(), 1);
    assert_eq!(view.packets[0].local, a);
    assert_eq!(view.packets[0].mesh, Value::Int(7));
    assert!(approx(view.packets[0].fade, 0.5));
    let rows = view.instance_rows();
    assert_eq!(rows[0].translation_fade, [1.0, 0.0, 0.0, 0.5]);
    assert_eq!(rows[0].ids[0], 7);
    assert_eq!(rows[0].packet_index(), Some(0));

    rr.render_frame(0.25, 1.0);
    assert!(!rr.is_local_present(a), "ttl 用尽后 local 实体自毁");
    assert_eq!(rr.local_count(particle), 0);
    assert!(rr.submit_local().is_empty());

    let b = rr
        .spawn_local(particle, vec![(f.ttl, Value::Float(1.0))])
        .unwrap();
    assert_eq!(b.id, a.id, "本地池复用释放的 slot");
    assert_ne!(b, a, "generation 不同，旧句柄不会误指新住户");
    assert!(!rr.is_local_present(a));
    assert!(rr.is_local_present(b));
}

#[test]
fn shared_reaction_can_spawn_render_local_floating_text() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("hp", Value::Int(30))], false);
    let f_hp = rt.field(unit, "hp");
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let text_ty = rr.register_local_type(
        "FloatingText",
        vec![
            RenderLocalFieldDef::new("pos", Value::vec3(0.0, 0.0, 0.0)),
            RenderLocalFieldDef::new("text", Value::str("")),
            RenderLocalFieldDef::new("ttl", Value::Float(0.75)),
            RenderLocalFieldDef::new("mesh", Value::Int(0)),
        ],
    );
    let r_pos = rr.local_field(text_ty, "pos").unwrap();
    let r_text = rr.local_field(text_ty, "text").unwrap();
    let r_ttl = rr.local_field(text_ty, "ttl").unwrap();
    let r_mesh = rr.local_field(text_ty, "mesh").unwrap();
    rr.local_renderable(
        text_ty,
        RenderBinding {
            translation: Some(r_pos),
            mesh: Some(r_mesh),
            ..Default::default()
        },
    )
    .unwrap();
    rr.reaction(
        "damage_text",
        unit,
        f_hp,
        Cond::Became(Value::Int(20)),
        vec![Proj::New(vec![]), Proj::Old(vec![])],
        false,
        &[],
        Box::new(move |ctx, input| {
            let new = input.arg(0).as_i64().unwrap_or(0);
            let old = input.arg(1).as_i64().unwrap_or(new);
            let damage = old - new;
            ctx.spawn_local(
                text_ty,
                vec![
                    (r_pos, Value::vec3(3.0, 4.0, 0.0)),
                    (r_text, Value::from(format!("-{damage}"))),
                    (r_ttl, Value::Float(0.75)),
                    (r_mesh, Value::Int(99)),
                ],
            );
        }),
    )
    .unwrap();
    let publisher = Publisher::new(rr.tracked_fields());

    let u = rt.spawn(unit, vec![(f_hp, Value::Int(30))]);
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 1.0);
    assert!(rr.submit_local().is_empty(), "出生 hp=30 不生成飘字");

    rt.debug_write(u, f_hp, Value::Int(20));
    rt.step();
    publisher.publish(&rt);
    rr.sync(&publisher, 0.016, 1.0);

    let view = rr.submit_local();
    assert_eq!(view.len(), 1);
    let local = view.packets[0].local;
    assert_eq!(view.packets[0].translation, Value::vec3(3.0, 4.0, 0.0));
    assert_eq!(view.packets[0].mesh, Value::Int(99));
    assert_eq!(rr.read_local(local, r_text), Value::str("-10"));
    assert_eq!(rr.read_local(local, r_ttl), Value::Float(0.75));
}

#[test]
fn local_d1_rejects_writer_collisions_and_duplicate_binding_fields() {
    let rt = Runtime::new();
    let mut rr = RenderRuntime::new(&rt);
    let (particle, f) = local_particle_type(&mut rr);

    rr.local_continuous("a", particle, &[f.ttl], Box::new(|_| {}))
        .unwrap();
    assert!(
        rr.local_continuous("b", particle, &[f.ttl], Box::new(|_| {}))
            .is_err(),
        "local 字段已有写者，第二个 writer 应注册失败"
    );
    assert!(
        rr.local_continuous("dup", particle, &[f.fade, f.fade], Box::new(|_| {}))
            .is_err(),
        "同一 local calc 片内重复声明应报错"
    );
    assert!(
        rr.local_renderable(
            particle,
            RenderBinding {
                mesh: Some(f.mesh),
                material: Some(f.mesh),
                ..Default::default()
            }
        )
        .is_err(),
        "一个 local 字段不能绑定两个提交槽位"
    );
}

#[test]
fn invalid_calc_side_spawn_local_is_reported_without_default_panic() {
    let rt = Runtime::new();
    let mut rr = RenderRuntime::new(&rt);
    let (particle, _f) = local_particle_type(&mut rr);
    rr.local_continuous(
        "bad_spawn",
        particle,
        &[],
        Box::new(|ctx| {
            ctx.spawn_local(RenderLocalTypeId(999), vec![]);
        }),
    )
    .unwrap();

    let _id = rr.spawn_local(particle, vec![]).unwrap();
    rr.render_frame(0.016, 1.0);
    assert_eq!(
        rr.local_count(particle),
        1,
        "invalid queued spawn is dropped instead of panicking or creating a bad local"
    );
}
