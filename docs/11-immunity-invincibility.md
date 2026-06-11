# 11 Immunity and Invincibility

**Language:** English | [中文](../docs-zh/11-immunity-invincibility.md)

## Problem

"After being hit, a unit is invincible for 30 frames. Super armor ignores knockback but still takes damage. Poison immunity is permanent. A boss is immune while a crystal is alive. Blocked attacks must display an 'Immune!' floating text."

## Why It Is Tricky

- Invincibility is saying "no" to a class of events for a time interval. Predicate rejection is cheap, but silent; no calculation wakes to show a receipt.
- Same-frame penetration: the hit that opens invulnerability and other hits in the same frame all see the previous invulnerability window.
- Dynamic immunity sets cannot be checked by `new.kind in own.immune_set` in predicate algebra.
- Crystal-granted immunity would read another instance row, which conditions cannot do.

## Decomposition

Use four techniques:

1. Time window as a timestamp guard.
2. Receipts through complementary predicates: accepted hits and blocked hits split into two guards.
3. Granted immunity mirrored into own fields through refs.
4. Dynamic sets handled inside calculation; small static sets can stay in predicate disjunctions.

- **entity** `Unit`: fields `immune_until`, `immune_all`, and `crystal`.
- `settle_calc` writes `hp` and `immune_until`.
- `immune_fx_calc` writes blocked-hit effects.
- `phase_calc` writes `immune_all` and `crystal`.

## Predicate Algebra

```text
on    type(Attacker, attack_out)
where new.target = self
      and cmp(new.frame, >=, own.immune_until)
      and not cmp(own.immune_all, =, true)
batch deliver(new)
-> settle_calc

on    type(Attacker, attack_out)
where new.target = self
      and (cmp(new.frame, <, own.immune_until) or cmp(own.immune_all, =, true))
batch deliver(new)
-> immune_fx_calc

on    own(crystal)
where became(null)
each
-> phase_calc     # write own(immune_all) = false
```

## Correctness Argument

- No per-frame cost: invulnerability does not poll. Each incoming hit pays one guard check.
- Same-frame penetration is a spec point: under batch semantics, invulnerability starts after net settlement of hits with the same timestamp. If "first hit starts window and discards the rest" is desired, sort the multiset by `(dmg, salt)` inside `settle_calc`.
- Complementary guards are auditable: one predicate catches accepted hits; the other catches blocked hits.
- Timestamp comparisons are closed because hit frames and `immune_until` are derived from the same clock source.
- Super armor and damage immunity are separate streams or kinds. Permanent poison immunity can be a constant guard.
- Crystal immunity is mirrored into `own.immune_all`; the boss does not join against crystal rows. Crystal destruction invalidates the ref, producing an ordinary `became(null)` event.

## Cost

Incoming-hit routing uses equality buckets. After routing, the live-threshold checks are point lookups over own fields. Immunity has no per-frame overhead; the blocked-hit effect cost is proportional to actually blocked hits.
