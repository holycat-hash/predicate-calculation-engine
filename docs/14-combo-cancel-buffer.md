# 14 Combo Windows / Cancel Frames / Input Buffering

**Language:** English | [中文](../docs-zh/14-combo-cancel-buffer.md)

## Problem

"Frames 12-20 of an action are cancel frames. If a buffered combo input exists inside the window, cancel immediately into the next action. Input may arrive before the window and must not be lost. Input during the window should be consumed as soon as possible. One input is consumed exactly once. Later input overwrites earlier input. Buffers expire."

## Why It Is Tricky

- "Frames 12-20" is a relative interval, not a cell value. Make endpoints fields such as `cancel_from` and `cancel_to`.
- Input may arrive before the window. A predicate that was false at input time has no memory, so the instant event must become a later-queryable waterline.
- There are two dual edges: intent first then window opens, or window open then intent arrives.
- "Inside the window and intent present" is a level condition, not an edge. With D2, a driver writing `action_frame` every frame would trigger every frame.
- Action restart resets the frame and changes all window fields atomically; splitting those writes across calculations breaks D1 and edge semantics.

## Decomposition

Use **window endpoints as fields + intent register with monotone seq + paid driver polling**.

Fighting-game actions already advance every frame, so `action_drive_calc` must subscribe to `Clock.frame`. That cost is already paid. Fold window checks, intent consumption, action restart, and expiry into the same run.

- **entity** `Fighter`: `input_req` is written by input systems; `intent` belongs to `intent_register_calc`; `action_id`, `action_frame`, `cancel_from`, `cancel_to`, `duration`, `combo_count`, and `consumed_seq` belong to `action_drive_calc`.
- Intent registration collapses local/network/AI input and same-frame multiple buttons into a monotone seq; D3 uses `max(seq)`.

## Predicate Algebra

```text
on    own(input_req)
batch deliver(new)
-> intent_register_calc    # keep max(seq)

on    type(Clock, frame)
each  deliver(new)
-> action_drive_calc       # advance frame; if seq is new, not expired, and frame in window,
                            # restart action and retire seq atomically
```

## Correctness Argument

- Intent is a waterline, not a one-shot event, so it is still visible when the window opens.
- It is not consumed early because the driver checks `action_frame in [cancel_from, cancel_to]`.
- It is not repeated because `consumed_seq` is monotone and each seq can pass once.
- Same-frame multiple inputs use `max(seq)`, an order-independent multiset function.
- Expiry is checked before window acceptance, so stale inputs cannot pop later.
- Restart is consistent because the driver is the only writer of the whole action-state cluster.
- New input in the same frame as consumption cannot be accidentally eaten because snapshot reads make it visible only next frame.

## Cost

The action driver runs once per active fighter per frame, a cost already paid by animation/physics. Intent registration runs only on input frames. With local time dilation, window and buffer deadlines should use the entity's local time axis from [22](22-time-dilation.md).

## Runnable Verification

`tests/combo_cancel_buffer.rs` verifies buffered consumption at the opening frame, no repeat across the window, delayed but unexpired input, expired-input retirement, and same-frame dual-key determinism by `max(seq)`.
