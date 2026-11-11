---
generated_at_commit: 2d05ef216180fc2454ccef6f4ed4d4c65bf69136
generated_at_date: 2026-11-11
covers_paths: [core/src/typecheck/, core/src/typecheck.rs]
---

# Map: core / typecheck

**`core/src/typecheck/` generates constraints, closes a ranked solver, then
reads back checked CBPV IR.** The algorithm is
[[internals/type-inference|type inference]]; the architectural decision is
[[decisions/261110_elaboration-is-decided-by-suspended-matches|suspended matches]].

## Entry points

`core/src/typecheck.rs` owns the inference sessions and their export checks.

- `typecheck(phrases, SessionSchemes, Option<ReturnContract>)` generates
  pending phrases on `Solver`, settles all remaining elaboration into
  `Settled`, holds an optional return contract live, closes gates, nested
  tables and errors into `Solution`, then reads back a `Toplevel` whose sites and readmissions name one fresh
  `UnitId`. Errors are sorted by span and no checked tree escapes. Re-checking
  a checked tree accepts it and inserts no further elaboration; raw variable
  ids are not canonical, so the two trees need not be `==`.
- `SessionSchemes` carries bindings, `handled: Vec<String>` for diagnostics
  only, and the host's `Manifest`.
  `Solver::seed` imports them at rank zero, re-seeding residuals once per unit;
  host bindings without schemes receive one weak monomorphic unknown.
- `bake_prelude` checks against the manifest and returns the annotated prelude;
  `SessionSchemes::from_prelude` reads its exported definition schemes, with
  no separate scheme blob.
- `arm_stands_in` enters a region to check an arm against its stand-in and
  closes the unit before returning an install verdict. It stores no scheme.
  `binding_value_scheme` instead exports a closed lexical binding scheme;
  `close_thunk_scheme` generalises after region exit, then settles only a
  successful unit. Both doors retain every error from close (`Vec<TypeError>`);
  the runtime reports refusal as one `IllTyped` fact through
  `TypeError::diagnostic` ([[map/core/diagnostics|diagnostics]]).

## Constraint generation

`generate.rs` and `generate/` never resolve a solved type or inspect a scheme.

- `generate.rs`: `Generator`, phrases, `let_`, patterns, lambdas,
  force/return, recursive groups, `check`, `region` and lexical scopes. Input
  is a `Checked` tree, output an `Inferring` one. `let_` closes the
  RHS/pattern region before `bind_pattern` generalises. `check` writes a
  lambda's arrow equation before checking its body. `rec_group` checks a
  group once in its own region and caches the members' generalised schemes
  with the pending group; every projection instantiates its member's scheme.
- `generate/call.rs`: application chains, head lookup, builtin calls,
  boundary saturation, argv and handler arms. `head_class` follows
  binding → value builtin → base frame → external, consulting Σ and lexical
  bindings alone. Stand-in lookup uses the manifest's base-frame scheme or
  `[String] → Command`, never the handler stack. `apply_args` takes
  one `CallHole` and writes left-to-right argument matches, naming the head
  variable when there is one; a bare binding
  head becomes `CompKind::BindingCall`. `argv` queues single-element gates,
  while a spread only demands a list. `arm` generates the block (literal
  bodies use `check_arm_body`), then calls `stands_in`. That is the sole
  arm-to-head relation, shared with catch-all and contract arms;
  `install_arm` owns a region.
- `generate/data.rs`: values, records, maps, primitives, updates and indexing.
  `Key::{Label, Position, Data}` immediately requires a record slot, list or
  string-keyed map. There is no deferred read or index-made weakness
  ([[decisions/261102_a-written-key-is-a-name|a written key is a name]]).
- `generate/control.rs`: joins, `if`, `case`, `try`, pipelines, redirects and
  capture. Arms meet fresh results, then equate with the join. Every pipeline
  stage demands ready-producer shape; non-final stages must write, and decoders
  are refused there. `stage_root_stdin_feed` rejects an overridden pipe edge.
  Stdout redirects retain the payload at a fresh result grade.
- `generate/scope.rs`: `within`, `grant`, options, cleanup, audit, spawn,
  gather and protection. Written options are checked key by key; nested table
  obligations wait for closure.

## Solver and readback

**Only `solve.rs` observes the live store.**

- `Solver` owns the private `Unifier`, suspended matches keyed by their
  scrutinee's root, per-hole decisions, obligations and errors.
  `binding(rhs, forced) -> (Ty, BindHole)`, `call() -> CallHole` and
  `argument(call, head, callee, arg, recursive_call)` are the only ways to
  suspend a match. `Unifier.at` is the one current span; `WithSpan for Solver` borrows it.
- Every public constraint is one private operation followed by one
  `propagate`, which drains `take_touched`: it improves the producer pairs on
  touched roots and re-steps their waiters, re-keyed under each root's new
  representative. `step` decides a match at a known head, recording a `Mode`
  or a split, and otherwise lowers its mentions to the scrutinee's rank.
  `close_region` chooses all young defaults together;
  `ConflictingDefaults` (T0080) refuses a root with both a binding and an
  argument match waiting, carrying the variable each names, if any. Refused regions retire unresolved young matches.
