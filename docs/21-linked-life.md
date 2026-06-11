# 21 Linked Life and Damage Transfer

## Problem

"A bodyguard skill makes B take 50% of A's damage. If A and B guard each other, does damage bounce forever? Can transferred damage be transferred again? Total damage must be exactly conserved. If the bodyguard dies while damage is in flight, where does that half go?"

## Why It Is Tricky

Double buffering prevents in-frame loops, but frame-to-frame mirror loops may still continue. A reflected flag would stop recursion, but it loses conservation semantics. Integer percentage split has remainders. Transfer is cross-entity dataflow, so in-flight damage can evaporate if the target dies unless there is a positive acknowledgment or reap fallback. Direct hit, forwarded hit, ack, and reclaim all want to affect HP and pending ledgers, so they must converge into one writer.

## Decomposition

Use **integer-decay monotone termination + remainder-local conservation + ack/reap fallback**.

- **entity** `Unit`: `hp`, `fwd_out`, `ack_out`, `pending`, and `fwd_seq` belong to `settle_calc`.
- `guard` and `ratio` belong to link setup logic.
- `reclaim_op` belongs to a reap probe.
- Homomorphic op shape: `{target, kind, amount, salt, source}` for hit/fwd/ack/reclaim.

## Predicate Algebra

```text
on    own(hit_in) | type(Unit, fwd_out) | type(Unit, ack_out) | own(reclaim_op)
where new.target = self
batch deliver(new)
-> settle_calc

on    own(guard)
where became(null)
each
-> reclaim_probe_calc
```

Inside `settle_calc`, process ack first, then reclaim, then damage. For total damage `D`, forward `f = floor(D * ratio)` to the guard and keep `D - f` locally. Store forwarded amounts in `pending` until ack arrives.

## Correctness Argument

- Termination: for integer `D >= 1` and `0 <= ratio < 1`, `floor(D * ratio) < D`. Forwarded amount strictly decreases and is bounded below by zero.
- Conservation: `keep + forwarded = D` exactly. Remainders stay local and never evaporate.
- If the guard dies before delivery, routing drops the fwd event, but pending remains and reclaim applies it back to the source.
- If the guard dies in the same frame it settles the forwarded hit, ack and reclaim may arrive together; internal order ack before reclaim removes the ledger before fallback, avoiding double application.
- Forwarded damage can transfer again because fwd and hit are the same shape. If the spec forbids that, add hop count and make the cutoff keep the full remainder.
- Multiple same-frame sources are batched into one net damage before split.
- If many acks must be emitted in one frame, spawn Ack event entities to avoid write folding.

## Cost

All routes are equality-to-self. Settlement runs once per unit per frame and costs `O(frame ops)`. The mirror-chain length is `O(log initial_damage)` for ratio below one.

## Runnable Verification

`tests/linked_life.rs` verifies A->B->C conservation, mutual-guard mirror convergence, and full fallback when a bodyguard dies with pending transferred damage.
