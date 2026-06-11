# 15 Summon Ownership and Kill Attribution

**Language:** English | [中文](../docs-zh/15-summon-attribution.md)

## Problem

"A player summons a pet, the pet creates a totem, and kills by any descendant credit the root owner. The killer gets XP, top contributor gets title progress, contributors above 10% get assists. Pets can be tamed and transferred. The owner may leave and leave orphan pets. A fireball may hit after ownership changed: who gets credit?"

## Why It Is Tricky

Walking an owner chain to find the root is a multi-hop join, forbidden even for one hop in conditions. "Last hit" is undefined under unordered same-frame delivery. The beneficiaries are not the victim, so the victim cannot directly write their XP. Assist lists are variable-size and cannot be claimed with `self in new.assists`.

## Decomposition

Flatten the root: descendants carry `root_owner` as data. Spawn initializes a child from the parent's root. Transfer rewrites flattened ownership and propagates it as homomorphic lineage ops. Attribution is collapsed on the victim side.

- **entity** `Unit`: lineage fields `owner`, `root_owner`, `grand_owner`, and `lineage_pub` belong to `lineage_calc`; `attack_out` freezes `attacker_root`; `hp` and `damage_book` belong to `settle_calc`; credit fields belong to `credit_calc`.
- **entity** `KillCredit`: event entity with `grant = {beneficiary, kind, amount}`.

## Predicate Algebra

```text
on    type(Unit, tame_out) | inst(owner, lineage_pub) | own(orphan_op)
where cmp(new.target, =, self) or cmp(new.target, =, null)
batch deliver(new)
-> lineage_calc

on    type(Unit, attack_out)
where new.target = self
batch deliver(new)
-> settle_calc

on    type(KillCredit, grant)
where new.beneficiary = self
batch deliver(new)
-> credit_calc

on    own(owner)
where became(null)
each
-> orphan_calc
```

## Correctness Argument

- No join is needed because `root_owner` travels as payload and own state.
- D1 keeps all lineage fields under one writer. Mirror and orphan events are normalized sources, not second writers.
- Transfer and in-flight projectiles use snapshot semantics: an attack credits the root frozen at fire time. Rechecking at hit time would be a join and might read a destroyed owner.
- Damage accounting is collapsed by root in the victim's `damage_book`, so death-time attribution does not need to chase attacker chains.
- "Last hit" is restated as max by `(dmg, salt)` within the lethal frame.
- Variable beneficiaries are handled by spawning `KillCredit` entities, one per beneficiary.
- Orphan inheritance cannot read dead owner rows, so the needed ancestor info must already be flattened.

## Cost

Claiming predicates use equality buckets; `inst` lineage propagation is `O(1) + triggers`. Settlement costs `O(hits)` and death adds `O(book size)`. Transfer propagation costs `O(subtree size)` writes and `O(depth)` frames.
