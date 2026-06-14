//! 类型化去装箱列 + 谓词预编译的语义保持验证。
//! - 类型化列：异构写入触发去优化（deopt → Boxed），保精确往返、不染邻行。
//! - 谓词预编译：常量子表达式折叠、复合 And/Or/AndNot 后缀求值逐字等价于原 AST。

use std::panic::{AssertUnwindSafe, catch_unwind};

use pce::predicate::{lit, new_val, type_scope};
use pce::{CmpOp, Cond, Delivery, Expr, FieldDef, Predicate, Runtime, Value};

fn field(name: &str, default: impl Into<Value>) -> FieldDef {
    FieldDef::new(name, default.into())
}

/// 向 Int 列写 Float / Null → 列去优化为 Boxed，精确往返，邻行不受污染。
#[test]
fn typed_column_deopt_preserves_values() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![field("n", 0)], false);
    let f_n = rt.field(unit, "n");

    let a = rt.spawn(unit, vec![(f_n, Value::Int(5))]);
    let b = rt.spawn(unit, vec![(f_n, Value::Int(9))]);
    rt.step();
    assert_eq!(rt.read(a, f_n), Value::Int(5));
    assert_eq!(rt.read(b, f_n), Value::Int(9));

    // 向 Int 列写 Float：触发 deopt。a 精确存 Float，b 仍是 Int（不被污染）。
    rt.debug_write(a, f_n, Value::Float(2.5));
    assert_eq!(rt.read(a, f_n), Value::Float(2.5)); // 精确往返，未被强转 Int
    assert_eq!(rt.read(b, f_n), Value::Int(9));

    // deopt 后该列仍正常工作：再写 Int / Null 都精确往返。
    rt.debug_write(b, f_n, Value::Int(7));
    assert_eq!(rt.read(b, f_n), Value::Int(7));
    rt.debug_write(a, f_n, Value::Null);
    assert_eq!(rt.read(a, f_n), Value::Null);
}

/// Bool 列（_alive 与普通 bool 字段）走无装箱快路；存活遍历正确。
#[test]
fn bool_column_alive_scan() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![field("flag", false)], false);
    let _a = rt.spawn(unit, vec![]);
    let b = rt.spawn(unit, vec![]);
    let _c = rt.spawn(unit, vec![]);
    rt.step();
    assert_eq!(rt.alive(unit).len(), 3);
    rt.destroy(b);
    rt.step();
    assert_eq!(rt.alive(unit).len(), 2); // _alive 无装箱位扫描跳过死者
}

/// `_alive` 是 runtime 生命周期位，不能由 spawn init 污染成非 Bool。
#[test]
#[should_panic(expected = "spawn init")]
fn spawn_rejects_alive_init() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![field("v", 0)], false);
    let alive = rt.field(unit, "_alive");
    let _ = rt.spawn(unit, vec![(alive, Value::Null)]);
}

/// debug_write 不能绕过 destroy API 直接写 `_alive`。
#[test]
#[should_panic(expected = "debug_write")]
fn debug_write_rejects_alive_field() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![field("v", 0)], false);
    let alive = rt.field(unit, "_alive");
    let u = rt.spawn(unit, vec![]);
    rt.debug_write(u, alive, Value::Null);
}

/// spawn 初始化非法字段时必须在分配前失败，不留下 live row / pending birth。
#[test]
fn failed_spawn_does_not_leave_partial_instance() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![field("v", 0)], false);
    let _u = rt.spawn(unit, vec![]);
    assert_eq!(rt.alive(unit).len(), 1);

    let err = catch_unwind(AssertUnwindSafe(|| {
        rt.spawn(unit, vec![(pce::FieldId(999), Value::Int(1))]);
    }));
    assert!(err.is_err());
    assert_eq!(rt.alive(unit).len(), 1);
    rt.step();
    assert_eq!(rt.alive(unit).len(), 1);
}

