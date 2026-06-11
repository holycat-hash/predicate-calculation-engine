# 07 Global Ordering / Top-K Leaderboard

## Problem

"Maintain a real-time server-wide Top-10 score board, and refresh UI when it changes."

## Why It Is Tricky

Global ordering is anti-sparse: any score change may affect the total order. The predicate layer intentionally excludes it. "Who is rank k" cannot bind to any per-write `O(1)` index, but the requirement is real.

## Decomposition

Use the Section 6.1 flip: **index as entity, view as data**. The ordering structure is state, and state belongs to an instance.

- **entity** `Board.0`: singleton; fields `rank_state` and `top10`.
- **calculation** `rank_calc`: incrementally maintains the ordered structure.
- **entity** `Hud` and other consumers: subscribe only to `top10`.

## Predicate Algebra

```text
on    type(Player, score)
batch deliver(writer_id, new, old)
-> rank_calc      # update rank_state by old -> new; write own(top10) if changed

on    type(Board, top10)
where changed
each  deliver(new)
-> hud_refresh_calc
```

## Correctness Argument

- D3 compliance: each batch row carries `(writer_id, old, new)`. The state update is "locate by id, remove old position, insert new position." D1 and write folding guarantee at most one row per player per frame.
- Snapshot read: `rank_calc` reads last frame's `rank_state`, applies this frame's batch of deltas, and writes the new state.
- Writing `top10` only when it really changes is fine; writing it every frame is also fine. Downstream users explicitly filter with `changed`.
- Tie stability belongs in the sorting key. As in resource contention, do not use entity id as an ordering key.

## Cost

Each `score` write routes in `O(1)` plus batch append. `rank_calc` costs `O(log N)` per row with an ordered structure. Total frame cost is `O(|W| log N)`, achieved entirely at user level without expanding predicate vocabulary.

## Variants

- Full ranking output: split the view into page fields (`page_0`, `page_1`, ...), and let consumers subscribe only to pages they need.
- "My rank": either let the player inspect a compact board view through `inst`, or broadcast rank-crossing events only when thresholds are crossed through [event materialization](06-event-materialization.md).
