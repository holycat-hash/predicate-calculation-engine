# 20 Projectiles: Flight, Pierce, and Hit Dedupe

## Problem

"An arrow flies every frame and stops on hit. A piercing arrow can hit at most three targets and never the same target twice. Fast arrows must not tunnel through cells. Wind can deflect projectiles, shields can reflect them and transfer attribution, and arrows keep flying after the shooter dies."

## Why It Is Tricky

Projectiles need self-driving motion, whose only legal source is `Clock.frame`. Point-sampled movement tunnels at high speed. Pierce budget needs an ordered prefix, but D3 delivery is unordered. Attribution cannot be looked up on the shooter at hit time, and hit stop races with same-frame candidates.

## Decomposition

Flip to **lazy parametric trajectories**: launch writes `traj = {origin, v, t0}` once; position is `pos(t)`. A grid index entity sweeps line segments and emits hit candidates. Slow projectiles may still use the honest per-frame baseline.

- **entity** `Projectile`: `traj` and `cred` belong to `steer_calc`; `pierce_left`, `hit_set`, and `_alive` belong to `settle_calc`.
- **entity** `Grid.0`: `traj_table` belongs to `track_calc`; `cursor` belongs to `sweep_calc`.
- **entity** `HitCand` and `Hit`: event entities.

## Predicate Algebra

```text
on    type(Clock, frame)
each
-> fly_calc      # honest baseline for a small number of slow projectiles

on    type(Projectile, traj) | type(Projectile, _alive)
batch deliver(writer_id, new)
-> track_calc

on    type(Clock, frame)
each
-> sweep_calc    # sweep [cursor, now] segments and spawn HitCand

on    type(HitCand, hit)
where new.proj = self
batch deliver(new)
-> settle_calc

on    type(Wind, deflect_op) | type(Shield, parry_op)
where new.proj = self
batch deliver(new)
-> steer_calc
```

## Correctness Argument

- Registration latency does not lose trajectory segments because the sweep cursor starts from `t0`.
- Pierce count is a batch multiset decision: remove already-hit targets, sort candidates by `(t, salt)`, take the prefix of length `pierce_left`, and update the ledger once.
- "Hit stops projectile" is settled inside the candidate batch. Extra same-frame candidates are discarded; late candidates route nowhere after `_alive = false`.
- Deflection rewrites the single source of truth, `traj`, and shield reflection rewrites `cred`. Attribution is frozen as payload, so shooter death does not break credit.
- If strict segment-generation correctness is needed, add a generation to candidates and drop stale ones, mirroring [18](18-cast-interrupt.md).

## Cost

Parametric flight changes scheduler writes from per-projectile per-frame position writes to trajectory changes. Sweep cost lives inside `sweep_calc`: `O(active segments + crossed cells)`. Hit candidate and hit event entities cost proportional to candidates and accepted hits. Slow and fast projectile strategies can coexist.

## Variants

Parabolic or homing projectiles replace `pos(t)` with another pure function. Homing can use `inst(target, position)` to reparameterize. Lasers are one-frame sweeps and do not need persistent trajectory table entries.
