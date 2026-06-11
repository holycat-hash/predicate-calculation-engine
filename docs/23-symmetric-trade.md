# 23 Two-Player Trade / Symmetric Atomic Exchange

**Language:** English | [中文](../docs-zh/23-symmetric-trade.md)

## Problem

"Two players trade face to face. Each offers items and confirms. When both confirm, the exchange commits atomically. If either side changes the offer before the other confirms, stale confirmation must be invalid. Cancellation or disconnect must refund both sides exactly. Items must not duplicate or disappear."

## Why It Is Tricky

The saga from [19](19-atomic-consume.md) is asymmetric; two-player trade is symmetric. There is no native cross-entity atomic write, so applying the two halves in separate frames creates observable gaps. The classic scam is TOCTOU: confirmation was based on the old offer, but the offer changes before confirmation arrives. Same-frame offer change plus confirm is especially dangerous under unordered delivery.

## Decomposition

Use **session entity batch arbitration + offer generations + bilateral escrow + single-value verdict**.

- **entity** `Trade`: long-lived session entity. `broker_calc` owns `offer_a`, `offer_b`, `gen`, `confirm_a`, `confirm_b`, `state`, and `verdict`.
- Any offer change increments `gen` and clears confirmations. A confirmation carries the gen it saw; broker accepts only matching gen.
- Same-frame offer plus confirm is defined by broker's internal order: offers before confirms, so stale confirmation always invalidates.
- Each player escrows its own offered items. Offer change refunds old escrow and escrows the new offer.
- When both confirmations are valid, broker writes one `verdict = {commit, to_a, to_b}` value. Both players watch the same verdict through `inst`.
- Death/session cleanup is handled by ref invalidation and idempotent reclaim.

## Predicate Algebra

```text
on    type(Player, trade_out) | own(a) | own(b)
where new.trade = self or became(null)
batch deliver(new)
-> broker_calc

on    own(cmd) | inst(pending_trade, verdict) | own(reclaim_op)
batch deliver(new)
-> wallet_calc

on    own(pending_trade)
where became(null)
each
-> reclaim_probe_calc
```

## Correctness Argument

- Scam window closes because confirmation validity is a local equality check against broker's current generation. Cross-frame stale confirms fail; same-frame offer+confirm fails because the broker processes offers before confirms inside its multiset function.
- The swap is atomic at the verdict level: both halves are in one cell value. Each side applies its own half atomically in its wallet calculation.
- Duplication is closed by the path `items <-> escrow -> verdict -> other items`; escrow has only commit or refund exits, and `applied_gen` makes verdict application idempotent.
- `state` is monotone from open to done, so a second verdict cannot be produced.
- If a player dies before decision, broker aborts and refunds the survivor. If death happens in the decision frame, already-written verdict is a value snapshot. If the session dies, `pending_trade` invalidation triggers reclaim.
- Normal completion also nulls `pending_trade`; empty-escrow reclaim is a no-op.

## Cost

Broker runs at most once per session per frame and costs `O(frame ops log frame ops)` if it sorts ops. `new.trade = self` should be an equality bucket in the full runtime. The multi-frame trade latency is the natural cost of dataflow interaction.

## Runnable Verification

`tests/symmetric_trade.rs` verifies atomic same-frame swap, stale confirmation invalidation after offer change, full refund on cancellation, and refund when one side dies.
