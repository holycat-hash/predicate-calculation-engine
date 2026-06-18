# PredicateCalculationEngine

**Language:** English | [中文](../docs-zh/PCE文档.md)

---

## 0. General Rules

The **simulation core** of this architecture has exactly four abstraction layers: **runtime, entity, calculation, predicate**. Every business requirement must fold into these four layers. No fifth concept may be introduced. Everything that follows, including clock, lifecycle, spatial indexes, and cross-entity interaction, is a consequence of those four layers rather than an extension.

Above the core, it is still possible to build a **derived consumer runtime**. `render` (Section 9) is a second runtime that reuses the same four-layer closure and consumes the write log committed by sim in one direction. It is not a fifth concept, but it also is not part of the sim core. Keep two non-core categories distinct: derived consumer runtimes, which are independent second runtimes, and helper tools inside the four-layer pattern, such as the `spatial` materialized-index helper in Section 6.1, which remains an implementation detail of a calculation.

The system is purely data-driven. The only trigger source is "writes from the previous frame." There is no polling, message system, or event bus; all of them are unified as writes to a cell. A **cell** is one field of one entity instance and is the smallest unit of data, write, and subscription.

The predicate layer is a closed algebra. A predicate is a declaration-shaped structure fixed at registration time, expressed as data / AST rather than arbitrary functions. All Turing-completeness stays in calculation. This restriction is not a style preference; it is the source of the performance guarantee in Sections 3.5 and 4.

### 0.1 Decision Record

**D1 Single writer**: every cell statically belongs to exactly one **writer**. Ownership conflicts are errors at registration time. There are three writer classes: calculation writers, which are sim business logic and own the write sets declared on their own instance type; built-in runtime writers, which exclusively own built-in cells such as `Clock.frame` and `Clock.alarm` and manage the `_alive` lifecycle bit; and extension writers, such as the render derived consumer runtime, which claims render namespace fields (`RFieldId`) through `claim_writes` (Section 9). The unified invariant is one cell to one writer, checked at registration time. This makes `new` unambiguous and removes arbitration from parallel execution.

**D2 Writes are events**: every write produces an event, whether or not the value changes. "The value really changed" is expressed explicitly with `changed`.

**D3 Batches are unordered**: batch delivery order is undefined. The runtime may deliver in any order, such as shard order or arrival order, and the routing stage performs no sorting or ordering barrier.

**D4 Effect confinement**: a calculation closure's only observable effects occur through `ctx` (`write`, `spawn`, and `destroy_self`). The closure has no ambient side effects outside `ctx`, such as I/O, log feedback, global/static mutation, or RNG not seeded through cells.

This is orthogonal to **write locality**. Write locality constrains where a calculation writes: only fields on its own instance, enforced by `Ctx::write` not accepting an instance parameter plus D1. Effect confinement constrains what else can be touched outside `ctx`: nothing. It is currently a contract, not a machine-checked guarantee, because the execution layer is an opaque `Box<dyn Fn>`; the runtime can assume it but cannot verify it until a kernel-IR seam exists.

D4 is pinned because legal free reordering (D3 / cross-calculation regrouping) and execution-stage parallelism (`parallel` feature) depend on it. More precisely, D1 + write locality + snapshot reads + effect confinement imply that persistent cell values committed to the store are independent of trigger order. New entity identities are order-independent only under a fixed schedule (`Canonical`, C4); under `Free`, results may differ by an id renaming. D1-D3 provide order independence inside the store, while D4 excludes ambient effects outside the store. Without both, reordering and parallel execution are not valid.

In addition, Section 1.4's "single predicate per calculation" rule and Section 2's snapshot-read / write-folding model are fixed consequences of D1-D4 and write locality. They are important acceptance points for the design.

---

## 1. Four Layers

### 1.1 runtime

The runtime is the only scheduler and index owner. It maintains the data double buffers, collects the frame N write set, routes that write set to predicates in frame N+1, maintains predicate indexes and incremental fold state, manages instance lifecycle, id allocation, and ref reverse tables, and acts as the built-in writer for built-in cells such as `Clock.frame`.

