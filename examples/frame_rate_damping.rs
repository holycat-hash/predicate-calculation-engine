//! Example: frame-rate independent damping on the render runtime.
//!
//! Run:
//!   cargo run --example frame_rate_damping

use pce::{FieldDef, Publisher, RenderRuntime, Runtime, Value};

const TARGET: f64 = 100.0;
const K: f64 = 3.0;
const FIXED_LERP: f64 = 0.10;

fn main() {
    let coarse = vec![0.200; 5];
    let fine = vec![0.005; 200];
    let mixed = vec![
        0.100, 0.016, 0.033, 0.250, 0.001, 0.100, 0.050, 0.200, 0.150, 0.100,
    ];

    let expected = TARGET * (1.0 - (-K).exp());
    let exp_coarse = run(&coarse, Damping::Exponential);
    let exp_fine = run(&fine, Damping::Exponential);
    let exp_mixed = run(&mixed, Damping::Exponential);
    let exp_max_diff = max_pairwise_diff(exp_coarse, exp_fine, exp_mixed);

    println!("== Frame-rate independent exponential damping ==");
    println!("target={TARGET:.1}, k={K:.1}, total=1.0s, expected={expected:.6}");
    println!("5 frames  x 0.200s -> x={exp_coarse:.6}");
    println!("200 frames x 0.005s -> x={exp_fine:.6}");
    println!("mixed variable dt   -> x={exp_mixed:.6}");
    println!("max difference      -> {exp_max_diff:.12}");

    assert!(
        exp_max_diff < 1e-9,
        "exponential damping should depend on total elapsed time, not frame count"
    );
    assert!(
        (exp_fine - expected).abs() < 1e-9,
        "1s of damping should match the closed-form solution"
    );

    let fixed_coarse = run(&coarse, Damping::FixedFactor);
    let fixed_fine = run(&fine, Damping::FixedFactor);
    let fixed_mixed = run(&mixed, Damping::FixedFactor);

    println!();
    println!("== Counter-example: fixed lerp factor per render frame ==");
    println!("factor={FIXED_LERP:.2}; the result now changes with frame count");
    println!("5 frames             -> x={fixed_coarse:.6}");
    println!("200 frames           -> x={fixed_fine:.6}");
    println!("mixed 10 frames      -> x={fixed_mixed:.6}");

    assert!(
        (fixed_coarse - fixed_fine).abs() > 50.0,
        "fixed per-frame lerp should visibly drift across frame rates"
    );
}

#[derive(Clone, Copy)]
enum Damping {
    Exponential,
    FixedFactor,
}

fn run(dts: &[f64], damping: Damping) -> f64 {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![FieldDef::new("anchor", Value::Int(0))], false);
    rt.enable_render_feed();

    let mut rr = RenderRuntime::new(&rt);
    let r_x = rr.add_render_field(unit, Value::Float(0.0));
    rr.continuous(
        "damp_to_target",
        unit,
        &[r_x],
        Box::new(move |ctx| {
            let x = ctx.read(r_x).as_f64().unwrap_or(0.0);
            let factor = match damping {
                Damping::Exponential => 1.0 - (-K * ctx.dt()).exp(),
                Damping::FixedFactor => FIXED_LERP,
            };
            ctx.write(r_x, x + (TARGET - x) * factor);
        }),
    )
    .unwrap();

    let publisher = Publisher::new(rr.tracked_fields());
    let u = rt.spawn(unit, vec![]);
    rt.step();
    publisher.publish(&rt);
    for sf in publisher.drain() {
        rr.ingest(&sf);
    }

    for &dt in dts {
        rr.render_frame(dt, 1.0);
    }

    rr.read(u, r_x).as_f64().unwrap()
}

fn max_pairwise_diff(a: f64, b: f64, c: f64) -> f64 {
    (a - b).abs().max((a - c).abs()).max((b - c).abs())
}
