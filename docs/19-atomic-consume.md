# 19 Atomic Consume and Double-Spend

## Problem

"One key and two doors request consumption in the same frame: only one door opens. A shop purchase deducts coins from the player and stock from the shop; any failure or mid-flight disappearance must not lose or duplicate money or goods."

## Why It Is Tricky

Check-then-act naturally races under snapshot reads. Two calculations can both read `keys = 1` and both decide to spend. D1 gives half the cure: balances have one writer. That writer still receives multiple same-frame requests and must arbitrate inside one batch. Cross-entity writes are not atomic, so purchases require an escrow saga. Rollback on disappearance is absence and must be triggered through ref invalidation or leases.

## Decomposition

Use **single-writer batch arbitration + escrow frame-to-frame saga + idempotent request ledger + ref-reap rollback**.

- **entity** `Player`: `balance`, `escrow`, `reserve_out`, `pending_shop`, and `spend_log` belong to `wallet_calc`.
- **entity** `Shop`: `stock` and `decide_out` belong to `shop_calc`.
- All money-moving sources normalize into ops `{target, kind, req, salt, ...}`.

## Predicate Algebra

```text
on    own(buy_req) | type(Door, demand) | type(Shop, decide_out) | own(reclaim_op)
where new.target = self
batch deliver(new)
-> wallet_calc

on    type(Player, reserve_out)
where new.shop = self
batch deliver(new)
-> shop_calc

on    type(Shop, decide_out)
where new.target = self and cmp(new.kind, =, "grant")
batch deliver(new)
-> stash_calc

on    own(pending_shop)
where became(null)
each
-> reclaim_probe_calc
```

Inside `wallet_calc`, settle old grants/rejects first, then reclaim, then new spends/buys sorted by a business key. A `buy` moves balance into `escrow[req]`, emits `reserve_out`, and stores the shop ref. A `grant` deletes escrow; a `reject` refunds; a `reclaim` refunds all remaining escrow.

## Correctness Argument

- Double-spend is closed because check and commit happen in the same wallet batch run. Two doors competing for one key become a deterministic multiset arbitration.
- Escrow preserves conservation after the player reserves money: it is not gone; it is in the player's own escrow ledger.
- Shop stock check and deduction happen in one `shop_calc` run. If the shop dies before routing, nothing changes; if it dies in the decision frame, its already-written grant/reject is a value snapshot and still routes.
- Late grant vs. reclaim is safe by idempotent ledger plus internal ordering: grant removes the escrow first, reclaim then finds nothing to refund.
- Normal settlement writing `pending_shop = null` may cause a reclaim probe; empty-ledger reclaim is a harmless no-op.
- Goods cannot duplicate because stock is single-writer and each req is granted once. Goods cannot disappear because reject/reclaim are the only escrow exits besides grant.

## Cost

All main routes are equality-to-self. Wallet and shop each run at most once per frame and cost `O(frame ops log frame ops)` if sorting is needed. The saga's multi-frame delay is the inherent cost of dataflow interaction.

## Runnable Verification

`tests/atomic_consume.rs` verifies same-frame double-spend arbitration, purchase conservation across frames, refund on insufficient stock, and rollback when the shop dies after reservation.
