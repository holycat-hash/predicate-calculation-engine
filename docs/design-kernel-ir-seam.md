# Design memo: the execution-layer kernel-IR seam

> Status: **implemented through migration Steps 1-4**. Core now has `KernelIr` /
> `KernelOp`, registration-time `Tier::Kernel` validation, SoA `KernelColumn`
> batch execution, a `KernelBackend` trait, and the default `ScalarKernelBackend`.
> SIMD/GPU implementations remain optional external or feature-gated backends. Chinese mirror:
> [../docs-zh/design-kernel-ir-seam.md](../docs-zh/design-kernel-ir-seam.md).

## 1. Problem

The predicate layer already took the "behavior -> data" step: `Cond` / `Expr` are lowered
at registration into a flat postfix program (`CompiledCond` / `CompiledExpr` in `route.rs`),
evaluated at runtime as a tight loop over a contiguous slice — vectorizable and auditable.
The execution layer now has the same seam for the kernel subset: `KernelIr` is a postfix
data program that can run through a SoA backend, while Turing-complete calculation bodies
still use the opaque `Box<dyn Fn>` fallback. The completed seam resolves the core
opaque/backend gap and leaves only optional hardware-specific backend implementations:

- **D4 effect confinement is machine-checked for IR, still a contract for closures**: a
  closure can silently do ambient I/O / logging / statics / RNG, while an IR body cannot
  express those effects.
- **C1 kernel tier has an executable column batch**: registration requires IR, and runtime
  dispatches each calc group as lanes over SoA input/output columns.
- **C3 GPU residency is now a backend-selection input**: `Schedule` / `Profile` resolve
  residency partitions, and runtime passes that hint to registered `KernelBackend`s. Without
  a GPU backend, the default scalar backend remains the exact fallback.
- **SIMD / GPU dispatch now has an entry point**: backend authors receive `(IR, lanes,
  input columns, residency hint)` instead of opaque closures.

Per the architecture philosophy (do free optimizations fully / hand costly ones an
interface / find the **smallest seam set** that lets the most optimizations through), SIMD,
GPU, residency, and migration all require the same precondition: **treating calc behavior as
data**. The kernel-IR seam is that smallest seam; Steps 1-4 now finish the core
data-parallel backend story.

## 2. The seam: behavior -> data (kernel IR) + pluggable dispatch

Two parts:

1. **Kernel IR**: a small, registration-time data form expressing the **kernel subset** of
   a calc body (C1: no spawn, no dynamic allocation, no divergent branching, side effects
   only via `ctx` writes to the calc's own instance). It is the execution-layer dual of
   `CompiledExpr` — a postfix / three-address program of "read inputs -> compute -> write
   outputs," but allowing **multiple output writes** (a calc may write several declared
   fields).
2. **Pluggable dispatch**: a `KernelBackend` interface taking (kernel IR, lanes, input
   columns, output columns, residency hint) and executing it. Core ships the SoA scalar
   backend; optional future implementations are SIMD and GPU.

**Turing-complete calc bodies still use the `Box<dyn Fn>` fallback** — the IR covers only
the kernel subset and does not force everything into IR. This mirrors the layering:
predicates are a closed algebra (data), calculations are Turing-complete (code); kernel IR
merely lowers the **pure-dataflow class** of calculations to data while the rest stay
closures.

## 3. Scope of the IR

The kernel IR describes a **per-instance kernel**:

```
inputs  : a set of column reads (own fields / projected inputs / clock scalars)
body    : arithmetic / comparison / select (no loops, no divergent branches — select, not jump)
outputs : a set of column writes (only the calc's own declared fields, D1)
```

This is exactly the shape of a SIMD lane / GPU thread: N instances = N lanes, input columns
= SoA input buffers, output columns = SoA output buffers. The SoA column substrate already
exists (`src/column.rs` typed columns + `genslots.rs` liveness slots) and is the kernel IR's
materialized base.