The runtime contains no business logic. Everything it "understands" about predicates comes from registration-time compilation.

### 1.2 entity

An entity instance, written `entityname.id`, is the smallest instantiated unit. Ids carry no ordering semantics and may be reused; reuse safety is provided by hidden runtime generations. All data belongs to some entity instance. There is no global data outside instances. Global-looking state is represented as **singleton entities**, such as `Grid.0` or `Clock.0`.

An entity has no behavior. All behavior lives in calculations attached under its type.

### 1.3 calculation

A calculation is attached under an entity type and runs after a predicate. Its input is the preceding predicate's delivery as **value snapshots, not references**. Its output may only write fields of its **own instance**.

**Write locality**: a calculation can only write its own instance fields. Cross-instance influence must travel through dataflow: write your own field, then let the other side sniff it through a predicate.

**Single writer (D1)**: each cell statically belongs to exactly one writer. Registration checks this and rejects conflicts. Calculation is one of the three writer classes; the other two are built-in runtime writers and render extension writers (Section 0.1). That makes `new` unambiguous and lets execution run in parallel without arbitration.

**Effect confinement (D4)**: a calculation's Turing-complete code has no observable effects except through `ctx`. It is separate from write locality: write locality governs "where can this write go," while effect confinement governs "what else can this closure touch." It is also a precondition for free reordering and execution-stage parallelism (Sections 0.1 and 2).

**Snapshot reads**: any field read by a calculation, including its own, is the value committed in the previous frame. Writes in the current frame are invisible to the current frame.

Calculation code itself may be arbitrary Turing-complete code.

### 1.4 predicate

A predicate precedes a calculation and has the shape `(scope, condition, delivery)`, detailed in Section 3.

**Single predicate per calculation**: each calculation has exactly one preceding predicate. If multiple predicates with different delivery shapes were allowed to feed one calculation, the input signature would no longer be single. Composition still has paths inside the four layers: use scope union (`|`) for "any source"; use conjunction (`&`, with the current limitations in Section 3.2) for same-frame co-occurrence; and materialize aggregates first when a trigger stream needs an aggregate value: put another fold predicate + calculation on the same entity, write the aggregate into a field, and let the main calculation read it through `project(own.f)` (Section 3.4).

---

## 2. Frame Model and Dataflow

```text
Frame N:    calculation execution, writes enter the frame N write buffer
Frame N+1:  Stage 1 (routing) runtime holds frame N write set W:
                       index lookup -> condition check -> fill each triggers /
                       batch buffers / update fold
            Stage 2 (execution) triggered calculations run,
                       writes enter the frame N+1 write buffer
```

**Double-buffer consequence**: all predicates in frame N+1 see a consistent snapshot from frame N. Every cell's `old` value is available for free. Feedback loops such as A triggering B and B triggering A unfold naturally as frame-to-frame ping-pong. There is no in-frame loop.

**Parallelism**: routing can run by cell shard; execution can run by entity instance. Write locality and single-writer ownership remove execution-stage data races and arbitration.

**Write folding**: if one calculation run assigns the same field multiple times, those assignments fold into one write record. `new` is the final value and `old` is the value committed in the previous frame.

**Multiple triggers, D3 consequence 1**: with `each` delivery, one calculation may run multiple times in one frame. Every run uses the same snapshot. If those runs write the same field, folding order is undefined and the final value is nondeterministic. Therefore in-frame aggregation must be expressed with `batch` or `fold`; `each` read-modify-write accumulation is forbidden.

**Unordered delivery, D3 consequence 2**: batch consumers must be order-independent and treat delivery as a multiset. Replay determinism only holds for order-independent consumers.

---

## 3. Predicate Specification

### 3.1 Shape

```text
predicate = (scope, condition, delivery)
```

