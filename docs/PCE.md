# PredicateCalculationEngine

**Language:** English | [中文](../docs-zh/PCE文档.md)

---

## 0. General Rules

This architecture has exactly four abstraction layers: **runtime, entity, calculation, predicate**. Every new requirement must fold into these four layers. No fifth concept may be introduced. Everything that follows, including clock, lifecycle, spatial indexes, and cross-entity interaction, is a consequence of those four layers rather than an extension.

The system is purely data-driven. The only trigger source is "writes from the previous frame." There is no polling, message system, or event bus; all of them are unified as writes to a cell. A **cell** is one field of one entity instance and is the smallest unit of data, write, and subscription.

The predicate layer is a closed algebra. A predicate is a declaration-shaped structure fixed at registration time, expressed as data / AST rather than arbitrary functions. All Turing-completeness stays in calculation. This restriction is not a style preference; it is the source of the performance guarantee in Sections 3.5 and 4.

### 0.1 Decision Record

**D1 Single writer**: every field statically belongs to exactly one calculation. Ownership conflicts are errors at registration time.

**D2 Writes are events**: every write produces an event, whether or not the value changes. "The value really changed" is expressed explicitly with `changed`.

**D3 Batches are unordered**: batch delivery order is undefined. The runtime may deliver in any order, such as shard order or arrival order, and the routing stage performs no sorting or ordering barrier.

In addition, Section 1.4's "single predicate per calculation" rule and Section 2's snapshot-read / write-folding model are fixed consequences of D1-D3 and write locality. They are important acceptance points for the design.

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

**Single writer (D1)**: each field statically belongs to exactly one calculation. Registration checks this and rejects conflicts. That makes `new` unambiguous and lets execution run in parallel without arbitration.

**Snapshot reads**: any field read by a calculation, including its own, is the value committed in the previous frame. Writes in the current frame are invisible to the current frame.

Calculation code itself may be arbitrary Turing-complete code.

### 1.4 predicate

A predicate precedes a calculation and has the shape `(scope, condition, delivery)`, detailed in Section 3.

**Single predicate per calculation**: each calculation has exactly one preceding predicate. If multiple predicates with different delivery shapes were allowed to feed one calculation, the input signature would no longer be single. Composition still has paths inside the four layers: use scope union (`|`) for "any source", conjunction (`&`) for same-frame co-occurrence, and materialize aggregates first when a trigger stream needs an aggregate value.

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

### 3.5 Admission Rule for Predicate Vocabulary

A primitive may enter the predicate layer if and only if there is an index structure that can be built at registration time and keep its amortized check inside the Section 4 cost budget. The vocabulary is generated by the cost model, not by enumerating use cases. Expression power that cannot bind to an index must either be materialized as an entity or stay inside calculation.

---

## 4. Cost Model and Index Binding

Every write must be seen at least once by routing, and every true trigger must be touched at least once for delivery, so the lower bound is `Omega(|W| + |F|)`, where `W` is the frame write set and `F` is the actual trigger set.

**Cost invariant**: total frame scheduling cost is `O(|W|*log + |F|)`, independent of total predicate count, instance count, and data size.

| Primitive | Runtime structure | Amortized cost per write |
|---|---|---|
| own / inst scope | `(type, id, field) -> subscription-chain hash` | `O(1) + triggers` |
| type scope + constant threshold | shared sorted threshold table / interval tree for the cell | `O(log s + k)` |
| type scope + equality | value -> bucket | `O(1) + k` |
| condition with own fields (live threshold) | point lookup per subscriber | `O(subscribers on that cell)` |
| changed / became / crossed | double-buffered old value | `O(1)` |
| `&` conjunction | per-predicate frame bitset / count latch | `O(1)` per clause |
| batch | predicate-private frame buffer append | `O(1)` |
| fold sum / count | delta maintenance | `O(1)` |
| fold min / max | heap or lazy recomputation | `O(log n)` |

`s` is the number of constant-condition subscribers on that cell; `k` is the number of actual hits.

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

**Invariants**: the four layers are closed; the only trigger source is previous-frame writes; write locality plus single writer (D1); writes are events (D2); snapshot reads; predicates are data fixed at registration time and compilable; the cost invariant is `O(|W|*log + |F|)` independent of predicate and instance counts; delivery is unordered (D3), so consumers must be order-independent.

**Open questions**: the shape of the `Clock` alarm interface and timer-wheel integration; whether `project` may perform projection-side arithmetic; whether `fold` exposes custom monoids and what reversibility or recomputation strategy would be required; and how expensive detection of "one calculation writes different values to the same field through multiple runs in one frame" should be, plus whether it is silent folding, warning, or error.
