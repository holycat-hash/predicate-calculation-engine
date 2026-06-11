# PredicateCalculationEngine (PCE)

**Language:** English | [中文](README-zh.md)

A pure data-driven predicate calculation engine implemented in Rust, aimed at high-frequency frame-driven domains such as game logic.
The system has exactly four abstraction layers: **runtime / entity / calculation / predicate**. Every new requirement must be folded into these four layers. There is no fifth concept: no messages, no event bus, no callbacks, and no global functions.

The only trigger source is "writes from the previous frame." Polling, messages, and events are all unified as writes to a cell, where a **cell** is one field of one entity instance.

## Core Design

| Layer | Responsibility |
|---|---|
| **runtime** | The only scheduler and index owner: double buffering, write-set routing, predicate indexes, incremental fold state, and instance lifecycle |
| **entity** | The smallest instantiated unit (`entityname.id`); global state is represented as singleton entities such as `Clock.0` |
| **calculation** | Turing-complete business code; input is the delivery from the preceding predicate as value snapshots, and output may only write fields on its own instance |
| **predicate** | A declaration-shaped triple `(scope, condition, delivery)` fixed at registration time; closed algebra, compilable, and indexable |

Three pinned decisions:

- **D1 Single writer**: every field statically belongs to exactly one calculation. Conflicts are rejected at registration.
- **D2 Writes are events**: every write produces an event, even if the value does not change. Real value changes are expressed explicitly with `changed`.
- **D3 Batches are unordered**: batch delivery order is undefined. Consumers must be order-independent and treat input as a multiset.

These constraints buy snapshot reads, parallel execution without data races, feedback loops that naturally unfold as frame-to-frame ping-pong, and the **cost invariant**: total per-frame scheduling cost is `O(|W|*log + |F|)`, independent of the total number of predicates, instances, or cells.

## Example

Predicate DSL style from document Section 7:

```text
# HP crosses below 30%: edge-triggered, not repeated every frame
on    own(hp)
where crossed(0.3 * own.hp_max, down)
each  deliver(new, old)
-> flee_calc                         # writes own(state)

# Attack: the only cross-entity interaction path is writing yourself
# and letting the target sniff that write.
on    type(Attacker, attack_out)
where new.target = self
each  deliver(new.dmg)
-> take_damage_calc                  # target writes own(hp)
```

See [src/main.rs](src/main.rs) for the corresponding Rust API usage in the `pce-demo` executable.

## Quick Start

```powershell
cargo run            # Run the demo (Section 7 examples 1 + 2)
cargo test           # Run the scenario tests under tests/
```

## Repository Layout

```text
src/
  lib.rs             # Crate entry point and core exports
  entity.rs          # Entity types, instances, fields, and cell addresses
  predicate.rs       # Predicate algebra: scope / condition / delivery
  calculation.rs     # Calculation registration and execution context
  value.rs           # Cell value type
  runtime/           # Scheduler: double buffers, write routing, clock
docs/
  PCE.md             # Architecture guide
  README.md          # Scenario-method index
  01..23-*.md        # Scenario documents
docs-zh/
  ...                # Chinese documentation
tests/               # Executable Rust integration tests derived from the scenario docs
```

## Documentation

- Architecture guide: [docs/PCE.md](docs/PCE.md). Covers the four layers, frame model, predicate specification, cost model and index binding, registration-time compilation, invariants, and open questions.
- Scenario method collection: [docs/README.md](docs/README.md). Covers 23 tricky logic patterns implemented using only predicate + calculation + entity decomposition, including aggro/taunt, combo cancel, projectiles, time dilation, and symmetric trades.
- Chinese documentation is kept under [docs-zh/](docs-zh/) with the Chinese root README at [README-zh.md](README-zh.md).

## Test Coverage

The 23 scenario documents under `docs/01..23-*.md` have corresponding Rust integration tests under `tests/`. Each test file validates the key decomposition, invariants, and D1/D2/D3 constraints from its scenario.

```powershell
cargo test
```

## Status

Early prototype (`0.1.0`, no third-party dependencies). The runtime is still scaffold-level and some index optimization / predicate compilation convergence is still marked as `TODO`, but all 23 scenario documents already have executable integration tests that serve as the current behavior and design regression net.