Out of scope (stay closures): spawn / destroy, cross-entity ref point reads, arbitrary
control flow, external library calls.

## 4. Migration path (incremental, non-breaking)

Each step is **additive**, the closure fallback always remains, and after each step the full
test suite should stay green byte-for-byte (interpreter == closure).

- **Step 0 (done)**: the predicate-layer "behavior -> data" already shipped (`CompiledCond`
  / `CompiledExpr`). The precedent and the evaluator shape (postfix program + value stack)
  exist and can be reused.
- **Step 1 (done): IR + scalar interpreter**. Define the kernel IR enum and a scalar interpreter.
  Calc registration may **optionally** carry an IR body alongside `CalcFn`; calcs with IR
  run via the interpreter, others via the closure. Acceptance: IR path and closure path
  produce identical results for the same calc.
- **Step 2 (done): C1 wiring + D4 machine-check**. C1-annotated calcs are required to provide IR;
  registration validates the IR meets the kernel constraints. **An IR body has no ambient
  side effects by construction => D4 is upgraded from contract to a machine-checked
  guarantee for the IR subset.**
- **Step 3 (done): SoA column execution**. Run the kernel over input columns producing output
  columns (instead of building a per-instance ctx). Reuse the `column.rs` typed columns.
  This is the SIMD prerequisite and delivers C1's "kernel batch."
- **Step 4 (done): pluggable backends**. A `KernelBackend` trait + the default
  `ScalarKernelBackend`; optional SIMD backend (`std::simd` or a crate) and GPU backend
  (`wgpu`, feature-gated) can be registered. The C3 residency suggestions from `Schedule` /
  `Profile` are real inputs to backend selection.

## 5. Payoff (each tier upgrades from "seam + advisory" to "real integration")

| Before seam | Current / remaining |
|---|---|
| D4 = un-checkable contract | IR subset has no out-of-`ctx` effects **by construction**; closures still assumed |
| C1 = annotation only | IR execution is a SoA kernel batch |
| C3 = advisory only (no backend) | residency is passed to backend selection; scalar fallback is exact |
| SIMD / GPU = no entry point | `KernelBackend` is the entry point; hardware backends are optional |
| parallel legality = assume effect confinement | provable for the IR subset; closures still assumed (shrinks the trusted surface) |

## 6. Boundaries (non-goals)

- **Do not push everything into IR**: Turing-complete bodies keep the closure fallback. The
  IR is the fast path for the pure-dataflow class, not a general scripting language.
- **Do not build hardware backends in core**: SIMD/GPU backends are "costly" optimizations
  — hand an interface (`KernelBackend`) to the developer to choose by hardware / load. Core
  ships the exact scalar SoA fallback and the backend trait. Same principle as the render
  submission seam and the spatial-index helper.
- **Do not change the predicate layer**: predicates are already data; this memo only adds the
  execution-layer dual.

## 7. Relationship to existing pieces

- `route.rs` `CompiledCond` / `CompiledExpr`: the predicate-layer "behavior -> data"
  precedent and a reusable evaluator shape.
- `src/column.rs` / `src/genslots.rs`: SoA columns + liveness slots, the kernel IR's
  materialized base.
- `runtime/mod.rs` `Schedule` / `ScheduleGroup` / `Profile`: already group by calc and
  resolve C1/C3 partitions; they become the dispatch driver.
- `Snapshot` / `restore` (GGPO): an IR body is deterministic, aiding Canonical replay.
- render `run_continuous` (B3 two-phase snapshot + parallel): already shaped as "read frozen
  columns -> write buffered columns," a render-side rehearsal of kernel-IR column execution.

## 8. Effort and recommendation

The core seam is now implemented and tested: IR equivalence, C1/D4 validation, SoA column
batching, and backend selection. The next useful round is optional backend work: SIMD lane
specialization, feature-gated GPU execution, and policy work around fallback / hysteresis.
