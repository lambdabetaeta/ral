# Phase E — retire `PipeMode::Values(A)`, fold byte/value streams onto Step

## What's in scope

`PipeMode::Values(A)` is the typechecker's representation of a typed
value stream between pipeline stages.  It coexists with the new Step
protocol (`[.more {head: A, tail: Thunk(F (Step A))} | .done]`) but
is no longer needed: a stage that produced `Values(A)` can produce a
`Step A` value instead, and downstream consumers can iterate on it
through the runtime adapter in `core/src/evaluator/invoke.rs` (drives
when upstream is a Step-shaped variant and the stage is a thunk-of-
lambda).

Goal of this phase: delete `PipeMode::Values` and migrate every
producer/consumer of typed value streams onto Step.  The
documented hand-rolled record / iterator dance in `from-lines |
each { … }` becomes the canonical demand-driven `producer | { |x|
… }` form.

## Where to start

- Read `~/.claude-sgai/plans/write-a-plan-for-hashed-babbage.md`
  Phase D (specifically the `Pipeline integration` and
  "`PipeMode::Values(A)` is not retired in this phase" notes) and
  the open design questions section.
- Read commits `02fd2fc` … `8a43de0` on `main` to see how Step
  landed (variants, case, recursive comp types, polymorphism).
- The runtime adapter is `try_drive_step` /
  `drive_step` in `core/src/evaluator/invoke.rs`.  It already
  handles upstream → Step iteration; you don't need to extend it.

## Inventory — call sites

```
rg -nF "PipeMode::Values" core/
```

at the time of writing returns these touch points:

  - `core/src/typecheck/ty.rs`           — enum variant
  - `core/src/typecheck/fmt.rs`          — display
  - `core/src/typecheck/unify.rs`        — unification arms
                                            (apply, occurs, free,
                                            unify_mode)
  - `core/src/typecheck/env.rs`          — free-var traversal
  - `core/src/typecheck/generalize.rs`   — substitution
  - `core/src/typecheck/infer.rs`        — pipeline stage typing
                                            (the warning at line 621)

The user-visible producers / consumers that ship `Values`:

  - `core/src/builtins/codecs.rs`        — `from-lines`, `from-json`,
                                            etc.  These currently
                                            *don't* actually produce
                                            a stream; they read all
                                            input and return a `List`.
                                            That's a happy accident:
                                            the migration is largely
                                            removing dead apparatus
                                            from the typechecker
                                            rather than touching
                                            runtime.
  - `core/src/typecheck/builtins.rs`     — schemes for the codec
                                            family, `each`,
                                            `fold-lines`, …
  - `core/src/prelude.ral`               — `each-line`, `map-lines`,
                                            `filter-lines`.  Already
                                            list-based at runtime,
                                            so just type changes.

`PipeMode::None` and `PipeMode::Bytes` stay.  Only `Values` goes.

## Concrete plan

1.  **Pin every `Values(A)` scheme to a Step shape.**  In
    `typecheck/builtins.rs`, replace
    `PipeMode::Values(τ)` on the `stdout`/`stdin` side of any
    builtin with `PipeMode::None` and have the builtin's `α` flow
    through `Ty::Variant(step_row(τ))` instead — the row pattern
    is built in `core/src/typecheck/infer.rs::is_step_type`
    (the helper is read-only; reuse the row-construction logic
    from there or factor it into `typecheck/ty.rs`).

2.  **Move `from-lines` and friends to return Step.**  At runtime
    today they return `Value::List`.  Change to return a Step
    value: `.more {head: line₁, tail: { … }}` … `.done`.  An
    eager `from-lines-list` shim can stay in the prelude as
    `from-lines | step-into-list` for callers that want a
    materialised list.

3.  **Update `each` / `fold-lines` to consume Step.**  They are
    pipeline-friendly already if Step iteration is the only
    contract.  Drop the `Values(α)` annotation from their schemes
    and let the Step row carry the element type.  Verify the
    canonical `from-lines | each { |l| echo $l }` still runs
    end-to-end.

4.  **Delete `PipeMode::Values(Box<Ty>)`** from `typecheck/ty.rs`
    and follow the resulting compile errors:

      - `apply_mode_inner`, `occurs_mode_inner`, `free_mode_inner`,
        `unify_mode_inner` — drop the Values arm.
      - `fmt_mode`, `fmt_mode_field_ctx` — drop the Values arm.
      - `infer_pipeline` — the byte/value mode mismatch hint
        (currently triggered by `(Bytes, Values)` neighbours)
        becomes redundant.  Keep the message but reword: the
        only mismatch is now `Bytes` vs `None`, which is already
        coerced.

5.  **Sweep the docs.**  `docs/SPEC.md` §15 / §20.4 mention
    `Values` directly; rewrite to describe Step pipelines.  The
    `_try` migration commit (`a8232fa`) is a good size template
    for SPEC + tests + runtime in one parcel.

6.  **Tests.**  `core/tests/typecheck.rs` and
    `ral/tests/variants.rs` already exercise Step pipelines.
    Add one test in `ral/tests/variants.rs` that runs
    `from-lines | { |line| echo $line }` (the inline-block
    form, no `each`) — it should iterate per line through the
    runtime adapter once `from-lines` returns Step.  Drop or
    rewrite any test that asserts `from-lines` returns a List.

## Watch-outs

- `infer_pipeline` (`core/src/typecheck/infer.rs:600`) currently
  emits a hint for `(Bytes, Values)` mismatches.  After this
  change there are no Values, but the mode unification may
  re-fire with a different shape.  Sanity-check the test suite
  before claiming victory.
- The variants/case/Step machinery is in place but the
  `case_extra_arm` test was deliberately removed (no annotations
  → no closed scrutinee).  Don't reintroduce it; revisit when
  type annotations land.
- `infer_case`'s scrutinee-CompTy is *not* coerced to `F α`
  (commit `8a43de0` reverted that experiment — it triggered
  value-type recursion errors in the prelude bake).  Don't
  re-add the coerce; it's a fundamental wall, not a missing
  piece.
- `Scheme` now carries `comp_ty_vars` and `comp_ty_bindings`.
  The `mk` helper in `typecheck/builtins.rs` builds schemes
  with both empty.  When you migrate a builtin that used
  `Values(τ)`, you don't need to touch these — the τ slot
  flows into the Step row and gets quantified the usual way.

## Verification

```
docker exec shell-dev cargo build 2>&1 | tail -5
docker exec shell-dev cargo test 2>&1 | grep -E '^test result|FAILED'
```

813 tests should still be green.  Add at least one new test for
the canonical `from-lines | { |l| … }` form.  Run

```
docker exec shell-dev ./target/debug/ral -c "echo $'a\nb\nc' | from-lines | { |l| echo \"L: \$l\" }"
```

to confirm streaming works at runtime.

## Files most likely to change

```
core/src/typecheck/{ty,fmt,unify,env,generalize,infer,builtins}.rs
core/src/builtins/codecs.rs
core/src/prelude.ral
docs/SPEC.md
ral/tests/variants.rs
```

Roughly a day's work if the runtime side stays the same (which it
should — `from-lines` already returns a List; turning that into a
Step is a constructor swap).
