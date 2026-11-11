---
generated_at_commit: e09e5acacd6c419dd33338be7efa6919a39901cf
generated_at_date: 2026-11-10
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
  pending phrases, closes the solver, ascribes an optional return contract,
  then reads back a `Toplevel` whose sites and readmissions name one fresh
  `UnitId`. Errors are sorted by span and no checked tree escapes. Re-checking
  a checked tree accepts it and inserts no further elaboration; raw variable
  ids are not canonical, so the two trees need not be `==`.
- `SessionSchemes` carries bindings, alias schemes and the host's `Manifest`.
  `Solver::seed` imports them at rank zero, re-seeding residuals once per unit;
  host bindings without schemes receive one weak monomorphic unknown.
- `bake_prelude` checks against the manifest and returns the annotated prelude;
  `SessionSchemes::from_prelude` reads its exported definition schemes, with
  no separate scheme blob.
- `alias_arm_scheme`, `catch_all_stands_in`, `binding_value_scheme` enter a
  region before checking the body and close before export. `close_thunk_scheme`
  generalises after region exit, then settles only a successful unit. The
  catch-all stores no scheme. Each returns every error from close
  (`Vec<TypeError>`); the runtime reports a refusal as one `IllTyped` fact
  through `TypeError::diagnostic` ([[map/core/diagnostics|diagnostics]]).

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
  boundary saturation, argv, handler/alias arms and installation. `head_class`
  follows binding → value builtin → handler → external. `apply_args` takes
  one `CallHole` and writes left-to-right argument matches, naming the head
  variable when there is one; a bare binding
  head becomes `CompKind::BindingCall`. `argv` queues single-element gates,
  while a spread only demands a list. `arm` equates its slot's parameters and
  producer with what the replaced head provides; `install_arm` owns a region.
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
- `Solver::close` closes rank zero, judges final-head argv gates, checks nested
  tables and attaches unit-read notes. It yields a `Solution` only for a unit
  with no errors, every hole decided; otherwise the sorted errors.
  `Solution` exposes `mode`, `splits`, resolution, sites, settlement and a
  `scratch` unifier for contract ascription, never constraints.
- `readback.rs`: `Readback` turns an `Inferring` tree into a `Checked` one. It
  applies each `BindHole`'s `Mode` (Let-fun, run, capture/decode) and each
  `CallHole`'s splits, turns a `BindingCall` into an `Exec`, snapshots sites,
  settles definition schemes into `Closed`, builds `Toplevel.admits`, and
  memoises rebuilt recursive groups.
- `env.rs`: `TyEnv`, handler metadata, and the `Inferring` phase
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
  closed by `Binders::close`, and cached schemes/curry spines. `Decl` is not
  `Clone`: the checker borrows manifest rows (a `Manifest` clone shares them),
  so each cache is computed once. A declaration fixes its arity and ordinary
  result-grade quantifier without the live unifier
  ([[invariants/fixed-arity|fixed arity]], [[map/core/builtins|builtins]]).
- `site.rs`: `Site::{snapshot, instantiate, scheme, exports}`. Free identities
  in every sort retain sharing; `Site::scheme` enters a scratch region before
  instantiation, exits, generalises and settles. Module exports share one
  scratch unifier across the record. The graph data is in
  [[map/core/ty|ty]]; runtime admission is `shell/runtime/admit.rs`.
- `contract.rs` and `contract/`: closed declared keysets and held-type rules,
  `ascribe`, nested checks and enum-derived shapes. Ascription runs on
  `Solution::scratch()`, one scratch per held row; a factory check holds its
  body to a block of the options in that scratch. No table enters the
  solution.
- `error.rs`: `TypeError`, `TypeErrorKind`, `Reason`, `Standing`, `UnitCall`.
  `Reason::{Binding, Call}` carry `defaulted_at`, the closing span when a
  default chose the branch, and `Call` whether it is a recursive call; failed
  head/coverage equations retain their source-specific diagnosis.
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
