# 08 Chain Reactions: Explosion Chains, Contagion, Dominoes

**Language:** English | [中文](../docs-zh/08-chain-reaction.md)

## Problem

"A barrel explodes, damages nearby barrels, and those barrels explode in turn until the chain burns out."

## Why It Is Tricky

A chain is a feedback loop: explosion -> damage -> explosion. Event-bus architectures often hit reentrancy, recursion depth, or in-frame infinite-loop failures here. In PCE, double buffering unfolds the feedback loop into frame-to-frame ping-pong, so there is no in-frame loop. The remaining issues are neighborhood lookup and proving termination.

## Decomposition

- **entity** `Barrel`: fields `hp`, `my_cell`, and `explosion_out`.
- **calculation** `explode_calc`: when HP crosses below zero, writes `own(explosion_out) = {cell, dmg}` and destroys itself.
- **calculation** `splash_calc` on `Cell`: receives explosions and writes aggregated splash for that cell.
- **calculation** `take_splash_calc` on `Barrel`: watches its current cell through `inst(my_cell, splash)`.

## Predicate Algebra

```text
on    own(hp)
where crossed(0, down)
each
-> explode_calc

on    type(Barrel, explosion_out)
where new.cell = self
batch deliver(new.dmg)
-> splash_calc      # write own(splash) = {dmg: sum, seq: own.seq + 1}

on    inst(my_cell, splash)
each  deliver(new.dmg)
-> take_splash_calc # write own(hp) = own.hp - new.dmg
```

## Correctness Argument

- Expansion rhythm: frame N barrel A crosses; N+1 writes explosion; N+2 neighbors take damage; N+3 neighbors cross and explode. Each hop is one frame.
- No repeated explosions: `crossed(0, down)` is an edge condition, and exploding barrels destroy themselves. In-flight deliveries are value snapshots and do not dangle.
- Termination: total live barrel count strictly decreases each time an explosion is produced, and an explosion only appears on the edge from nonnegative HP to negative HP. More generally, find a monotone measure consumed by the chain.
- Same-frame explosions in one cell are summed by `splash_calc`, following the aggregation discipline from [03](03-frame-aggregation.md).

## Variants

- Neighbor-cell splash: do not fan out to every cell with an unindexed condition. Let `Grid.0` materialize "explosion -> affected cells" as a view and write per-cell fields.
- Contagion: latency is a timestamp guard, while probability and immunity stay inside calculation.

## Cost

Each hop uses equality or `inst` routing: `O(1) + hits`. Total chain cost is `O(number of affected entities)`, independent of total barrels in the world.