`scope` is the static part: known and indexable at registration time, deciding who can be woken. `condition` is the dynamic part: an O(1)-per-write check deciding whether it is worth waking. `delivery` is the pushed-down projection and cardinality: what is delivered and how many times. Each part binds to a known optimal data structure in Section 4.

### 3.2 scope

```text
scope := own(field)              # one cell on this same instance
       | inst(ref, field)        # a specific instance cell through a ref held by this instance
       | type(Entity, field)     # all instances of an entity type
       | scope | scope           # union: any source write may trigger
       | scope & scope           # conjunction: all sources write in the same frame
```

The `ref` used by `inst` must come from a ref-typed field on the subscriber instance.

**Current limitations of conjunction `&`.** Conjunction is a same-frame co-occurrence gate. The current implementation has these boundaries:

1. **Only `each` delivery**: conjunction scopes currently support only `each`. Using `batch` or `fold` with conjunction is rejected at registration time because the meaning of aggregation under same-frame gating is still undefined (Section 8).
2. **Scope shape**: only "conjunction of disjunctions," such as `(a|b) & c`, is supported. "Disjunction of conjunctions," such as `(a&b) | c`, is rejected. For deeper nesting, materialize the intermediate value as an entity (Section 6.1).
3. **Gate, not join**: `&` only checks that every branch wrote during the same frame. It does not join values. When the latch completes, delivery projects only the final write that completed the latch; it cannot deliver all branch values together. Association is by subscriber identity, not by a join key. Keyed association is a join and is forbidden by Section 3.3. Each subscriber can trigger at most once per frame.
4. **Branch limit**: the conjunction latch is a `u32` bit mask, with the top bit reserved as the "already triggered this frame" flag, so the implementation limit is 31 branches.
5. **Same-frame barrier**: `&` needs the complete frame write set and is the only pipelining barrier inside routing. Co-occurrence is strictly same-frame; there is no time window. Timeout and "silent for N frames" semantics are expressed with clock cells (Section 6.2).

### 3.3 condition

Conditions may reference only a closed set: `new`, `old`, fields on the subscriber's own row, constants including `self`, and field paths inside structured cells such as `new.target`. Comparison operands may include scalar arithmetic over constants and own fields, such as `0.3 * own.hp_max`; that cost is charged to the "live threshold" line of Section 4.

References to other instance rows are forbidden. That would be a join and would break the cost invariant. Materialize an entity when you need it.

```text
cond := cmp(new|old|own.f, expr)           # = != < <= > >=
      | new in [a,b] | new in {...}
      | changed
      | became(v)
      | crossed(t, up|down)
      | cond and cond | cond or cond
      | cond and not cond                  # negation only as a guard
```

The reason for the negation restriction is that "no write occurred" is not an event. An independent NOT trigger would drag the system back to per-frame polling. Guard-style `and not` still needs a positive trigger source, so sparsity is preserved. Timeout / silence semantics are expressed through clock cells.

### 3.4 delivery

```text
delivery := each  project(...)        # one calculation run per hit
          | batch project(...)        # one unordered frame batch
          | fold(sum|count|min|max)   # runtime-maintained incremental aggregate
```

`project` may project `new`, `old`, the writer instance id as a ref value, and fields from the subscriber's own row. Delivery is always a value snapshot, never a reference.

`fold` is the most important pushed-down form: "sum of all Enemy HP" is O(N) if scanned in calculation every frame, but `fold sum` turns it into per-write delta maintenance.

**Current limitations of `batch` / `fold`.**

- **batch**: delivery is unordered (D3). The runtime aggregates one batch per `(calculation, subscriber)` per frame and delivers it once. The `Canonical` tier (C4) sorts by `(writer type, id, generation, field)`. `batch` cannot currently be used with conjunction `&` (Section 3.2).
- **fold**: operators are the typed monoid set `{sum, count, min, max}`. Custom monoids are not exposed yet (open question three in Section 8). `min` and `max` are not invertible, so they are maintained with a multiset at `O(log n)` per write. `count` counts distinct contributing cells; `sum` ignores non-numeric writes. Member death, which has no write, is revoked out of band by the runtime and re-delivered as a shrunk aggregate in the next frame (Section 6.3). `fold` also cannot currently be used with conjunction `&`.

