# 18 Casting, Channeling, and Interrupts

**Language:** English | [中文](../docs-zh/18-cast-interrupt.md)

## Problem

"A fireball casts for 90 frames and settles on completion. Taking 50 cumulative damage or being stunned interrupts immediately, movement cancels, the last 10 frames are uninterruptible, and interrupt refunds 70% mana. A channel ticks every 30 frames and keeps already-settled ticks when interrupted."

## Why It Is Tricky

Completion is a future event, not a write. A stale completion probe may still fire after an interrupt. Same-frame completion and interrupt are unordered. Tail uninterruptibility can be silent if guarded in predicates or receipt-capable if handled in calculation. Mana refund and casting state must be atomic. Cumulative damage is fan-in aggregation and must reset by cast generation.

## Decomposition

Use **generation guards**: every cast gets a sequence number; any terminating op invalidates the active number. Future ops carry a sequence and are dropped if stale. Heterogeneous inputs normalize into `{target, op, seq, frame, skill, cost}` and converge into one `cast_settle_calc`.

- **entity** `Caster`: `phase`, `cast_seq`, `due_at`, `unint_from`, `mana`, and `tick_out` belong to `cast_settle_calc`.
- `begin_op`, `cancel_op`, `hurt_op`, and `due_op` are normalization fields with single writers.
- External stun can either emit the same op shape directly or be materialized through event entities when source types grow.

## Predicate Algebra

```text
on    own(cast_req)
each  deliver(new)
-> begin_calc

on    own(move_req)
where cmp(own.phase, !=, "idle")
each  deliver(new)
-> cancel_calc

on    type(Attacker, attack_out)
where new.target = self and cmp(own.phase, !=, "idle")
batch deliver(new.dmg, new.frame)
-> accum_calc

on    type(Clock, frame)
where cmp(new, >=, own.due_at)
each  deliver(new)
-> probe_calc

on    own(begin_op) | own(cancel_op) | own(hurt_op) | own(due_op) | type(Attacker, stun_out)
where new.target = self
batch deliver(new)
-> cast_settle_calc
```

## Correctness Argument

- `cast_settle_calc` is a segmented multiset function: first drop stale seqs, then drop interrupts in the uninterruptible tail if that is the chosen spec, then process begin/due/interrupt segments in a fixed internal order.
- Generation guards are required. Without them, an old completion op can arrive during a new cast and complete it early.
- Same-frame due plus stun has no "arrival order." If the chosen spec is "tail due wins, otherwise interrupt wins," encode that as segment priority.
- Cumulative damage uses batch sum and one `dmg_accum` owner. Resetting by generation is local and deterministic.
- Refund and state transition happen in the same writer run, so there is no "paid but not started" or "interrupted but not refunded" frame.
- If mana lives in a shared pool entity, use a frame-to-frame escrow saga as in [19](19-atomic-consume.md).
- Channel ticks already emitted are ordinary writes and cannot be revoked; interrupt only stops future ticks.

## Cost

Own-chain routing and own-state guards are `O(1)`. Attack/stun routing uses equality buckets. The `Clock.frame` probe is a live threshold over casters and should become an alarm/timer-wheel entity at scale. Accumulation and settlement cost `O(frame ops)`.
