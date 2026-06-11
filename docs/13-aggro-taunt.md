# 13 Aggro and Taunt Table

## Problem

"Each enemy maintains an aggro table toward players. Damage adds 1 hate per point. Healing adds 0.5 times the heal amount to all enemies fighting the healed target. Taunt forces top priority for 120 frames. Hate decays 5% per second. Target switching requires the challenger to exceed the current target by 110%. Dead or disengaged targets are cleared and a new target is selected."

## Why It Is Tricky

- Five sources want to update `hate_book`: damage, healing, taunt, decay, and clear. D1 rejects that unless they converge.
- Healing is a dynamic broadcast to "all enemies engaged with the healed target"; this is a join-like membership query.
- Decay has no event. Per-frame table rewriting is explicit polling multiplied by table size.
- Taunt is a sort override with expiry, and expiry is absence.
- Sticky 110% switching compares values that are both decaying.

## Decomposition

Use three flips:

1. **The table is a ledger**: all heterogeneous sources normalize into one op stream handled by one book calculation.
2. **Decay is lazy**: store original hate and a timestamp; interpret current hate as `hate * decay_factor(delta)` when reading. Time changes interpretation, not data.
3. **Taunt is lexicographic priority**: key is `(taunt_active, hate, salt)`, where taunt activity is a timestamp guard.

- **entity** `Player`: emits `attack_out`, `heal_out`, and `taunt_out` with a shared shape `{kind, target, amount, frame, salt}`.
- **entity** `Enemy`: `hate_book`, `book_stamp`, `current_target`, `next_due`, and `lease_until` belong to `book_calc`. `due_op` and `dead_op` are normalization fields.

## Predicate Algebra

```text
on    type(Player, attack_out) | type(Player, heal_out) | type(Player, taunt_out)
      | own(due_op) | own(dead_op)
where cmp(new.target, =, self) or cmp(new.kind, =, "heal")
batch deliver(new, writer_id)
-> book_calc

on    type(Clock, frame)
where cmp(new, >=, own.next_due)
each  deliver(new)
-> due_probe_calc

on    type(Player, _alive)
where became(false)
batch deliver(writer_id)
-> dead_relay_calc
```

## Correctness Argument

- `book_calc` is a multiset function: damage/heal sums by writer, taunt takes max expiry, dead clear unions sets, due is idempotent.
- Same-frame "taunt then death" is a segmented multiset spec, not delivery order.
- With a shared decay stamp, all rows are multiplied by the same positive factor, so table ordering and 110% comparisons can be performed on stored values. Absolute thresholds such as disengage need decay-to-threshold time and can be scheduled through `next_due`.
- Stickiness does not oscillate: target switching is single-writer and runs at most once per frame; between ops, decay preserves ratios and does not wake the calculation.
- Healing fan-out is an honest degradation if implemented as "all enemies receive heal ops then filter." For massive enemy counts, materialize encounter membership as entities and route healing by encounter ref.
- Death clears arbitrary rows and then reselects target in the same calculation, so there is no frame with a dangling chosen target.

## Cost

Damage and taunt route by `new.target = self`: `O(1) + hits`. Naive healing fan-out costs `O(number of enemies subscribed to healing)` per heal and should be flipped to `Encounter` entities when locality matters. Lazy decay has zero per-frame table cost. Book updates cost `O(frame ops + table rows)`. Due probing on `Clock.frame` is a live threshold and can be centralized with an alarm entity.
