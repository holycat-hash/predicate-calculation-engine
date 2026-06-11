# 10 Damage Formula: Bilateral Pipeline, Shields, and Reflection

## Problem

"Final damage = attack * multiplier * crit * (1 - armor reduction) * (1 + vulnerability - mitigation). Deduct shields before HP, true damage pierces shields, targets reflect 10%, many same-frame sources net correctly, and shield overflow is exact."

## Why It Is Tricky

- Formula inputs belong to two instances: attack-side factors live on the attacker, defender-side factors live on the defender. Conditions cannot join.
- Shields are stateful mitigation. Many hits in one frame cannot all ask "how much shield is left" with `each` read-modify-write.
- Reflection is a feedback loop.
- Random crits must not break replay determinism.

## Decomposition

**Split the formula by data ownership**. The attack side computes its half and freezes it into the event payload. The defender side nets armor, vulnerability, shields, HP, and reflection in one `settle_calc`.

- **entity** `Attacker`: writes `own(attack_out) = {target, raw, tags, reflected: false, frame, salt}`. `rng_state` is an own field, so random state is deterministic data.
- **entity** `Unit`: `settle_calc` is the only writer of `hp`, `shield`, and `reflect_out`. Defender-side modifiers are snapshot-read from own fields or derived panels.

## Predicate Algebra

```text
on    type(Attacker, attack_out) | type(Unit, reflect_out)
where new.target = self
batch deliver(new, writer_id)
-> settle_calc
```

Inside the calculation:

```text
eff_i = raw_i * (1 - armor) * (1 + vuln - mit)
blocked = min(own.shield, sum(blockable))
overflow falls to hp
true damage bypasses shield
write own(shield), own(hp) once
emit reflection only for rows where reflected = false
```

## Correctness Argument

- No join is needed because the formula is split along ownership boundaries. In-flight damage uses the attacker's stats at fire time.
- "Shield before HP" and "true damage pierces shield" are segmented functions over a multiset, not an order.
- Shield overflow is exact under netting. "The hit that broke the shield" is undefined under D3; if a reward is needed, choose a winner by a business total-order key such as `(dmg, salt)`.
- Reflection terminates by flattening `reflected` into the payload and reflecting only false rows. Longer mirror systems need the monotone-chain argument from [08](08-chain-reaction.md).
- If one victim must reflect to many attackers, one `reflect_out` cell is not enough; spawn `ReflectHit` event entities as in [06](06-event-materialization.md).

## Cost

`new.target = self` uses an equality bucket. Settlement runs once per target per frame and costs `O(frame hits)`. Reflection event spawn is `O(1)` per event. The total cost is independent of total world size.

## Variant

Unify all damage as `Hit` event entities. Every source spawns a `Hit`; settlement subscribes to one type. This reduces union-branch growth and gives receipts a natural inst-ref anchor, at the cost of per-hit allocation.
