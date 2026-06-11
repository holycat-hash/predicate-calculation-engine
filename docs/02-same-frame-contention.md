# 02 Same-Frame Contention for a Unique Resource

## Problem

"N units request to pick up the same item in the same frame. Exactly one succeeds, and every requester must learn the result."

## Why It Is Tricky

- Write locality: nobody except the item's own calculation can write `Item.owner`, so the winner cannot be "notified" by directly writing requesters.
- D3: applications arrive in a batch with undefined order. "First come first served" is not expressible; arbitration must be an order-independent function of the request multiset.
- Entity ids carry no ordering semantics and cannot be used as tie-breakers.

## Decomposition

- **entity** `Unit`: field `claim` containing item ref plus priority, and `want` as a ref to the desired `Item`. `want` is the receipt anchor.
- **entity** `Item`: field `owner`. The item itself is the arbitrator; no third-party entity is needed.
- **calculation** `claim_calc` on `Unit`: writes `own(claim) = {item, prio, salt}` and `own(want) = item_ref`.
- **calculation** `grant_calc` on `Item`: receives frame-batched claims, arbitrates, writes `own(owner)`.
- **calculation** `on_result_calc` on `Unit`: watches the item owner through `inst`.

## Predicate Algebra

```text
# Arbitration: receive all same-frame claims in one batch.
on    type(Unit, claim)
where new.item = self and not cmp(own.owner, !=, null)
batch deliver(writer_id, new.prio, new.salt)
-> grant_calc      # winner = max by (prio, salt); write own(owner) = winner_ref

# Receipt: requester watches the result through its own ref.
on    inst(want, owner)
each  deliver(new)
-> on_result_calc  # new = self means success; otherwise fail and clear own(want)
```

## Correctness Argument

- `max by (prio, salt)` is a multiset function and therefore independent of delivery order.
- `salt` is a business field, such as random salt or entry order. It must not be entity id order.
- If `(prio, salt)` still ties, the result is unspecified. Deterministic replay requires a total tie-break key.
- D1: only `grant_calc` writes `owner`; requesters can never write it.
- Timeline: frame N request -> frame N+1 item writes owner -> frame N+2 requesters receive the receipt. The two-frame delay is the natural cost of dataflow interaction.
- The `owner = null` guard prevents late requests from re-arbitrating an already owned item.

## Cost

`new.item = self` is an equality condition and can use a value bucket: `O(1) + hits`. Batch append is `O(1)` per claim; arbitration is `O(number of claims in the frame)`. Receipt delivery through `inst` is `O(1) + triggers`.