- `Solver::settle` closes rank zero and retires every elaboration match,
  yielding `Settled`. Its contract-phase API permits equations (including
  `equate_held`), fresh types, instantiation, resolution, `nested` and
  `diagnose`, but no `binding`, `call`, `argument`, `enter` or `exit`.
  All equations propagate through the solver; no contract mutates its private
  unifier behind propagation. `Settled::close` judges final-head argv gates,
  checks nested tables and attaches unit-read notes. It yields a `Solution`
  only for an error-free unit; otherwise the sorted errors. `Solution`
  exposes decisions, resolution, sites, settlement and scratch, never
  constraints.
- `readback.rs`: `Readback` turns an `Inferring` tree into a `Checked` one. It
  applies each `BindHole`'s `Mode` (Let-fun, run, capture/decode) and each
  `CallHole`'s splits, turns a `BindingCall` into an `Exec`, snapshots sites,
  settles definition schemes into `Closed`, builds `Toplevel.admits`, and
  memoises rebuilt recursive groups.
- `env.rs`: `TyEnv`, handled-name sets for diagnostics only, and the `Inferring` phase
  (`Bind = BindHole`, `Call = BindingCall = CallHole`, `Boundary = Ty`,
  `Membrane`). `Membrane::{Kept, Imported}` is a protected value's contract
  while inferring: a checked tree's site, or a session binding's solved type
  ([[map/core/ir|IR]]).

## Types, contracts and diagnostics

- `unify.rs`: four typed union-find stores (`Store<T: Sort>`, slots
  `Parent` or a ranked `Root` whose state is free or bound), each root ranked
  even when bound; unions keep the minimum, bindings lower reachable unknowns,
  weakness is rank zero. A free row's meta is `Depth::{Shallow, Deep(witness)}`.
  `take_touched` reports the roots changed since it was last called.
  `WeakSource` records provenance. Data-guarded recursive types, `Pairs`,
  Rémy row rewrite and kind admission remain here. `bind_row` enforces finite
  tail spines and type-rooted field payloads ([[design/row-types|row types]]).
  `unify_producers` checks grade/payload pairing for returners and handles;
  `improve_producers` entails value grades from non-unit payloads, lowers pair
  dependencies and `exit` retires young pairs before generalisation
  ([[design/capture|capture]]).
- `generalize.rs`: rank-based `generalize`, freshening `instantiate`,
  `settle_weak` (to `Closed`), `reseed_weak`, cyclic snapshots and syntactic
  closure over the whole scheme graph. No environment scan or cached environment free variables
  ([[invariants/schemes-leave-closed|schemes leave closed]]).
- `builtins.rs`: `Decl`, `Manifest`, `BuiltinTypeRule = fn(&mut Binders) -> Ty`
  closed by `Binders::close`, and cached schemes/curry spines. `Manifest`
  clones share declaration rows and their caches; the checker borrows those
  rows. A declaration fixes its arity and ordinary
  result-grade quantifier without the live unifier. Building a base-frame
  declaration asserts that its result payload is ground; result grades may
  still quantify
  ([[invariants/fixed-arity|fixed arity]], [[map/core/builtins|builtins]]).
- `site.rs`: `Site::{snapshot, instantiate, scheme, exports}`. Free identities
  in every sort retain sharing; `Site::scheme` enters a scratch region before
  instantiation, exits, generalises and settles. Module exports share one
  scratch unifier across the record. The graph data is in
  [[map/core/ty|ty]]; runtime admission is `shell/runtime/admit.rs`.
- `contract.rs` and `contract/`: closed declared keysets and held-type rules,
  `ascribe`, nested checks and enum-derived shapes. `ascribe` takes `Settled`
  and the `Manifest`, not the generator; it holds fields and factory equations
  live after elaboration settles. `Held::Unify` uses `equate_held`;
  `Holds::Arms`/`Held::Arms` holds rc and manifest `handlers:` through
  `stands_in` (record labels) or `[String] → Command` (map values).
  It re-reads row tails after each field, then judges final keysets on scratch
  and holds only fields not already held, without replaying live clashes or inference.
  `written_at` supplies written field spans, the tail phrase otherwise.
  `Settled::nested` queues nested tables for the unchanged scratch check at
  close, after live holds. No declared table becomes a unified record type.
- `error.rs`: `TypeError`, `TypeErrorKind`, `Reason`, `Standing`, `UnitCall`.
  `Reason::{Binding, Call}` carry `defaulted_at`, the closing span when a
  default chose the branch, and `Call` whether it is a recursive call; failed
  head/coverage equations retain their source-specific diagnosis.
  `ExecArgNotText` retains whether a head was handled where written, so final
  gate refusal can explain why the handler does not widen the external gate.
- `explain.rs`: the pure wording of errors and hints, including both-use
  default conflicts. `()` notes are recorded during generation and attached
  from the final solution. Final-head static gates do not establish uniform
  runtime `Unit` refusal for arbitrary handlers (`docs/SPEC.md` §6.6).
- Evidence lives in `solve.rs`, `unify.rs`, `generalize.rs`, `site.rs` and
  `readback.rs` tests, and `core/tests/suspended_matches.rs` (whole programs'
  verdicts, elaborations and hints). Scope and remaining gates are recorded in
  [[internals/type-inference|type inference]].

The type language, schemes and site data live below this module in
[[map/core/ty|ty]]. Formal rules: `docs/SPEC.md` §§17.3–17.7.
