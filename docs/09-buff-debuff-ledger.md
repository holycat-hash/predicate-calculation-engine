# 09 Buff/Debuff: Stacks, Refresh, Dispel, and Panels

**Language:** English | [中文](../docs-zh/09-buff-debuff-ledger.md)

## Problem

"A +20% attack buff lasts 300 frames. Reapplying the same kind refreshes duration and stacks up to a cap. Different kinds multiply. One dispel removes all magic debuffs. Panel attack is `(base + sum(additive)) * product(multipliers)`, and every add/remove must be reflected immediately."

## Why It Is Tricky

The intuitive definition is "someone continuously modifies someone else's attributes." Every word hits a constraint: the caster cannot write the target's fields, "continuous" cannot rely on free ticks, and many buffs cannot all write `atk` under D1. D3 also makes "apply then refresh" undefined when two same-kind buffs arrive in one frame.

## Decomposition

Flip the model: **a buff is not a modifier; it is a ledger credential**. The target owns a `buff_book`, and one `book_calc` owns the book plus derived panel fields. Apply, dispel, and expiry are normalized into one op shape.

- **entity** `Caster`: writes `own(buff_op_out) = {target, op, kind, add, mul, stacks, dur, tag, salt}`.
- **entity** `Unit`: fields `buff_book`, panel fields such as `atk_final`, `next_expire`, and `expiry_op`.
- **calculation** `expiry_probe_calc`: turns due time into an op, using the lease/Clock pattern.
- **calculation** `book_calc`: the only writer of book, panel, and next expiry.

## Predicate Algebra

```text
on    type(Clock, frame)
where cmp(new, >=, own.next_expire)
each  deliver(new)
-> expiry_probe_calc    # write own(expiry_op) = {target: self, op: "expire", frame: new}

on    type(Caster, buff_op_out) | own(expiry_op)
where new.target = self
batch deliver(new)
-> book_calc
```

## Correctness Argument

- D1 collapses "who changes my attack" into one ledger owner. Casters only emit ops.
- D3 compliance: `book_calc` merges a multiset of ops. Apply same kind: cap stack count and take max expiry. Dispel deletes by tag. Expire prunes by `until <= frame`. Same-frame apply/dispel ordering is a specification choice expressed as a segmented multiset function.
- Expiry can redundantly fire for one frame because of snapshot reads; pruning is idempotent, so the extra op becomes a no-op.
- Book and panel fields are written in the same calculation run, so there is no observable "new book, old panel" frame.
- Timestamps are all derived from the same `Clock` source and keep the same stable offset.

## Cost

Apply/dispel routes by `new.target = self`, so `O(1) + hits`. The expiry probe on `Clock.frame` is a live threshold and degrades to units that currently carry buffs; massive counts should be flipped into an `Alarm.0` timer-wheel entity. `book_calc` costs `O(frame ops + active buffs)`.

## Variants

- DoT: add `next_tick`, generalize `next_expire` to `next_due`, and emit damage ops when due.
- Behavioral buffs: materialize a buff entity for behavior, but still drive accounting through ops into the book.
- Many caster types: normalize through a `BuffApply` event entity.
- Auras: grid enter/leave events from [04](04-dynamic-subscription.md) become apply/dispel ops.
