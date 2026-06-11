# 12 Limited Charges: Deathrattle, Ward Stacks, Nth Trigger

## Problem

"A deathrattle fires exactly once. A charm blocks three lethal hits and fails on the fourth. Every fifth attack is empowered. If five hits land in one frame while one charm charge remains, exactly one hit is blocked, not five and not zero."

## Why It Is Tricky

"Exactly N" is a counting invariant, and same-frame fan-in plus snapshot reads are its enemy. Each `each` run reads the same previous count, so all hits may think a charge remains. Edge guards solve cross-frame repetition, not same-frame fan-in. "The fifth hit" also implies an order that D3 does not provide.

## Decomposition

First, a free theorem: D1 plus write folding means any cell has at most one write per frame, so an own-scope edge guard is exactly once.

Fan-in streams require three pieces: a single-writer count field, `batch` to converge the frame into one run, and arbitration inside the multiset.

- **entity** `Unit`: `hp`, `charge`, `hit_count`, and `empower_next` all belong to `settle_calc`.
- **calculation** `deathrattle_calc`: watches own HP edge.

## Predicate Algebra

```text
on    own(hp)
where crossed(0, down)
each
-> deathrattle_calc

on    type(Attacker, attack_out)
where new.target = self
batch deliver(new)
-> settle_calc
```

Inside `settle_calc`: compute net damage; if lethal and `own.charge > 0`, consume one charge and leave `hp = 1`; update `hit_count`; if a multiple of five is crossed, write `empower_next` or select an empowered hit by a total-order key.

## Correctness Argument

- Charge is single-writer state. Each frame consumes at most the snapshot value and writes back once, so total consumption is bounded by N.
- `each` with guards is insufficient for fan-in because it gives "at most K times per frame", not "at most once."
- "One charge protects the whole frame" and "one charge protects one hit" are both valid D3-compliant specs. The latter sorts the multiset by a business key and simulates per-hit order inside the calculation.
- "Every fifth hit" must be specified as either frame-granularity crossing or registered for the next frame. A global nth hit is not defined without a chosen total order.
- Deathrattle does not repeat: `crossed` is an edge, and the instance destroys itself afterward.
- Shared team-wide charges are same-frame contention and should use the arbitration/receipt pattern from [02](02-same-frame-contention.md).

## Cost

Own-stream edge routing is `O(1)`. Fan-in equality routing is `O(1) + hits`; settlement costs `O(hits)` or `O(k log k)` if a deterministic per-hit order is simulated.
