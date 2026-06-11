# 01 Timeout / N Silent Frames / Heartbeat Drop

**Language:** English | [中文](../docs-zh/01-absence-timeout.md)

## Problem

"If a `Session` has not received a `Heartbeat` for 30 frames, mark it disconnected."

## Why It Is Tricky

"No write happened" is not an event; it is the absence of an event. The predicate layer does not allow an independent NOT trigger source, because that would degrade the system into per-frame polling. Timeout logic still needs a concrete firing moment.

## Decomposition

- **entity** `Session`: fields `lease_until` and `state`.
- **entity** heartbeat source such as `Conn`: writes `own(beat) = {session: ref, frame: F}`. The writer stamps the frame by snapshot-reading `Clock.frame`; the one-frame offset is stable and harmless.
- **calculation** `renew_calc` on `Session`: receives heartbeats and renews the lease.
- **calculation** `expire_calc` on `Session`: detects expiry. The only legal time trigger is subscribing to `Clock.frame`; this is explicit polling and pays its own cost.

## Predicate Algebra

```text
# Renew: positive events maintain the lease.
on    type(Conn, beat)
where new.session = self
each  deliver(new.frame)
-> renew_calc                  # write own(lease_until) = new.frame + 30

# Expire: the only legal poller.
on    type(Clock, frame)
where cmp(new, >, own.lease_until) and not cmp(own.state, =, "dead")
each
-> expire_calc                 # write own(state) = "dead", or destroy_self
```

## Correctness Argument

- The negation restriction is not broken: `and not` is only a guard; the positive trigger source is `Clock.frame`.
- The guard `own.state != "dead"` makes expiry one-shot. Without it, the condition would fire every frame after expiry.
- D1: `lease_until` belongs to `renew_calc`, while `state` belongs to `expire_calc`; no conflict.
- Snapshot read: `expire_calc` sees last frame's `own.lease_until`. If a heartbeat and expiry race in the same frame, this is conservatively one frame late, which is acceptable for lease semantics. If exactness is required, use 31 instead of 30.

## Cost

The `Clock.frame` condition references own fields, so it is a live threshold and degrades to the number of `Session` subscribers on that cell: `O(number of sessions)` checks per frame. For massive session counts, flip the design: build an `Alarm.0` singleton index entity, batch-subscribe to `type(Session, lease_until)`, maintain timer-wheel buckets, and let it be the only entity subscribing to `Clock.frame`. Expired sessions can then be dispatched through [event materialization](06-event-materialization.md).