### 3.5 Admission Rule for Predicate Vocabulary

A primitive may enter the predicate layer if and only if there is an index structure that can be built at registration time and keep its amortized check inside the Section 4 cost budget. The vocabulary is generated by the cost model, not by enumerating use cases. Expression power that cannot bind to an index must either be materialized as an entity or stay inside calculation.

**Admission classes.** By the registration-time index they can bind to, predicate primitives fall into three outcomes:

- **indexed**: the primitive binds to a registration-time index and stays amortized `O(1)` or `O(log s)`, so it is a first-class fast path. Examples: hash subscription chains for `own` / `inst`; value buckets for type scope + equality (`= constant`, `became`, `in {...}`, and `new.path = self` ref point lookups); shared sorted threshold tables / interval queries for type scope + constant thresholds or `crossed`.
- **guard**: the primitive can only guard a positive trigger source and cannot trigger on its own. Negation (`and not`) is in this class. "No write happened" is not an event, so independent NOT would drag the system back to per-frame polling. The left conjunction branch must still be a positive trigger, whether indexed or scan-degenerate.
- **scan-degenerate**: the primitive is admitted but degenerates into scanning the subscribers of that cell, `O(subscribers on that cell)`. Live thresholds, whose conditions reference subscriber own fields or `self`, and compound conditions that cannot bind to an index are in this class. The cost still does not depend on global scale, which is the honest degradation clause in Section 4.

---

## 4. Cost Model and Index Binding

Every write must be seen at least once by routing, and every true trigger must be touched at least once for delivery, so the lower bound is `Omega(|W| + |F|)`, where `W` is the frame write set and `F` is the actual trigger set.

**Cost invariant**: total frame scheduling cost is `O(|W|*log + |F|)`, independent of total predicate count, instance count, and data size.

| Primitive | Runtime structure | Amortized cost per write | Class (Section 3.5) |
|---|---|---|---|
| own / inst scope | `(type, id, field) -> subscription-chain hash` | `O(1) + triggers` | indexed |
| type scope + constant threshold | shared sorted threshold table / interval tree for the cell | `O(log s + k)` | indexed |
| type scope + equality | value -> bucket | `O(1) + k` | indexed |
| condition with own fields (live threshold) | point lookup per subscriber | `O(subscribers on that cell)` | scan-degenerate |
| changed / became / crossed | double-buffered old value | `O(1)` | indexed(1) |
| `&` conjunction (only `each`; gate, not join; <=31 branches; Section 3.2) | per-predicate frame bitset / count latch | `O(1)` per clause | indexed |
| `and not` guard | reuses the positive trigger check on the left branch | `O(1)` | guard |
| batch | predicate-private frame buffer append; unordered by D3 | `O(1)` | indexed |
| fold sum / count | delta maintenance | `O(1)` | indexed |
| fold min / max | heap or lazy recomputation | `O(log n)` | indexed |

`s` is the number of constant-condition subscribers on that cell; `k` is the number of actual hits. (1) `became` maps to value buckets and `crossed` maps to threshold tables, so both are indexed. Bare `changed` with a pure type scope has no constant to bucket by and falls back to scan-degenerate; under `own` / `inst` scope, the subscriber is the writer, so it remains indexed.

**Honest degradation clause**: when a condition parameter references fields on the subscriber itself, the threshold is live and cannot be merged into a shared index. The cost degrades to the number of subscribers on that cell, still independent of global scale. If one cell is subscribed to by massive personalized conditions, flip the design and materialize the intermediate value as an entity.

---

## 5. Registration-Time Compilation

Registration and unregistration take effect at frame boundaries. The compilation pipeline is:

