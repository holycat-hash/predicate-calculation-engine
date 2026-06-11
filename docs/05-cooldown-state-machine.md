# 05 Cooldowns and State Machines

## Problem

"A skill has a 60-frame cooldown; cast requests during cooldown must be rejected. A character state machine has `idle/casting/stunned`, and illegal transitions such as casting while stunned must not happen."

## Why It Is Tricky

"During cooldown" is a time interval. Checking a timer every frame falls back to polling. Illegal transitions can be guarded in calculation with `if`, but that is late: the trigger already happened, the `|F|` budget is spent, and every calculation must duplicate defensive checks.

## Decomposition

Use two techniques: stamp events with frame numbers, and lift guards into the predicate layer. Conditions answer "is this worth waking?"

- **entity** `Unit`: fields `cast_req`, `cd_until`, and `state`.
- **calculation** `request_calc`: when producing a request, snapshot-read `Clock.frame` and write `own(cast_req) = {skill, frame}`.
- **calculation** `cast_calc`: the only writer of `cd_until`; cooldown and illegal states have already been filtered by the predicate.

## Predicate Algebra

```text
on    own(cast_req)
where cmp(new.frame, >=, own.cd_until) and own.state in {"idle", "moving"}
each  deliver(new.skill)
-> cast_calc      # write own(casting_skill); own(cd_until) = new.frame + 60

on    own(stun_hit)
where cmp(own.state, !=, "dead")
each
-> state_calc     # write own(state) = "stunned"

on    own(stun_until_passed)
where became(true)
each
-> state_calc     # write own(state) = "idle"
```

## Correctness Argument

- No polling: while in cooldown, no predicate wakes until a new `cast_req` write arrives.
- Frame-stamp offset is stable. `request_calc` reads the previous frame's `Clock.frame`, and `cd_until` is derived from the same source, so comparisons stay closed.
- Guards are visible documentation: legal-transition conditions are in predicates and can be inspected at registration time.
- D1 bonus: `state` has one owner, `state_calc`, so competing state changes are caught at registration.
- Multiple transition sources are compatible with the single-predicate rule by using scope union, or by first materializing requests into fields and then merging them.

## Cost

Own-scope hash chain is `O(1)`. Own-field guards are point lookups. This does not degrade globally because each guard checks only the subscriber's own row.
