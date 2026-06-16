# PredicateCalculationEngine (PCE)

**Language:** English | [中文](README-zh.md)

A pure data-driven predicate calculation engine implemented in Rust, aimed at high-frequency frame-driven domains such as game logic.
The **simulation core has exactly four abstraction layers: runtime / entity / calculation / predicate**. Every business requirement must be folded into these four layers. There is no fifth concept: no messages, no event bus, no callbacks, and no global functions. Above the core there are two derived constructs that reuse the same four-layer closure and are not new concepts: the **derived consumer runtime** (`render`, a second runtime that consumes the sim write stream one-way) and the **materialized-index helper** (`spatial`).

The only trigger source is "writes from the previous frame." Polling, messages, and events are all unified as writes to a cell, where a **cell** is one field of one entity instance.

## Core Design

| Layer | Responsibility |
|---|---|
| **runtime** | The only scheduler and index owner: double buffering, write-set routing, predicate indexes, incremental fold state, and instance lifecycle |
| **entity** | The smallest instantiated unit (`entityname.id`); global state is represented as singleton entities such as `Clock.0` |
| **calculation** | Turing-complete business code; input is the delivery from the preceding predicate as value snapshots, and output may only write fields on its own instance |
| **predicate** | A declaration-shaped triple `(scope, condition, delivery)` fixed at registration time; closed algebra, compilable, and indexable |

Four pinned decisions:

- **D1 Single writer**: every cell statically belongs to exactly one writer: a calculation, a built-in runtime writer, or a render extension writer. Conflicts are rejected at registration.
- **D2 Writes are events**: every write produces an event, even if the value does not change. Real value changes are expressed explicitly with `changed`.
- **D3 Batches are unordered**: batch delivery order is undefined. Consumers must be order-independent and treat input as a multiset.
- **D4 Effect confinement**: a calculation closure's only observable effects go through `ctx`; there are no ambient side effects outside `ctx`. Together with write locality, this is what makes free reordering and execution-stage parallelism legal.

These constraints buy snapshot reads, parallel execution without data races, feedback loops that naturally unfold as frame-to-frame ping-pong, and the **cost invariant**: total per-frame scheduling cost is `O(|W|*log + |F|)`, independent of the total number of predicates, instances, or cells.

## Use As A Dependency

This crate is a pure library crate (`publish = false`; the crate name `pce` is a placeholder). Use it as a path dependency:

```toml
[dependencies]
pce = { path = "../predicate-calculation-engine" }
# Optional: parallel execution stage.
# D1 + write locality make this legal without execution-stage data races.
# pce = { path = "...", features = ["parallel"] }
```

The crate entry point exposes the four core layers (`runtime` / `entity` / `calculation` / `predicate`) plus the derived consumer runtime (`render`) and the materialized-index helper (`spatial`).
The earlier Rust API shape that closely mirrored the documentation DSL has moved to [docs-zh/original-api-shape.rs](docs-zh/original-api-shape.rs) as a teaching reference only; it is no longer exported as a module and is not compiled into the library.

## Example

Predicate DSL style from document Section 7:

```text
# HP crosses below 30%: edge-triggered, not repeated every frame
on    own(hp)
where crossed(0.3 * own.hp_max, down)
each  deliver(new, old)
-> flee_calc                         # writes own(state)
```

The same idea using the pure library core API:

```rust
use pce::predicate::{own, own_field};
use pce::{
    Cond, Delivery, Dir, Expr, FieldDef, Predicate, Proj, Runtime, ValRef, Value,
};

let mut rt = Runtime::new();
let unit = rt.register_entity_type(
    "Unit",
    vec![
        FieldDef::new("hp", Value::Int(100)),
        FieldDef::new("hp_max", Value::Int(100)),
        FieldDef::new("state", Value::str("idle")),
    ],
    false,
);

let (f_hp, f_hp_max, f_state) = (
    rt.field(unit, "hp"),
    rt.field(unit, "hp_max"),
    rt.field(unit, "state"),
);

rt.register_calculation(
    "flee",
    unit,
    Predicate::new(
        own(f_hp),
        Cond::Crossed(
            Expr::Mul(
                Box::new(own_field(f_hp_max)),
                Box::new(Expr::Val(ValRef::Const(Value::Float(0.3)))),
            ),
            Dir::Down,
        ),
        Delivery::Each(vec![Proj::New(vec![]), Proj::Old(vec![])]),
    ),
    &[f_state],
    Box::new(move |ctx, _| ctx.write(f_state, "fleeing")),
)
.unwrap();
```

For cross-entity attacks (`new.target = self`, compiled into an equality fast path by ref lookup), fold-based incremental aggregation, and per-frame ECS-style systems, see [examples/demo.rs](examples/demo.rs).

