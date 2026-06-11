# 17 Same-Frame Movement and Knockback Composition

**Language:** English | [中文](../docs-zh/17-knockback-compose.md)

## Problem

"In one frame, two explosions, a dash, a slow field, and platform movement all affect a unit. Root prevents active movement but forced movement still applies. Super armor resists knockback. If two hooks grab the same target in one frame, exactly one wins and losers receive receipts. The final movement must not clip through walls."

## Why It Is Tricky

Many systems want to write `position`, which D1 rejects. Same-frame movement ops arrive unordered, so "hook A after hook B" is undefined. Platform following is a dynamic subscription target. Root and super armor reject or scale movement by class. Collision depends on map geometry, which conditions cannot join.

## Decomposition

Normalize every movement source into a homomorphic op `{target, class, kind, vec, prio, frame, salt}` and converge them into one `mover_calc`.

- **entities** `Bomb`, `Hook`, and other movers emit op fields such as `kb_out` and `hook_out`.
- **entity** `Unit`: `position` and `grab_winner` belong to `mover_calc`; `dash_op`, `move_op`, `carry_op`, and `platform` are normalization/ref fields; root, resist, and slow are materialized from the buff ledger.
- **entity** `Platform`: owns its position.
- **entity** `Grid.0`: materializes solid-cell and carrier occupancy views.

## Predicate Algebra

```text
on    type(Bomb, kb_out) | type(Hook, hook_out)
      | own(dash_op) | own(move_op) | own(carry_op)
where new.target = self
      and not (cmp(new.class, =, "active") and cmp(new.frame, <, own.root_until))
batch deliver(new)
-> mover_calc

on    inst(hook_target, grab_winner)
each  deliver(new)
-> hook_result_calc

on    inst(platform, position)
where changed
each  deliver(new, old)
-> carry_calc

on    own(position)
where changed
each  deliver(new)
-> board_calc
```

Inside `mover_calc`, partition the multiset by class: exclusive hooks choose max `(prio, salt)`; otherwise forced movement sums and is scaled by knockback resistance; otherwise active movement chooses by a business key; ordinary movement sums with slow multipliers; carrier movement is always added. Collision reads the previous-frame solid view and clips the final vector before writing `position` once.

## Correctness Argument

- `position` has one writer. Adding teleport, wind, or more movement sources means emitting ops, not writing position.
- Partition, sum, and max-by-key are multiset functions and therefore D3-compliant.
- Root rejection belongs in routing because rejected active movement should be silent and cheap. Super-armor scaling belongs in calculation because even a zeroed knockback may still influence class priority.
- Hook receipts use the grabbed target as the arbitrator, just like same-frame resource contention.
- Platform following has honest latency. Rendering may smooth visually by attaching local coordinates, but simulation remains dataflow-correct.
- Collision uses the previous-frame geometry view; same-frame new walls block next frame, an explicit snapshot consequence.

## Cost

Movement op routing is equality-based. `mover_calc` runs at most once per unit per frame and costs `O(frame ops + crossed cells)`. Inst receipts and carrier following are `O(1) + triggers`.
