# 03 In-Frame Aggregation: Many Attackers, Many Sources

**Language:** English | [中文](../docs-zh/03-frame-aggregation.md)

## Problem

"In the same frame, five attackers hit one unit and two healers heal it. The final HP must be correct."

## Why It Is Tricky

The intuitive approach copies the Section 7 attack example with `each`: for each hit, run `hp = own.hp - dmg`. That is wrong. Under D3 consequence 1, if the same calculation runs multiple times in one frame, every run reads the same snapshot. Five runs all see `hp = 100`, each writes `100 - dmg_i`, and write folding chooses an undefined final value. In-frame aggregation must use `batch` or `fold`; `each` read-modify-write accumulation is forbidden.

## Decomposition

- **entity** `Attacker`: writes `own(attack_out) = {target: ref, dmg: 5}`.
- **entity** `Healer`: writes `own(heal_out) = {target: ref, dmg: -3}`. Use the same payload shape; healing is negative damage.
- **calculation** `settle_hp_calc` on `Unit`: the only writer of `hp`. It receives the entire frame's deltas, sums them, and writes exactly once.

## Predicate Algebra

```text
on    type(Attacker, attack_out) | type(Healer, heal_out)
where new.target = self
batch deliver(new.dmg)
-> settle_hp_calc      # hp' = clamp(own.hp - sum(rows)); write own(hp) once
```

## Correctness Argument

- Summation is closed over multisets and independent of delivery order.
- The "damage then healing" order question disappears. There is no order, only one frame's net delta. If the business rule demands an in-frame order such as "damage before healing, dead units cannot be healed", that is a state-machine problem: handle death in the same `settle_hp_calc`, or split the behavior across frames.
- `fold sum` is not the right tool here. Fold aggregates current cell values by delta, such as total Enemy HP. This scenario sums event payloads grouped by `target = self`, so batch plus calculation-side summation is the matching tool.
- Clamp, crits, armor, and arbitrary business logic belong in calculation, where Turing-completeness lives.

## Cost

`new.target = self` uses an equality bucket: `O(1) + hits`. Batch append is `O(1)` per row. The settle calculation runs once per damaged target per frame and costs `O(number of hits for that target)`.