condition normalization -> equivalent predicate merging -> constant-threshold / equality clustering into shared indexes -> single-writer conflict check (D1) -> negation guard validation -> optional implication reuse.

Because predicates are data, all of this is done once at registration time. Runtime routing performs no decision-making beyond interpreted evaluation of the compiled structures.

---

## 6. Three Things Folded Into the Four Layers

### 6.1 Joins, Spatial Queries, Global Ordering -> Materialized Entities

These do not enter the predicate layer. The standard pattern is to build a singleton **index entity**, batch-subscribe to raw writes, maintain the index incrementally in its calculation, and write query results or index slices into its own fields. Downstream users subscribe to those ordinary fields.

**Index as entity, view as data.** This keeps the predicate vocabulary closed while still making the optimal incremental-maintenance path available to user code.

**Helper: `spatial`.** This repository provides `SpatialGrid` in `src/spatial.rs`, a uniform-grid helper for broad phase, area-of-interest, and range queries, as an implementation of the materialized-index pattern above. It is calculation-private incremental state held by an index entity's calculation closure: an implementation detail of Turing-complete code, not a core layer, not a predicate primitive, and not a fifth concept. It embodies the principle that costly optimizations expose interfaces but do not decide policy for the developer. A uniform grid is only a common sweet spot for moving objects and local-neighbor interaction; if a use case needs a hierarchical grid, BVH, or sweep-and-prune, it can swap the implementation behind the same query interface.

### 6.2 Clock and Per-Frame Logic -> runtime as Writer

Each frame, the runtime writes the frame number into built-in cell `Clock.frame`. Subscribing to it is equivalent to polling: the cost exists, but it is explicit, visible, and paid by the subscriber. This is the only legal path for "must run every frame" logic. Everything else remains change-driven.

The intended timer semantic is a `Clock` alarm mechanism backed by a timer wheel, but its exact interface remains an open question.

### 6.3 Lifecycle, refs, and id Reuse

**ref is a cell type known to the runtime.** The runtime maintains reverse tables from a target instance to ref cells that point to it and `inst` predicates scoped through it.

**Creation**: the runtime writes built-in cell `_alive = true` for a new instance. Observers use `type(E, _alive) where became(true)` to see birth.

**Destruction**: the only entry point is writing false to the instance's own `_alive` cell. Because of write locality, killing another instance must travel through a dataflow request. An external destroy API, if provided, is semantically equivalent to the runtime writing `_alive = false` on behalf of the instance.

**Settlement at frame boundary**: the runtime removes `inst` subscriptions through the dead instance and writes null to all ref cells that pointed to it. These are ordinary writes, so holders can collect corpses with `became(null)` next frame. Ids may then be reused; hidden generations prevent ABA.

Death therefore propagates through ordinary dataflow. There is no special broadcast mechanism.

---

## 7. Examples

```text
# 1 HP crosses below 30%: edge-triggered
on    own(hp)
where crossed(0.3 * own.hp_max, down)
each  deliver(new, old)
-> flee_calc

# 2 Attack: write yourself, target sniffs it
on    type(Attacker, attack_out)
where new.target = self
each  deliver(new.dmg)
-> take_damage_calc

# 3 Spatial grid: index as entity on Grid.0
on    type(Unit, position)
batch deliver(writer_id, old, new)
-> grid_update_calc

# 4 Boss HP bar: incremental aggregate
on    type(Enemy, hp)
fold  sum
-> boss_bar_calc

# 5 Target death detection: collect ref invalidation
on    own(target)
where became(null)
each
-> retarget_calc
```

---

## 8. Invariants and Open Questions

**Invariants**: the four layers are closed for business mechanisms. Derived consumer runtimes and materialized-index helpers reuse the four layers and are not fifth concepts (Sections 0 and 9). The only trigger source is previous-frame writes. Write locality plus single writer (D1). Writes are events (D2). Delivery is unordered (D3), so consumers must be order-independent. Effect confinement (D4): calculations have no side effects outside `ctx`, which is a precondition for reordering and parallel execution. Snapshot reads: current-frame writes are invisible to the current frame. Predicates are data fixed at registration time and compilable. The cost invariant is `O(|W|*log + |F|)`, independent of predicate and instance counts.

