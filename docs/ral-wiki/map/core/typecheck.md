---
generated_at_commit: c4bd558f157dbf49cf244af6c6eda4ba88bf37c7
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

- `typecheck(phrases, SessionSchemes, options: Option<&'static Table>)`
  generates pending phrases, checking a host file's tail as written options
  when a table is supplied (SPEC §15.3). `Solver::close` returns a `Solution`,
  then readback builds a `Toplevel` whose sites and readmissions name one fresh
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
- `arm_stands_in(name, comp, schemes)` enters a region to check a persistent
  handler against `U([String] → Command)` and closes the unit before returning
  an install verdict. `within` needs no runtime vet. It stores no scheme.
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
  bindings alone. A base-frame call has result `Command` without consulting
  its row's scheme. `apply_args` takes
  one `CallHole` and writes left-to-right argument matches, naming the head
  variable when there is one; a bare binding
  head becomes `CompKind::BindingCall`. `argv` queues single-element gates,
  while a spread only demands a list. `arm` generates the block (literal
  bodies use `check_arm_body`), then calls `stands_in`. That is the sole
  rule for written scoped entries, the catch-all and persistent install
  checks: equate the slot shape, then its fresh result with `Command`,
  independent of the name and Σ. `install_arm` owns a region.
- `generate/data.rs`: values, records, maps, primitives, updates and indexing.
  `Key::{Label, Position, Data}` immediately requires a record slot, list or
  string-keyed map. There is no deferred read or index-made weakness
  ([[decisions/261102_a-written-key-is-a-name|a written key is a name]]).
- `generate/control.rs`: joins, `if`, `case`, `try`, `guard`, pipelines,
  redirects and capture. Arms meet fresh results, then equate with the join.
  `Generator::guard` gets the resource type through `scope_body`, checks
  body against one resource argument and a producer tail, and release against
  resource plus `ExitCase::<ErrorRecord>::ty()` and a discarded producer tail.
  Only the body's producer type and grade are returned; `GuardBody` and
  `GuardRelease` explain the operand-specific argument demands. Every pipeline
  stage demands ready-producer shape; non-final stages must write, and decoders
  are refused there. `stage_root_stdin_feed` rejects an overridden pipe edge.
  Stdout redirects retain the payload at a fresh result grade.
- `generate/scope.rs`: `within`, `grant`, options, audit, spawn,
  gather and protection. `file_options` follows elaborated binds and sequences
  to a written tail; only a manifest admits a written one-parameter factory,
  one level deep. `check_options` checks keys with `check_option`, diagnosing
  unknown, refused, duplicate and missing keys during generation.
  `Holds::Table` recurses on a record literal immediately; a bound or computed
  table, map or `()` is T0079. `Holds::At` constrains field values through
  `equate_held`. For `Holds::Handlers`, a `Val::Map` checks each entry through
  `install_arm` in its own region, in forms and host files alike; any other
  value uses the ordinary map equation. Only a written `within` map supplies
  handled names to the body's diagnostics.
  `Holds::CatchAll` checks the catch-all by the arm rule alone.

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
- `Solver::close` closes rank zero and retires every elaboration match,
  judges final-head argv gates and attaches unit-read notes. It yields a
  `Solution` only for an error-free unit; otherwise the sorted errors.
  `Solution` exposes decisions, resolution, sites and scheme settlement,
  never constraints. Its `scratch` copy and `Unifier: Clone` are test-only;
  production table checks all run during generation.
- `readback.rs`: `Readback` turns an `Inferring` tree into a `Checked` one. It
  applies each `BindHole`'s `Mode` (run or capture/decode) and each
  `CallHole`'s splits, turns a `BindingCall` into an `Exec`, snapshots sites,
  settles definition schemes into `Closed`, builds `Toplevel.admits`, and
  memoises rebuilt recursive groups.
- `env.rs`: `TyEnv`, handled-name sets for diagnostics only, and the `Inferring` phase
  (`Bind = BindHole`, `Call = BindingCall = CallHole`, `Boundary = Ty`,
  `Membrane`). `Membrane::{Kept, Imported}` is a protected value's contract
  while inferring: a checked tree's site, or a session binding's solved type
  ([[map/core/ir|IR]]).

## Types, tables and diagnostics

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
- `builtins.rs`: `Decl`, `Manifest`, `Convention::{Value(BuiltinTypeRule), Argv}`,
  and cached schemes/curry spines. Value rules are closed by `Binders::close`
  without the live unifier. `Argv` has no type rule: `Decl::scheme` derives
  `U([String] → Command)` from `Arm::Named(name).ty()`. `Arm::{Named, CatchAll}`
  supplies `shape`, `ty` and diagnostic `standing`; the private `handler()`
  also supplies the member type of `handlers_ty()`. No name or manifest lookup
  selects a handler's type. `Manifest` clones share declaration rows and caches
  ([[invariants/fixed-arity|fixed arity]], [[map/core/builtins|builtins]]).
- `site.rs`: `Site::{snapshot, instantiate, scheme, exports}`. Free identities
  in every sort retain sharing; `Site::scheme` enters a scratch region before
  instantiation, exits, generalises and settles. `Exports::fits` compares each
  used module export's complete scheme against its use, including functions
  nested in records, lists, maps or variants. Exports share one scratch unifier
  across the record. Known incompatible nested schemes fail at `use`; unresolved
  re-exported shapes additionally carry recursive delayed contracts from
  `Site::wrap`, guarding exactly the caller-required arrows and results when
  reached, without inferring function bodies at handler installation. The graph
  data is in [[map/core/ty|ty]]; runtime admission is `shell/runtime/admit.rs`
  and delayed wrapping is `shell/runtime/protect.rs`.
- `table.rs` and `table/`: `Table`, `Key`, `Holds`, `Vocab`, `declared(Form)`
  and enum-derived vocabulary shapes. `Key::required` is independent of
  `Holds`; `Key::blank` supplies declared empty values for diagnostics and the
  rc skeleton, including `` `inherit `` for nested `LIMITS` and `THEME` keys.
  `setting_table!` derives each nested table and its parent's blank from one
  label list. `RC` requires all thirteen keys, `LIMITS` all three child-limit
  keys, and `THEME` both display keys. `setting_row` supplies the closed
  ``[`inherit, `set T]`` vocabulary; `Vocabulary::Setting` identifies its
  diagnostic vocabulary. `MANIFEST` requires only `name` and admits a
  factory; `GRANT` and its nested tables have no required keys.
  `unify_held` and `unify_vocab` word field equations; `Reason::Handlers`
  explains the map spelling and arm mismatches. No declared table becomes
  a unified record type. Runtime decoders keep value-shape and domain checks;
  grant decoding also retains shape and keyset checks for grants received
  as data.
- `error.rs`: `TypeError`, `TypeErrorKind`, `Reason`, `Standing`, `UnitCall`.
  `Unwritten` classifies refused syntax: `OptionsNotWritten` (T0077) for a
  file tail, `TableNotWritten` (T0079) for a nested table. `OptionsContext`
  distinguishes a file's factory admission from a factory body's written
  result, so advice names the right context. Variable advice uses the actual
  variable; spread and computed results use a neutral `$value` example.
  `OptionsMissing` (T0081) lists only missing required keys in one diagnosis
  per table; it does not claim that optional keys are required. Its defaults
  hint appears when every missing key declares a blank.
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