/// 谓词预编译：常量算术子表达式折叠（`0.3 * 100` → `30`），语义不变。
#[test]
fn precompiled_constant_arithmetic_condition() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![field("hp", 100)], false);
    let watch = rt.register_entity_type("Watch", vec![field("hits", 0)], true);
    let f_hp = rt.field(unit, "hp");
    let f_hits = rt.field(watch, "hits");

    // new < 0.3 * 100（编译期折叠为 new < 30）
    rt.register_calculation(
        "low",
        watch,
        Predicate::new(
            type_scope(unit, f_hp),
            Cond::Cmp(
                new_val(),
                CmpOp::Lt,
                Expr::Mul(
                    Box::new(lit(Value::Float(0.3))),
                    Box::new(lit(Value::Int(100))),
                ),
            ),
            Delivery::Each(vec![]),
        ),
        &[f_hits],
        Box::new(move |ctx, _| {
            let n = ctx.read_own(f_hits).as_i64().unwrap();
            ctx.write(f_hits, n + 1);
        }),
    )
    .unwrap();

    let u = rt.spawn(unit, vec![]);
    let w0 = rt.alive(watch)[0];
    rt.step();

    rt.debug_write(u, f_hp, Value::Int(25)); // 25 < 30 → 命中
    rt.step();
    assert_eq!(rt.read(w0, f_hits), Value::Int(1));
    rt.debug_write(u, f_hp, Value::Int(35)); // 35 ≥ 30 → 不命中
    rt.step();
    assert_eq!(rt.read(w0, f_hits), Value::Int(1));
    rt.debug_write(u, f_hp, Value::Int(29)); // 命中
    rt.step();
    assert_eq!(rt.read(w0, f_hits), Value::Int(2));
}

/// 谓词预编译：复合 And/Or/AndNot 后缀求值与原 AST 逐字等价。
#[test]
fn precompiled_compound_boolean_conditions() {
    let mut rt = Runtime::new();
    let unit = rt.register_entity_type("Unit", vec![field("v", 0)], false);
    let w = rt.register_entity_type("W", vec![field("band", 0), field("oddlow", 0)], true);
    let f_v = rt.field(unit, "v");
    let (f_band, f_oddlow) = (rt.field(w, "band"), rt.field(w, "oddlow"));

    // And：10 < new < 50（band）
    rt.register_calculation(
        "band",
        w,
        Predicate::new(
            type_scope(unit, f_v),
            Cond::And(
                Box::new(Cond::Cmp(new_val(), CmpOp::Gt, lit(Value::Int(10)))),
                Box::new(Cond::Cmp(new_val(), CmpOp::Lt, lit(Value::Int(50)))),
            ),
            Delivery::Each(vec![]),
        ),
        &[f_band],
        Box::new(move |ctx, _| {
            let n = ctx.read_own(f_band).as_i64().unwrap();
            ctx.write(f_band, n + 1);
        }),
    )
    .unwrap();

    // AndNot：new < 50 且 not(new < 20) ⇔ 20 ≤ new < 50（守卫式否定）
    rt.register_calculation(
        "oddlow",
        w,
        Predicate::new(
            type_scope(unit, f_v),
            Cond::AndNot(
                Box::new(Cond::Cmp(new_val(), CmpOp::Lt, lit(Value::Int(50)))),
                Box::new(Cond::Cmp(new_val(), CmpOp::Lt, lit(Value::Int(20)))),
            ),
            Delivery::Each(vec![]),
        ),
        &[f_oddlow],
        Box::new(move |ctx, _| {
            let n = ctx.read_own(f_oddlow).as_i64().unwrap();
            ctx.write(f_oddlow, n + 1);
        }),
    )
    .unwrap();

    let u = rt.spawn(unit, vec![]);
    let w0 = rt.alive(w)[0];
    rt.step();

    rt.debug_write(u, f_v, Value::Int(30)); // band: yes(10<30<50)  oddlow: yes(20≤30<50)
    rt.step();
    assert_eq!(rt.read(w0, f_band), Value::Int(1));
    assert_eq!(rt.read(w0, f_oddlow), Value::Int(1));

    rt.debug_write(u, f_v, Value::Int(15)); // band: yes(10<15<50)  oddlow: no(15<20)
    rt.step();
    assert_eq!(rt.read(w0, f_band), Value::Int(2));
    assert_eq!(rt.read(w0, f_oddlow), Value::Int(1));

    rt.debug_write(u, f_v, Value::Int(60)); // band: no   oddlow: no
    rt.step();
    assert_eq!(rt.read(w0, f_band), Value::Int(2));
    assert_eq!(rt.read(w0, f_oddlow), Value::Int(1));
}
