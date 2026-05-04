# Variants, recursive computation types, and streamable Step in ral

## Context

ral has row-polymorphic *records* but lacks tagged sums and recursive computation types. Three real gaps follow:

1. **No tagged sums.** `try` already encodes its outcome as a fixed record `{ok: bool, value, …}` (`core/src/builtins/control.rs:90-102`) — a workaround for the missing variant. Anything with multiple structured outcomes (`.ok`/`.err`, `.some`/`.none`, parsed git-status, k8s pod states) has no clean type.
2. **No recursive types.** Self-referential function types fail unification's occurs check, and there is no way to write a stream protocol type at all.
3. **`PipeMode::Values(A)` is the only typed-stream story** and it is bespoke pipeline-runtime machinery rather than a first-class language feature. Demand-driven streams over typed protocols are not expressible.

The plan adds row-polymorphic variants (dual to records, sharing one `Row` machinery), a `case` expression to eliminate them via a tag-keyed record of handlers, equi-recursive computation types per the previously-approved 240409 plan, and a `Step A` stream protocol in the prelude with demand-driven pipeline integration. Each phase merges cleanly on its own; the user can stop after any phase.

The full vision unblocks ergonomic structured shell scripting — parsing porcelain output, classifying per line, take-while-and-bail — without bash's textual-prefix-encoding sins.

### Two key design decisions threaded through the plan

- **The eliminator is named `case`, taking a variant and a tag-keyed record of handlers.** The existing prelude `case` (runtime pattern dispatch on parameterised blocks, SPEC §17, `prelude.ral:153`) is *deleted outright* in Phase B. ral has not been deployed, so the rename-and-deprecate dance is unnecessary. The two existing call sites (`tests/lang/conditionals.ral` and `tests/lang/control-flow.ral`) rewrite trivially to `if`/`elsif` chains since their bodies don't exercise pattern dispatch.
- **Records accept tag-keyed rows in addition to bare-keyed rows.** Two disjoint row alphabets coexist: bare keys (`[host: "db", port: 5432]`) for ordinary records, and tag keys (`[.ok: A, .err: String]`) for variant types and handler tables. The `Row` data structure is shared; the typechecker keeps the alphabets disjoint at unification. This is what makes `case $e [.ok: f, .err: g]` symmetric with the construction `.ok 5`.

## Phase A — Variants and tag-keyed records. Tier M.

Add `Ty::Variant(Row)` types and `.tag value` construction. Allow `.tag` as a key in record literals, producing tag-keyed records (`Ty::Record(tag_row)`). Variant construction is useful on its own; elimination is in Phase B.

**Files to modify:**