## Optimizations

**Always-on optimizations (Layer A):** SoA column storage (`(type, field)` -> dense columns); double buffering as single storage plus write logs, committed at frame boundaries; value buckets for type scopes (`O(1)+k` for constant equality); shared sorted threshold tables and crossed interval queries (`O(log s + k)`); equivalent-condition merged evaluation; incremental fold maintenance (`sum`/`count` by +/-delta, `min`/`max` by multiset); `type(Clock, frame)` plus a true condition recognized at registration as a classic ECS system, bypassing routing and iterating dense columns directly; cross-frame route scratch reuse; and a free profiler (`Runtime::profile`), because routing input already contains per-cell write frequency through D2.

**Developer-selected tiers (Layer C):**

| Tier | Entry Point | Cost |
|---|---|---|
| C1 Execution tier | `.tier(Tier::Kernel)` | Restricted subset; divergence risk is on the caller |
| C2 Read-set declaration | `.reads(["hp_max"])` | Declaration burden; buys hot/cold separation and more precise prefetching |
| C3 Residency pinning | `.residency(Residency::Gpu)` | No static perfect answer; pair with profiling and hysteresis |
| C4 Determinism | `rt.set_determinism(Canonical)` | Canonical batch ordering cost; useful for lockstep/replay |
| C5 Detection tier | `rt.set_detect(Strict/Warn/Silent)` | Strict/Warn can pollute the hot path; default follows build mode |
| C6 Row identity | `.compact()` (stable rows by default) | Stable rows leave holes; compact rows require death-time remapping |

## Quick Start

```powershell
cargo run --example demo          # Section 7 examples 1 + 2 + 4
cargo test                        # Scenario tests plus core API / optimization behavior
cargo test --features parallel    # The same test set with rayon-backed execution-stage parallelism
```

## Repository Layout

```text
src/
  lib.rs             # Crate entry point, four-layer core + derived runtime / helper exports
  entity.rs          # Entity types, instances, fields, and cell addresses
  predicate.rs       # Predicate algebra: scope / condition / delivery
  calculation.rs     # Calculation execution context, C1/C2 detection-aware
  value.rs           # Cell value type
  runtime/
    mod.rs           # Scheduler: registration compilation, frame loop, tiers, profiler
    store.rs         # SoA column storage and row identity policy (C6)
    route.rs         # Routing indexes: value buckets, threshold tables, equivalence merge, fold
    clock.rs         # Clock and alarm timer-wheel surface
  render/            # Derived consumer runtime: dynamic frame rate, consumes sim writes one-way
  spatial.rs         # Materialized-index helper: uniform grid for the Section 6.1 pattern
examples/demo.rs     # Complete pure-library core API example
docs/
  PCE.md             # Architecture guide: invariants, derived runtime, and cost model
  README.md          # Scenario-method index
  01..23-*.md        # Scenario documents
docs-zh/
  PCE文档.md          # Chinese architecture guide
  original-api-shape.rs # Original API shape; teaching reference only, not compiled
  README.md          # Chinese scenario-method index
  01..23-*.md        # Chinese scenario documents
tests/               # Scenario tests plus core API / optimization behavior
```

## Documentation

- Architecture guide: [docs/PCE.md](docs/PCE.md). Covers the four layers, frame model, predicate specification, cost model and index binding, registration-time compilation, invariants, and open questions.
- Scenario method collection: [docs/README.md](docs/README.md). Covers 23 tricky logic patterns implemented using only predicate + calculation + entity decomposition, including aggro/taunt, combo cancel, projectiles, time dilation, and symmetric trades.
- Chinese documentation is kept under [docs-zh/](docs-zh/) with the Chinese root README at [README-zh.md](README-zh.md).

## Test Coverage

The 23 scenario documents under `docs/01..23-*.md` and `docs-zh/01..23-*.md` have corresponding Rust integration tests under `tests/`. Each test file validates the key decomposition, invariants, and D1-D4 constraints from its scenario.

```powershell
cargo test
```

## Status

Version `0.1.0`, pure library crate, unpublished. The default build has no third-party dependencies; the optional `parallel` feature enables rayon for execution-stage parallelism.

The Section 4 cost-table index bindings are implemented across the runtime: own/inst hash chains, value buckets, shared threshold tables, incremental folds, and conjunction latches. Always-on Layer A optimizations are active, Layer C tier entry points are exposed, and the 23 scenario integration tests plus core API / optimization behavior tests form the current regression net. SIMD kernel code generation and GPU-residency backends remain future C1/C3 backend work; the structural hooks are already present through column storage, threshold tables, `Tier`/`Residency` annotations, and profiler edge telemetry.
