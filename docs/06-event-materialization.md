# 06 Event Materialization: Many Events in One Frame, One Event to Many Receivers

**Language:** English | [中文](../docs-zh/06-event-materialization.md)

## Problem

"A matcher creates many trade pairs in one frame, and both sides of each pair must be notified."

The same pattern appears in many explosions in one frame, bulk timer expiry, or one drop producing many loot items.

## Why It Is Tricky

Two walls block the direct approach:

1. **Write folding**: one cell keeps only one write per frame. If the matcher writes ten results to `own(match_result)`, only the last survives.
2. **Closed condition set**: even if all results are packed into a map, a receiver cannot express "self is somewhere in `new`" because field paths are static.

## Decomposition

An event is not a value; it is a birth. Spawn one entity per event. Each event has its own cells, so folding no longer collapses events. Receivers claim their event with ordinary equality conditions.

- **entity** `Matcher.0`: singleton; batch-receives applications.
- **entity** `Trade`: event entity with fields `members` and `ttl_seen`.
- **calculation** `match_calc`: for each matched pair, spawns `Trade` with `members = {a, b, price}`.
- **calculation** `on_trade_calc` on `Unit`: claims trades involving itself.
- **calculation** `reap_calc` on `Trade`: burns the event entity after it has been visible.

## Predicate Algebra

```text
on    type(Trade, members)
where new.a = self or new.b = self
each  deliver(new, writer_id)        # writer_id is the Trade ref
-> on_trade_calc

on    own(_alive)
where became(true)
each
-> mark_calc            # write own(ttl_seen) = true

on    own(ttl_seen)
where became(true)
each
-> reap_calc            # destroy_self()
```

## Correctness Argument

- One event instance means one independent set of cells, so write folding cannot swallow separate events.
- Claiming uses ordinary equality such as `new.a = self`, which is indexable. No new predicate primitive is needed.
- Lifetime ladder: spawn in frame N; claims and mark run in N+1; reap runs in N+2; frame-boundary settlement invalidates refs in N+3.
- If an event needs a receipt or handshake, both sides store the `writer_id` ref and continue through `inst(trade_ref, ...)`. The event entity naturally becomes the negotiation state host.

## Cost

Spawn is `O(1)` allocation plus initial writes. Claiming by equality is `O(1) + hits`. The per-event entity cost buys queue-like semantics inside the four-layer model. For extremely high-frequency tiny events, fall back to batch aggregation and let one receiver consume the whole batch.
