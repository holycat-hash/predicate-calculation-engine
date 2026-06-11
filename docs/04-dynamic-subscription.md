# 04 Dynamic Subscription Targets and Spatial Neighborhoods

**Language:** English | [中文](../docs-zh/04-dynamic-subscription.md)

## Problem

"A melee unit only cares about occupancy changes in the grid cell it currently stands in. Units move between cells."

## Why It Is Tricky

Predicates are registration-time data: fields and types inside a scope are static. The "cell I care about" is a runtime variable. Subscribing to one huge `Grid.0` occupancy field and checking `changed` is fake sparsity. Dynamically subscribing to field `(x,y)` is not allowed.

## Decomposition

The key is `inst(ref, field)`: the subscription target follows the value of a ref field. A ref is a cell, and a calculation can rewrite its own ref cell. Rewriting the ref redirects the subscription without changing registrations.

- **entity** `Cell`: one instance per grid cell; field `occupants`.
- **entity** `Grid.0`: singleton index entity; field `cell_table` maps coordinates to `Cell` refs.
- **entity** `Unit`: fields `position` and `my_cell`.
- **calculation** `locate_calc` on `Unit`: on position change, snapshot-read `Grid.0.cell_table` and write `own(my_cell)` to the new cell ref.
- **calculation** `occupancy_calc` on `Cell`: maintains the cell's occupants.
- **calculation** `react_calc` on `Unit`: reacts to occupancy changes in its current cell.

## Predicate Algebra

```text
# 1 Move: position -> my_cell.
on    own(position)
where changed
each  deliver(new)
-> locate_calc

# 2 Occupancy maintenance. Enter if new = self, leave if old = self.
on    type(Unit, my_cell)
where cmp(new, =, self) or cmp(old, =, self)
batch deliver(writer_id, new, old)
-> occupancy_calc

# 3 Neighborhood sensing: target is the current my_cell value.
on    inst(my_cell, occupants)
each  deliver(new)
-> react_calc
```

## Correctness Argument

- Predicate 3 fixes only the shape "through `my_cell`, watch `occupants`." The actual target instance is data maintained by the runtime ref reverse table.
- Predicate 2 is set add/remove maintenance, a multiset function and therefore D3-compliant.
- Propagation is honest: frame N writes position -> N+1 writes `my_cell` -> N+2 writes `occupants` -> N+3 occupants are observed.
- If a `Cell` is destroyed, the runtime writes all `my_cell` refs to null, and holders can collect that with `became(null)`.

## Cost

Predicate 2 is an equality-to-self bucket; predicate 3 is an `inst` hash chain. Compared with scanning neighborhoods every frame, this changes the cost from `O(N*M)` to `O(number of movement writes)`. Radius queries follow the same flip: materialize query results as fields on an index entity, then subscribe to those result fields.
