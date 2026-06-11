# 22 Hitstop / Bullet Time / Local Time Dilation

## Problem

"Hitstop freezes attacker and victim for several frames. Bullet time makes enemies act at half speed while the player stays full speed. Frozen entities stop for N frames. Their combo windows, invulnerability, cooldowns, and poison leases should all extend naturally and resume from the same point. Different entities may run at different rates, and rates can change."

## Why It Is Tricky

Many time semantics are written as differences on `Clock.frame`. Once each entity has its own rate, global-frame difference no longer equals perceived duration. Patching all future timestamps by adding N is impossible under D1 and misses timestamps not yet written. Slowdown is scaling, not translation. Cross-entity checks must choose whose time axis owns the rule. Floating-point accumulation drifts and breaks replay.

## Decomposition

Use **local time axis as a field + timestamp guards on that axis + freeze as stopped axis**.

- Every entity has a single-writer **timekeeper** subscribing to `Clock.frame`. Animation/physics already pays this polling cost.
- Advance `local_time` as an integer fraction accumulator: `acc += num; local_time += acc / den; acc %= den`.
- Freeze means do not advance the axis while `freeze_left > 0`. No pending timestamps are modified.
- Every relative duration stamps and compares on the same entity's local axis.
- Cross-entity judgments use the judged entity's axis. Invulnerability belongs to the defender, so the predicate compares defender `own.local_time` and `own.invuln_until`.

## Predicate Algebra

```text
on    type(Clock, frame)
each
-> timekeeper_calc

on    own(cast_req)
where cmp(own.local_time, >=, own.cd_ready_at)
batch deliver(new)
-> skill_calc

on    type(Attacker, attack_out)
where new.target = self and cmp(own.local_time, >=, own.invuln_until)
batch deliver(new.dmg)
-> take_damage_calc
```

## Correctness Argument

- `local_time` is monotone, so `>=` and edge guards keep the same shape as global-time guards.
- Freeze extension is a consequence of the axis not moving. There is no timestamp enumeration and no missed field.
- Rate changes affect future integration only; already-stamped deadlines remain valid.
- Integer fraction accumulation is deterministic and drift-free.
- Same-frame pace/freeze requests use monotone seqs to absorb duplicate sampling and D3 competition.
- I-frame checks use the victim's axis because that rule is owned by the victim. The architecture makes this the only predicate-expressible option.

## Cost

One O(1) timekeeper run per active entity per frame, usually already paid by animation/physics. Completely idle entities can switch to sparse alarm-driven advancement. Guard rejection remains zero-trigger.

## Ripple Effects

The lease from [01](01-absence-timeout.md), cooldown from [05](05-cooldown-state-machine.md), invulnerability from [11](11-immunity-invincibility.md), and combo windows and buffers from [14](14-combo-cancel-buffer.md) should all use local time in games with time scaling.

## Runnable Verification

`tests/time_dilation.rs` verifies hitstop pausing cooldowns, exact half-speed integer advancement, and defender-axis invulnerability during freeze.