**Open questions**: first, the shape of the `Clock` alarm interface and timer-wheel integration. Second, whether `project` may perform projection-side arithmetic; condition-side scalar arithmetic is already allowed (Section 3.3). Third, whether `fold` exposes custom monoids; if it does, reversibility or recomputation strategy must be specified, with `min` / `max` already serving as non-invertible precedents. Fourth, the detection cost and severity for "one calculation writes different values to the same field through multiple runs in one frame" (Section 2): silent folding, warning, or error. Fifth, the semantics of using conjunction `&` with `batch` / `fold`: under same-frame gating, what is aggregated and how many times? The current implementation rejects this at registration time (Section 3.2).

---

## 9. Derived Consumer Runtime (render)

The four layers are the simulation core. **`render` is a second runtime built above it: a derived consumer runtime with a dynamic frame rate.** It is not part of the core, but it reuses the same four-layer closure and is not a fifth concept.

**Design theorem.** `render = simulation - lifecycle + dynamic clock + sim write-log ingestion + interpolation primitives`. Render logic is also built from predicate + calculation. The differences are trigger sources, lifecycle ownership, and dependency direction.

**Two trigger sources, dual to sim's "only trigger source."**

- render clock tick -> **continuous update**: on every render frame, run over each present instance for camera damping, derived transforms, interpolation, and similar work. This is a dense ECS scan and the main hot path of render, because interpolation is inherently per-frame.
- ingested sim write log -> **event reaction**: reuse the predicate algebra, including the same precompiled condition evaluator, while subscribing to the sim write stream.

After sim enables ingestion with `Runtime::enable_render_feed()`, each frame retains the routed write set: the same write stream that Section 0 calls the only trigger source, including birth writes from external spawn. Render consumes it through `Runtime::committed_writes()`.

**One-way dependency rule.** Render writes only render namespace fields (`RFieldId`) and reads sim fields through tracked mirrors. **Sim never reads render.** This structurally enforces concurrency decoupling and is an expression of field-level D1: render extension writers exclusively claim render fields through `claim_writes` (Section 0.1).

**Shared lifecycle is owned by sim.** Render has no entry point for spawning or destroying shared entities; its sidecar rows passively follow sim birth and death deltas. If death fade-out is configured, render only delays reclaiming its own sidecar row by advancing a fade weight from 1 to 0 using real `dt`; it does not change the sim fact of life or death.

**Interpolation is the render dual of fold.** `track(sim_field, Interp)` is maintained automatically by the render runtime each frame as `(prev, cur)` and evaluated by alpha. Like `fold`, it is declarative pushdown, not hand-written calculation logic.

**Culling / LOD.** Render maintains its own `SpatialGrid`, fed by tracked position deltas ingested from sim, and performs render-rate camera queries to produce a visible set. Continuous update and submission are narrowed to the viewport. This is the render dual of Section 6.1's "materialize as index entity" pattern: it remains a composition of the registered pieces above, not a fourth registration concept. When disabled, behavior is byte-for-byte equivalent to no culling.

**Submission seam.** `RenderRuntime::submit()` returns ordered semantic `RenderPacket`s — the principled "good upstream" handoff: interpolated transform, mesh / material handles, animation state, and fade weight, one per visible entity. For backends that prefer fixed-shape data, `SubmissionView::instance_stream()` derives the same order, without reordering, into `SubmissionInstanceRow`s — a stable 64-byte little-endian encoding plus a row-major 3×4 affine helper — and contiguous same-key `SubmissionInstanceSpan`s; each row carries its `packet_index` back to the semantic packet and `InstanceId`. This is deliberately not a driver API: it decides no buffer residency, draw sorting, or backend object model. Those remain "costly" optimizations to be handed an interface, not decided for the developer.
