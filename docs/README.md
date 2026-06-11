# Method Collection for Tricky Logic

**Language:** English | [中文](../docs-zh/README.md)

This directory collects implementation patterns for tricky logic that can be expressed using only **predicate + calculation + entity decomposition**.
The rule is the same as Section 0 of [PCE.md](PCE.md): the four layers are closed, every requirement must fold into them, and no fifth concept is introduced.

Each document uses the same shape: **Problem -> Why It Is Tricky -> Decomposition -> Predicate Algebra -> Correctness Argument -> Cost**.
Predicates use the DSL style from Section 7 of the architecture guide.

## Index

| No. | Problem | Core Technique |
|---|---|---|
| [01](01-absence-timeout.md) | Timeout / N silent frames / heartbeat drop | Absence is not an event -> timestamp lease + explicit Clock polling |
| [02](02-same-frame-contention.md) | Multiple actors racing for one resource in the same frame | Batch arbitration + inst-ref receipt channel |
| [03](03-frame-aggregation.md) | In-frame aggregation, such as many attackers combining damage | Forbid each read-modify-write -> batch sum / scope union |
| [04](04-dynamic-subscription.md) | Dynamic subscription targets / spatial neighborhoods | Cell entities + ref redirection |
| [05](05-cooldown-state-machine.md) | Cooldowns, state machines, illegal transition blocking | Event frame stamps + own-field guards |
| [06](06-event-materialization.md) | Many events in one frame / one event to many receivers | Materialize events as entities; spawn is broadcast |
| [07](07-global-order-topk.md) | Global ordering / Top-K leaderboard | Index as entity, view as data |
| [08](08-chain-reaction.md) | Chain reactions such as explosion chains and contagion | Frame-to-frame ping-pong + monotone termination measure |
| [09](09-buff-debuff-ledger.md) | Buff/debuff stacks, refresh, dispel, and panels | Buff as credential -> homomorphic ops + single-writer ledger |
| [10](10-damage-pipeline.md) | Damage formulas, shields, reflection | Split formulas by data ownership + multiset segmented netting |
| [11](11-immunity-invincibility.md) | I-frames, immunity, and blocked-hit receipts | Timestamp guards for zero-trigger rejection + complementary guard split |
| [12](12-limited-charges.md) | Exactly N uses, Nth trigger, deathrattle | Own-stream edges are exactly once; fan-in uses batch + counting |
| [13](13-aggro-taunt.md) | Aggro and taunt table | Normalize heterogeneous ops + lazy decay + lexicographic taunt priority |
| [14](14-combo-cancel-buffer.md) | Combo windows / cancel frames / input buffering | Window endpoints as fields + monotone intent seq + paid driver polling |
| [15](15-summon-attribution.md) | Summon ownership and kill attribution | Flatten root ownership + victim-side attribution collapse |
| [16](16-xp-loot-split.md) | Same-frame XP / loot allocation | Integer remainder by total order + roll-window arbitration + batch cursor advancement |
| [17](17-knockback-compose.md) | Same-frame movement and knockback composition | Normalize movement ops + one mover atomically decides the landing point |
| [18](18-cast-interrupt.md) | Casting, channeling, and interrupts | Cast generations + stale delivery invalidated at consumption |
| [19](19-atomic-consume.md) | Atomic consume and double-spend | Single-writer batch arbitration + escrow frame-to-frame saga |
| [20](20-projectile.md) | Projectiles: flight, pierce, hit dedupe | Parametric lazy trajectory + sweep candidates sorted by total order |
| [21](21-linked-life.md) | Linked life / damage transfer | Integer-decay monotone termination + remainder conservation + ack/reap fallback |
| [22](22-time-dilation.md) | Hitstop / bullet time / local time dilation | Single-writer local time axis + timestamp guards on that axis + freeze as stopped axis |
| [23](23-symmetric-trade.md) | Two-player trade / symmetric atomic exchange | Session entity batch arbitration + offer generation invalidation + bilateral escrow |

## Common Techniques Quick Reference

1. **Materialize as entity**: joins, spatial queries, global ordering, and mass personalized thresholds should become index entities. Subscribe to raw writes in batch, maintain the view incrementally, and write the view into the entity's own fields.
2. **Event materialization**: if one cell would need to emit many events in one frame, or many receivers need individual payloads, spawn one entity per event. Its birth (`_alive became(true)`) is the broadcast.
3. **Receipts through inst refs**: a condition cannot read another instance row, but a ref held by the subscriber can precisely watch `inst(ref, field)`.
4. **Timestamp guards**: do not poll for "not before" or cooldown windows. Stamp writes with a `Clock.frame` snapshot and let the receiver guard with comparisons such as `cmp(new.frame, >=, own.xxx_until)`.
5. **Absence through leases**: "did not happen" cannot be subscribed to. Maintain `lease_until` with positive events, then let the only legal poller (`Clock.frame`) or a centralized Alarm entity detect expiry.
6. **Flip the design signal**: if one cell has massive personalized-condition subscriptions, materialize the intermediate value as an entity instead of trying to invent a cleverer index.
7. **Homomorphic op merge**: all branches of a scope union must deliver the same payload shape. Normalize heterogeneous sources into one op shape before merging.
8. **Silent rejection vs. receipt rejection**: predicate guards are free but silent. For feedback, split complementary guards into settlement and receipt predicates, or move rejection into calculation.
9. **Own stream exactly once**: D1 + write folding means a cell has at most one write per frame, so an own-stream edge guard is exactly once. Fan-in "exactly N" needs a single-writer counter, batch, and multiset arbitration.
10. **Total-order replay inside calc**: D3 only says delivery order is undefined. A calculation may sort its multiset by a business total-order key to recover deterministic per-item semantics.
11. **Paid polling fold-in**: if an entity already wakes every frame for animation or physics, fold window checks, expiry checks, and intent consumption into that driver. Predicate-edge/alarm machinery is for entities without a paid driver.
12. **Local time axis**: under time scaling or freeze, all "N frames" semantics should hang off the entity's own monotone `local_time`: the timekeeper advances it, freeze stops it, and timestamp guards only change axis, not shape.

## Verification Status

These documents were written before the full runtime, but all predicates have been checked against the algebra expressible in `src/predicate.rs`.
Scenarios 01-23 all have executable Rust integration tests under `tests/`; run the full regression set from the repository root with:

```powershell
cargo test
```
