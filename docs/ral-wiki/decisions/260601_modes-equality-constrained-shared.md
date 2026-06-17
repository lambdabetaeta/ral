---
status: active
---

# Pipeline modes are equality-constrained, in one shared definition

A pipeline stage has a computation type `F[I,O] A`; connecting two stages
requires `O_left = I_right`, and a value cannot silently cross a byte edge
(`docs/SPEC.md` §4.2.1, §20.4). Modes are therefore *equality*-constrained:
`none` and `bytes` do not unify.

## What was wrong

ral kept **two** mode-inference engines that had silently drifted apart on
this rule.

- The static Hindley–Milner checker (`core/src/typecheck/`) unified modes in
  `unify.rs`, where `None ↔ Bytes` was made to *succeed* ("coercion preserved
  by design"). That made the `TypeErrorKind::ModeMismatch` arm dead code: the
  static checker enforced *nothing* about modes, so SPEC §20.4's "mismatches
  are caught at type-check time" was false.
- The runtime engine (`core/src/ty.rs`), which runs per-bind at evaluation
  time with the live `Shell` and must keep working under `--no-typecheck`,
  correctly returned `Err` on `Bytes ↔ None`. It alone caught mismatches, at
  eval time, via `validate_pipeline` → `pipeline_mismatch`.

The two engines also duplicated the whole lattice: runtime `Mode`/`CompType`
(`ModeVar(usize)`) vs static `PipeMode`/`PipeSpec` (`ModeVar(u32)`) — the same
four constructors (`none`/`ext`/`decode`/`encode`) written twice, with the
runtime `CompType` near-colliding with the static value type `CompTy`.

## What we decided

1. **One lattice.** `core/src/mode.rs` owns `PipeMode`, `ModeVar` (`u32`),
   `PipeSpec` and the four constructors, all `Copy` PODs with serde derives
   (they ride inside a `Scheme` into the postcard-baked prelude — see the
   schema-evolution note in `core/src/lib.rs`; the consumer `build.rs`
   scripts now re-run on `mode.rs` / `typecheck/ty.rs` / `typecheck/scheme.rs`).
   Both engines import it. The static `typecheck/ty.rs` re-exports it; the
   runtime `ty.rs` renamed `Mode → PipeMode`, `CompType → PipeSpec`.

2. **One unify rule.** `mode.rs` also owns the equality rule:
   `unify_mode<S: ModeStore>(store, a, b) -> Result<(), ModeMismatch>`, where
   `ModeStore { resolve_mode, bind_mode, union_modes }` abstracts the two
   variable stores (runtime `Vec<ModeSlot>` slots, static `Store<PipeMode>`
   union-find). Both engines implement `ModeStore` and call the one function;
   neither can drift again. The static checker now rejects `None ↔ Bytes` with
   a live `ModeMismatch` (T0012), matching the runtime.

3. **Modes that were *only* sound under the old coercion are now mode-poly,
   not pinned to `none`.** Making unification strict revealed two places that
   had relied on `None ↔ Bytes` silently succeeding — both genuine
   over-constraints, not SPEC violations:
   - The higher-order list builtins `map`/`each`/`filter`/`fold`/`sort-list-by`
     typed the callback result as `pure(β)` = `F[none,none] β`, rejecting any
     byte-output callback (`map { echo $x }`). The callback result is now
     `F[μ₀,μ₁] β` with the modes quantified (`typecheck/builtins.rs`,
     `callback_result`).
   - The control wrappers `try`/`guard`/`audit` pinned their thunk body to
     `F[none,none]` via `CompTy::pure`. A control-wrapper body may itself emit
     bytes (`try { echo x } …`); the body is now `F[μ₀,μ₁] α` while the wrapper
     still *returns* a value (`F[none,none]`, `typecheck/scope.rs::infer_thunk_body`),
     as at runtime.

## The value↔byte structural boundary — enforced

The static checker enforces the §4.2.1 value↔byte boundary, and the static
output mode of streaming reducers is honest at its source. The one subtlety is
the *output mode of streaming reducers* `map-lines`/`filter-lines`/`each-line`
(prelude wrappers over `fold-lines`): their inner `echo` flushes to the stage's
stdout, so the runtime classifies them `F[Bytes,Bytes]`. The static checker
agrees by making the output mode honest at its source, rather than typing them
value-output `F[Bytes,∅]`:

- **`fold-lines` is mode-polymorphic.** Its scheme is now
  `∀α μ. U(α → Str → F[∅,μ] α) → α → F[Bytes,μ] α` (`typecheck/builtins.rs`),
  named structurally as `BuiltinTypeRule::Reducer` and given its boundary by
  `reducer_spec` in both engines (static `typecheck/builtins.rs`, runtime
  `ty.rs::reducer_pipe`). The callback's output mode `μ` is
  the reducer's output mode: a pure callback (`return $acc`) keeps `μ = ∅`
  (value-producing decode), while a callback that `echo`s per line lifts the
  whole stage to `F[Bytes,Bytes]`.
- **A sequence's byte-output is a join, not the tail's.** `seq_byte_output`
  (runtime `ty.rs`) and `lift_seq_output` (`infer.rs`) make a `Seq`'s output
  mode `Bytes` if *any* statement emits bytes, not merely the last — so a
  reducer body that `echo`s then `return`s an accumulator is typed as
  byte-output, matching its observable behaviour. Return type and input mode
  stay the tail's; a `Fun`-tailed block keeps its shape.
- **The value→byte edge is checked strictly.** `apply_piped_value` does a
  strict `unify_comp_ty_hint`, so a genuine arg-shape mismatch surfaces. The
  pipeline stage loop discriminates application from channel adjacency by
  `consumes_value_arg`: a value producer feeding a *value-arg function*
  consumer (a lambda, or a block-literal `Return(_, Thunk(Fun))`) iterates
  element-by-element through the function path, whereas a value producer
  feeding a concrete non-application stage (a `from-X` byte decoder,
  `Return(_, τ)`) is a plain channel edge whose modes are unified — so a
  `∅`-into-`Bytes` adjacency is rejected as the §4.2.1 mismatch it is.

With this, `printf "a\nb\n" | map-lines { |x| echo $x } | from-lines` typechecks
because the byte-emitting reducer is honestly `F[Bytes,Bytes]`, and the genuine
value→byte conversions (`return hello | from-json`, `'abc' | blah`) are caught.
The static and runtime engines now agree on both the *equality rule* and the
*output mode of streaming reducers*.

**Parked for maintainer review (2026-06-01):** the `if`/`case` branch-mode
*union* (`infer.rs::union_mode`/`merge_branches`, mirroring runtime
`ty::infer_branches`). When two branches disagree on a mode, the conditional's
mode becomes a fresh variable a downstream stage can pin, rather than a hard
mismatch — since only one branch runs. This is a deliberate design choice that
the maintainer wants to weigh against equality-strictness; it is implemented and
green but not yet ratified.
