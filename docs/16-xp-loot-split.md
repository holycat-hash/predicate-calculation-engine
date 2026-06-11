# 16 Same-Frame XP and Loot Allocation

## Problem

"A monster dies: 100 XP must be integer-split among three members without loss or duplication. A rare drop opens a roll window; the highest roll wins, late rolls are invalid. Common loot uses a round-robin cursor; three same-frame drops must advance the cursor by three. The whole flow must be auditable."

## Why It Is Tricky

- Integer division has a remainder. "Give the remainder to the first" is undefined under D3 unless "first" is a business total order.
- The distributor cannot write member XP, and one cell cannot emit many grants in one frame. Event materialization is needed.
- Roll-window close is a future/absence event, so it needs an alarm/lease pattern.
- Round-robin cursor increments from multiple same-frame drops must be one batch function, not many writes.
- Audit crosses entities and must be decomposed into conservation of the split function plus exactly-once delivery.

## Decomposition

Use **integer conservation split + event-entity fan-out + window guard arbitration + batch cursor advancement**.

- **entity** `Party`: roster, `rr_cursor`, and `kill_in`.
- **entity** `Award` / `Grant`: event entities carrying `{target, amount/item}`.
- **entity** `Loot`: arbitrates rolls through `rolls`, `closed`, and `winner`.
- **entity** `Member`: receives XP/items and emits `roll_out`.

## Predicate Algebra

```text
on    own(kill_in)
batch deliver(new)
-> split_calc       # total, per, remainder; spawn Award per member

on    type(Award, grant)
where new.target = self
batch deliver(new.amount)
-> xp_recv_calc

on    type(Member, roll_out)
where new.loot = self and cmp(own.closed, =, false)
batch deliver(new)
-> collect_calc

on    type(Clock, alarm)
where new.loot = self
each
-> award_calc       # close; winner = max by (roll, salt); spawn Grant

on    type(Corpse, drop_out)
where new.party = self
batch deliver(new)
-> rr_assign_calc   # sort by salt; assign from cursor; write cursor + k
```

## Correctness Argument

- `per * n + rem = total` is an integer identity. Assigning the first `rem` slots by roster order is deterministic and delivery-order independent.
- Award entities each produce one grant write. Receivers batch-sum same-frame grants, avoiding `each` read-modify-write.
- If an Award target dies before delivery, the grant may evaporate. Strict conservation requires an ack/reap reclaim pattern as in [21](21-linked-life.md).
- The roll close boundary is explicit: rolls written before the alarm frame are in the snapshot; same-frame alarm rolls are not.
- Round-robin advancement is a pure function of a sorted multiset and writes `rr_cursor` once.
- Audit compares single-writer ledgers on both sides; no global lock is needed.

## Cost

Split costs `O(roster size)`. Grants spawn in proportion to recipients. Roll collection costs `O(frame rolls)`, close costs `O(participants)`, and round-robin assignment costs `O(k log k)`.

## Runnable Verification

`tests/xp_loot_split.rs` verifies 100/3 as 34/33/33, same-frame multi-drop cursor advancement, and late-roll rejection after close.