- `core/src/typecheck/ty.rs:81-105` — add `Ty::Variant(Row)` next to `Record(Row)`. The `Row` data structure is unchanged; tag/bare distinction lives in label spelling and is enforced at unification.
- `core/src/typecheck/unify.rs:303-349` (`unify_ty`) — add Variant arms parallel to Record. Reject mixed-alphabet record unification (a row whose first label is `.foo` does not unify with a row whose first label is `foo`). `unify_row` (`:353-399`) reused unchanged once the alphabet check is at the call site.
- `core/src/typecheck/unify.rs:209-254` (`apply_ty`), `:256-301` (`occurs_ty`) — Variant arms.
- `core/src/typecheck/{env,generalize,fmt}.rs` — mirror Record arms in free-vars, generalize/instantiate, and pretty-printing. Render variants as `[.l: A | .m: B | ρ]` (with `|`) and tag-keyed records as `[.l: A, .m: B]` (with `,`) so they're distinguishable in errors.
- `core/src/lexer.rs:422-427` — extend the `.` branch (currently only `...` Spread): `.` followed by ident-start emits `Token::Tag(String)`; otherwise existing Spread/error path. Add `Token::Tag` near `Token::Pipe` (`:122`) and `Token::Spread` (`:133`).
- `core/src/ast.rs` — add `Ast::Tag { label, payload: Option<Box<Ast>> }` for variant construction. Allow `MapEntry` keys to be `Token::Tag` as well as bare idents (the AST's existing key field becomes a `RecordKey { Bare(String), Tag(String) }` enum).
- `core/src/parser.rs:571` (`parse_primary`) — `Token::Tag` arm: payload is the next adjacent atom on the same line (`{`, `[`, ident, `$ref`, literal); bare `.tag` is nullary. Adjacency convention matches the existing `!` bang (parser.rs:783). In record-literal parsing (`:904-939`), accept `Token::Tag` as a key in addition to bare idents; flag mixing tag and bare keys in one literal as a parse error.
- `core/src/elaborator.rs:444-451` — `Ast::Tag` → `Val::Variant(label, payload)`. Tag-keyed `Ast::Map` → `Val::Map` with the tag-keyed alphabet preserved.
- `core/src/typecheck/infer.rs:528` (`infer_val`) — Variant construction: returns `Ty::Variant(Row::Extend(l, A, Var(ρ)))` (open). Tag-keyed record literal: returns `Ty::Record(Row::Extend(.l, A, …, Empty))` (closed; same as the bare-keyed record rule).
- `core/src/types/value.rs:30-45` — add `Value::Variant { label: String, payload: Option<Box<Value>> }`. Update `type_name`, `Display`, `PartialEq`. `Value::Map` continues to back tag-keyed records — the alphabet distinction lives in the type, not the runtime.
- `core/src/evaluator/expr.rs` — eval `Val::Variant` → `Value::Variant`.

**Tests** (new `ral/tests/variants.rs`):

- `let x = .ok 42; $x` — typechecks at `[.ok: Int | ρ]`, prints `.ok 42`.
- `[.ok 1, .err "x"]` — list element type unifies to `[.ok: Int | .err: String | ρ]`.
- `[.dev: 8080, .prod: 443]` — tag-keyed record, type `[.dev: Int, .prod: Int]`.
- `[.dev: 8080, port: 443]` — parse/type error: mixed alphabet.
- `$[.ok 1 + 1]` — `TyMismatch` (`expected Int`).
- `.foo` (nullary) typechecks at `[.foo: Unit | ρ]`.

## Phase B — `case` (sum eliminator). Tier M.

Surface form, symmetric with construction:

```
let r = .ok 5

case $r [
  .ok:  { |x| return $x },
  .err: { |m| return -1 }
]
```

The handler table is an ordinary tag-keyed record whose values are lambdas. `case` is a typing-rule builtin (like `if`) — its scheme cannot be expressed as an ordinary HM polymorphic function because the row of the variant must be coupled label-by-label to the row of arrows in the handler table.

**Step 1 — delete the existing `case`.**

- `core/src/prelude.ral:149-162` — delete the `let case = …` binding and the surrounding `# Dispatch` comment block.
- `docs/SPEC.md` §17 — rewrite to describe the new `case` (sum eliminator) introduced in Step 2. Update §3/§7/§19 cross-references.
- `tests/lang/conditionals.ral:24,34` and `tests/lang/control-flow.ral:18` — rewrite the `case $action [{ |v| if !{equal $v …} … }]` blocks to plain `if`/`elsif` chains; their bodies already use `if/equal` internally and don't exercise pattern dispatch.
- Verify no other call sites: `rg -nP '\bcase\s+\$' core/ ral/ exarch/ data/ plugins/`.

**Step 2 — add `case` as a builtin.**

- `core/src/typecheck/builtins.rs` — register `case` with a custom typing rule (parallel to `if` if that's where conditional typing lives). The rule:
  - Infer scrutinee → `Ty::Variant(row_v)`.
  - Infer handler table → `Ty::Record(row_h)` with tag-keyed alphabet.
  - For each label `.l` in `row_v` with payload `A_l`, look up `.l` in `row_h` and unify the table value's type with `A_l → B` (fresh `B`, shared across labels).
  - Closed in v1: `row_v` and `row_h` must have identical label sets; `unify_row` after the per-label connection rejects extras on either side. Open rows (default arm) deferred.
  - Result type `B`.
- `core/src/ir.rs:174-249` — add `CompKind::Case { scrutinee: Box<Comp>, table: Box<Comp> }`.
- `core/src/elaborator.rs` — translate the `case` form (parsed as an ordinary application of the `case` builtin) into `CompKind::Case`. Choice point: do we parse `case` as a special form or as an ordinary builtin call? Recommended **special form in the parser** so that the table argument is parsed with whatever ergonomics we want, and so that error messages can say "case expected a tag-keyed handler table." See parser change below.
- `core/src/parser.rs` — when the head identifier is `case` and is followed by an expression and a record literal on the same form, parse as a `case` AST node rather than an ordinary call. This keeps the surface from drifting if we later want shorthand sugar.
- `core/src/evaluator/expr.rs` (and possibly a sibling `core/src/evaluator/case.rs`) — runtime: evaluate scrutinee to `Value::Variant`, evaluate table to `Value::Map`, look up handler by label, force/apply with payload. After typechecking the lookup is total; an internal-error if not.
- `core/src/typecheck/scheme.rs` — add `CaseNotExhaustive { missing, extra }` and `CaseLabelTypeMismatch { label, expected, found }` to `TypeErrorKind`. Wire `Display` impls.

**Decision: do not extend `let` patterns to variants.** A `let .ok x = $e` would force the row closed to a single label, almost never what users want. Force them through `case`.

**Tests** (`ral/tests/variants.rs`):

- Exhaustive: `case .ok 5 [.ok: { |x| return $x }, .err: { |_| return -1 }]` → `5`.
- Missing arm: `case .ok 5 [.ok: { |x| return $x }]` against scrutinee row `[.ok: Int, .err: String]` → `CaseNotExhaustive { missing: [.err], extra: [] }`.
- Extra arm: `case .ok 5 [.ok: …, .err: …, .nope: …]` → `CaseNotExhaustive { extra: [.nope] }`.
- Arm payload mismatch: handler at `.ok` typed `String → B` against payload `Int` → `CaseLabelTypeMismatch`.
- Arms disagree on result: handlers returning different `B` → `TyMismatch`.
- Rewritten tests (`tests/lang/conditionals.ral`, `tests/lang/control-flow.ral`) still pass after their old-`case` blocks become `if`/`elsif` chains.

## Phase C — Equi-recursive computation types. Tier L (mostly mechanical).

Body of work: **`dev/docs/240409_recursive_types.txt`**. Plan is still actionable; only file paths have moved as the typecheck module was split.

| 240409 step | New location |
|---|---|
| 1, 8, 9 (Scheme + generalize/instantiate) | `core/src/typecheck/scheme.rs`, `generalize.rs` |
| 2-6 (cycle-aware traversals) | `core/src/typecheck/unify.rs` (apply, occurs), `infer.rs:181 apply_piped_value` |
| 10, 11 (remove comp occurs check, co-inductive guard) | `core/src/typecheck/unify.rs:411-425` |
| 12 (display) | `core/src/typecheck/fmt.rs` |

**Interactions with A/B:** Phase A's new `Ty::Variant(Row)` and tag-keyed `Ty::Record(Row)` arms in `apply_ty`, `occurs_ty`, free-vars get cycle-aware `_inner` versions when C lands — same shape as the Record arms. **Variant rows do not need recursion**: μ binds at kind `CompTy`, never at `Ty`. The streamable cons-list lives in `CompTy` with `Thunk(Variant(...))` as its payload (Phase D). `unify_comp_ty`'s co-inductive pair guard does not interact with `unify_row`.

**Tests** add over the 240409 plan's 7:

- Recursive consumer: `let drain = { |s| case !$s [.more: { |p| drain $p[tail] }, .done: { |_| return unit }] }` typechecks.
- Cyclic display: a mismatch involving a cyclic comp type errors with finite output (no stack overflow in `fmt`).

## Phase D — `Step A` in prelude + demand-driven streaming. Tier L. Depends on C.

`Step A = μs. [.more: {head: A, tail: Thunk s} | .done]` — defined entirely by use in the prelude; no new keyword, no new kind. Recursion is closed by Phase C's union-find cycles. Tag-keyed records (Phase A) underlie the variant; `case` (Phase B) eliminates each step.

**Prelude additions** (`core/src/prelude.ral` near existing streaming helpers ~line 270):

- Constructors: `step-cons`, `step-done`.
- Combinators: `step-take`, `step-map`, `step-fold`, `step-each`, `step-into-list`. Each is a self-recursive thunk that `case`s on a forced `Step` and recurses through the `tail` thunk. They typecheck without annotation thanks to Phase C.

**Pipeline integration** (the iterator-protocol bit):

- `core/src/typecheck/infer.rs:181` (`apply_piped_value`) and `:563-606` (`infer_pipeline`) — add a structural recogniser `is_step_type(&Ty) -> Option<Ty>` that returns the element type when a type unifies with the `Step τ` shape (read-only; no mutation). When a producer's output is `Step τ` and the next stage is a function expecting `τ → β`, propagate `τ` (the element type) as the piped value type rather than the whole `Step`.
- `core/src/evaluator/pipeline.rs` and/or `invoke.rs:30` (`push_upstream`) — runtime adapter: when the typechecker has resolved the producer to `Step τ` and the consumer expects `τ`, force the producer to a `Value::Variant`, walk `.more`/`.done` matching `tail` thunks, push each `head` through the consumer in turn. Demand-driven: the consumer's loop is the driver; producer suspends in unforced thunks. Stopping the consumer → tail thunks GC'd → producer's unforced suffix never runs (Haskell-laziness; correct for `seq | head 5`).

**Recommended:** structural recognition of Step's shape, not nominal binding-by-name. Any user-defined recursive variant of the same shape is also streamable; ral does not privilege a prelude name.

**`PipeMode::Values(A)` is not retired in this phase.** The codecs path (`core/src/builtins/codecs.rs`: `from-json`, `from-lines`, `each`) keeps working unchanged. Step is a parallel addition; deprecation is a separate effort after coexistence soak.

**Tests:**

- Manual build: `let s = step-cons 1 { step-cons 2 { step-done } }; step-into-list $s` → `[1, 2]`.
- Lazy: `step-take 3 (nats 0)` terminates with infinite producer.
- Pipeline scalar consumer: `step-source | { |x| echo $x }` runs per element.
- Recursive type inference: `let nats = { |n| step-cons $n { nats $[$n + 1] } }` infers `Int → F (Step Int)`.
- Regression: existing `from-lines | each { |l| echo $l }` unchanged.

## Open design questions to resolve during implementation

1. **Phase A** — `.tag(payload)` parens form? Recommended **no**; whitespace-adjacent atom only, mirroring `!atom`.
2. **Phase A** — error messages for mixed-alphabet record literals: parse-time or type-time? Recommended **parse-time** (clearer, earlier).
3. **Phase B** — open rows / default arm. Recommended **deferred to a v2**. Add a sibling builtin `case-or` taking a default thunk if it earns its keep.
4. **Phase B** — `case` as parser-special-form vs ordinary builtin call. Recommended **special form** for better error messages.
5. **Phase D** — structural Step recognition vs nominal? Recommended **structural**.
6. **Phase D** — `PipeMode::Values(A)` deprecation? Recommended **not in this plan**; revisit after one minor release of coexistence.
7. **Long-tail (no phase)** — migrate `try` to return a variant `[.ok: A | .err: ErrorRec]` instead of its current fixed record. Tracked separately; orthogonal to this plan but a natural follow-up once variants land.

## Phase ordering & merge plan

A → B → C → D, each independently mergeable. A is shippable on its own (variants + tag-keyed records flow through pipelines and integrate with future `try` migration). C depends on nothing in A/B but must land before D. D is the largest user-facing payoff but requires the full stack.

Effort: A ~3-5 days; B ~1 week including the old-`case` deletion and test rewrites; C 1-2 days mechanical lift over the 240409 spec; D ~1 week including prelude + runtime adapter + tests. Total ~3-4 weeks of focused work for the full vision.

## Verification

For each phase:

1. `docker exec shell-dev cargo build 2>&1 | tail -10` — clean compile.
2. `docker exec shell-dev cargo test 2>&1 | grep -E '^test result|FAILED'` — all green, including the phase's new tests.
3. `docker exec shell-dev cargo clippy --all-targets 2>&1 | tail -20` — no new warnings.
4. End-to-end: `docker exec shell-dev ./target/debug/ral` and run the example scripts from each phase's tests interactively.
5. After Phase B: confirm `retry`, `try`, `attempt`, and the rewritten test files in `tests/lang/` still pass.
6. After Phase D: confirm the canonical example (parse `git status --porcelain -z` into a stream of typed entries, `case` per arm, `take 3` early-exits) runs end-to-end. This is the user-facing acceptance test for the whole arc.

## Critical files

- `core/src/typecheck/ty.rs` — type constructors; new `Ty::Variant`, tag-keyed alphabet
- `core/src/typecheck/unify.rs` — row + comp-type unification, occurs, apply, alphabet check
- `core/src/typecheck/infer.rs` — inference rules; `case` rule; `Step` recognition (Phase D)
- `core/src/typecheck/{env,generalize,fmt,scheme,builtins}.rs` — supporting visitors, errors, builtin registry
- `core/src/lexer.rs` — `Token::Tag`
- `core/src/parser.rs` — `.tag` syntax; tag-keyed record literals; `case` parser form
- `core/src/ast.rs` — `Ast::Tag`, record key alphabet
- `core/src/elaborator.rs` — `Val::Variant`, alphabet preservation
- `core/src/ir.rs` — `CompKind::Case`
- `core/src/types/value.rs` — `Value::Variant`
- `core/src/evaluator/{expr,case,pipeline,invoke}.rs` — runtime dispatch (new `case.rs` sibling)
- `core/src/prelude.ral` — delete old `case` (Phase B); add `Step` combinators (Phase D)
- `tests/lang/conditionals.ral`, `tests/lang/control-flow.ral` — rewrite old-`case` blocks to `if`/`elsif` (Phase B)
- `dev/docs/240409_recursive_types.txt` — Phase C body of work
- `docs/SPEC.md` — document each phase; rewrite §17 to describe the new sum eliminator (Phase B)
