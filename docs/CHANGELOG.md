# ral — spec changelog for implementation migration

This document lists every normative change in the current SPEC.md and
RATIONALE.md relative to the previous version, grouped by subsystem
and annotated with:

- **Where** — which spec section is now authoritative
- **Kind** — rename / refactor / semantic / removal / addition
- **Risk** — low (mechanical) / medium (requires care) / high (touches
  multiple subsystems or changes observable semantics)
- **Action** — what the implementation needs to do

Read all the way through before starting. Section 0 is cross-cutting;
fix it first or you'll waste work.

---

## 0  Read first: terminology and two sigils

Two name changes pervade everything else. Apply these with grep before
anything semantic.

### 0.1  `Lambda` type merged into `Block`

- **Where** §2 value grammar, §20.1 type grammar
- **Kind** refactor
- **Risk** medium — touches runtime value representation, type
  system, serialisation, error messages
- **Action**
  - Remove `Lambda` as a distinct runtime value constructor. One
    category: `Block`. Nullary vs parameterised distinguished by
    parameter count held inside the block value.
  - In the type system, `{B}` covers both cases: a nullary block has
    type `{B}`, a parameterised block has type `{A → B}`. Same type
    constructor.
  - Error messages that previously said "Lambda" should say "Block".
  - In prose and comments, replace "lambda" with "block" (or
    "parameterised block" where arity matters).

### 0.2  `_fail` renamed to `fail`

- **Where** §16.1, §17.1 (`retry`, `reduce`, `case` all use it)
- **Kind** rename
- **Risk** low — grep and replace
- **Action** `_fail` → `fail`. The `_` prefix is now reserved strictly
  for private substrate primitives. No "user-facing exception" rule.

### 0.3  "ral (run, audit, log)" backronym deleted

- **Where** §0
- **Kind** removal
- **Risk** none
- **Action** anywhere your tooling / documentation expands `ral` as
  "run, audit, log", stop.

---

## 1  Grammar changes

### 1.1  Simpler production set

- **Where** §1
- **Kind** refactor (no language change)
- **Risk** low
- **Action** Several productions merged or inlined. Your parser may
  already handle these identically; verify.
  - `carg`/`lelem` unified as `spread = atom | '...' atom`.
  - `primary` folded into `atom`.
  - `return`, `param`, `tilde`, `bypass`, `fd`, `cmpexpr`, `CMPOP`
    inlined into their one use site.
  - No language change — 33 productions → 24.

### 1.2  Pattern grammar **unchanged**

- **Where** §1, §7
- **Kind** confirmation
- **Action** There are **no literal patterns**. Patterns are purely
  structural (`_`, `IDENT`, `plist`, `pmap`). If you previously
  considered adding literal patterns, don't. Literal dispatch uses
  explicit `equal`.

---

## 2  Execution model

### 2.1  Pipeline semantics: always discard non-final returns

- **Where** §4.2
- **Kind** semantic clarification
- **Risk** medium — affects pipeline implementation
- **Action** Pipeline composition connects only the output channel.
  The non-final stage's return value is **always** discarded. On a
  byte edge the channel carries bytes; on a value edge it carries
  structured values, delivered to the next stage as its final
  argument (data-last).

### 2.2  Return-type table reorganised

- **Where** §4.2, §20.3
- **Kind** semantic (streaming reducers clarified)
- **Risk** medium
- **Action**
  - Buffering byte-output (externals, `echo`, `grep`, …) returns
    decoded `String`.
  - **Streaming reducers** (`map-lines`, `filter-lines`, `each-line`)
    return `Unit`, not `String`. They emit line by line and do not
    accumulate.
  - Encoders (`to-X`) return `Bytes`.
  - Decoders, value builtins, ordinary functions return their
    structured value.
  - If your implementation lumped streaming reducers with
    byte-output, split them.

### 2.3  Head dispatch rule

- **Where** §4 rule 2
- **Kind** clarification
- **Risk** low (matches prior behaviour in most implementations)
- **Action** A bound block head: trailing atoms are applied as
  arguments (none for a nullary block) and **the result is forced**.
  One rule, covers both nullary and parameterised.

### 2.4  The `!{…}` idiom and left-to-right hoisting

- **Where** §4 prefix operators
- **Kind** semantic clarification
- **Risk** **high** — if your implementation parsed `!$f $x` as "apply
  f to x", it is wrong. The change here is not in the grammar but in
  the idiom users should write.
- **Action**
  - `!$pred $head` parses as two atoms: `!$pred` (force of the pred
    value, which is ill-typed on a parameterised block) and `$head`
    (a separate atom). This will typically be over-application to the
    surrounding command.
  - To inline a call-and-use-the-result, the correct form is
    `!{$pred $head}` — force of a block containing the application.
    This is the **hoist-evaluate-substitute** idiom.
  - Multiple `!{…}` atoms in one command are hoisted and evaluated
    **left to right**, before the containing command runs.
  - The evaluator should implement this: for each command, scan its
    atoms left to right, evaluate each `!{…}` to a value, then
    assemble the call with substituted values.

### 2.5  Type-checker is mandatory

- **Where** §4.2, §20
- **Kind** removal
- **Risk** low
- **Action** The previous "(under the checker; else runtime errors
  with adapter hint)" hedge is gone. Mode mismatches between stages
  are **type errors**, always. There is no checker-off mode.

---

## 3  Binding

### 3.1  All `let` is `letrec`; SCC elaboration

- **Where** §3 Recursion and generalisation, §20.5
- **Kind** **semantic — new rule**
- **Risk** **high** — changes elaborator
- **Action**
  - Every `let` admits self- and forward-references within the same
    scope. The prelude (and user code) uses forward references
    freely.
  - Implement: partition consecutive `let`s into strongly connected
    components by dependency, elaborate each SCC as:
    - singleton, non-recursive → ordinary `let`, generalise at bind
    - singleton, self-recursive, or multi-member → `letrec`,
      monomorphic within the group, generalise after fixed point
  - The prelude now relies on this (`flat-map` references `concat`
    defined later; `while` recurses on itself; etc.).

---

## 4  Modules

### 4.1  `use-reload` and `clear-use-cache` removed

- **Where** §8, §16.1
- **Kind** removal
- **Risk** low
- **Action**
  - Delete both builtins.
  - Module cache is not invalidated on file change. To pick up
    changes, **restart the process**.
  - Rationale: silent reload allows two versions of a module's
    definitions to coexist in one process (closures captured before
    reload reference the old bindings). Manual-only is the coherent
    answer.

---

## 5  Error handling

### 5.1  `_try` record fields fully specified

- **Where** §10.1
- **Kind** clarification
- **Risk** low — but verify your implementation matches
- **Action** `_try` returns `[ok, value, status, cmd, stderr, line, col]`.
  - On success: `ok=true`, `status=0`, `value=result`, `cmd=""`,
    `stderr=empty Bytes`, `line`/`col` point at `_try` call site.
  - On failure: `ok=false`, `status=failing exit code`, `cmd=failing
    command name`, `stderr=its error output (Bytes)`, `line`/`col`
    point at failing command's source position.

### 5.2  `_try-apply` is a new primitive

- **Where** §16.4, used by `case` in §17.1
- **Kind** addition
- **Risk** medium
- **Action** Add `_try-apply f val` to the private substrate:
  - Applies `f` to `val`.
  - Catches **only pattern-mismatch failures** from destructuring
    `f`'s parameter (all other failures propagate).
  - Returns `[ok: true, value: r]` on successful apply+match, or
    `[ok: false, value: unit]` on mismatch.
  - This requires distinguishing pattern-mismatch errors from other
    runtime errors at the error-kind level, if you haven't already.

### 5.3  Execution tree types clarified

- **Where** §10.3
- **Kind** refactor
- **Risk** low
- **Action**
  - No variant types. Tree nodes are open records with a `kind:
    String` discriminant. Consumers dispatch with `equal` on `kind`
    and access kind-specific fields through row polymorphism.
  - Common prefix: `[kind, script, line, col, children, start, end,
    principal]`.
  - Kind-specific extensions: `command` (cmd, args, status, stdout,
    stderr, value), `scope` (scope, status, value), `capability-check`
    (resource, decision, granted).
  - `_audit` returns `Node`, not `Node α` — the α was gratuitous.

### 5.4  MCP serialisation removed

- **Where** former §10.8 (deleted)
- **Kind** removal
- **Risk** low
- **Action** Delete MCP-specific serialisation code. Delete `ral_run`
  mentions. If you had a separate serialisation path for MCP
  consumption, it's gone.

---

## 6  Capabilities

### 6.1  `net` checks do not emit audit nodes

- **Where** §10.3, §11.4
- **Kind** semantic
- **Risk** low
- **Action**
  - `capability-check` nodes are emitted for `exec` and `fs` checks
    only, not `net`.
  - `resource` field admits `"exec"` | `"fs"`, never `"net"`.
  - Rationale: OS sandbox backends (Seatbelt, bubblewrap) are coarse
    at the per-host level; they can't produce a meaningful per-check
    audit event, so emitting one would be misleading.
  - If your implementation emitted net capability-check nodes, stop.

---

## 7  Unix interface

### 7.1  `ask` fails on EOF

- **Where** §15, §16.1
- **Kind** **semantic change**
- **Risk** medium — changes observable behaviour
- **Action**
  - Previously `ask` returned `""` on EOF (per an earlier spec
    revision). **Now it fails.**
  - An empty line is still the empty string `""`. EOF is now
    distinguishable from empty input.
  - Callers that want to tolerate EOF wrap in `try`.

---

## 8  Dispatch

### 8.1  `case` is purely structural pattern dispatch

- **Where** §4.4, §17.1, RATIONALE "Known pitfalls" and §17.1 comment
- **Kind** **redesign**
- **Risk** **high** — user-facing behaviour change
- **Action**
  - Old `case` (map-keyed handlers with `_` fallback) is gone.
  - New `case val clauses`: clauses is a list of parameterised blocks.
    Each clause is tried in order; `_try-apply` applies it to `val`.
    First successful destructuring wins.
  - **No literal patterns**: value equality dispatch uses explicit
    `equal` inside clause bodies, not pattern literals.
  - **Shape-class limitation (accepted):** all clauses must have
    unifiable parameter types, so you cannot mix list-shape and
    map-shape clauses in one `case`.
  - If no clause matches, `case` fails.
  - Catch-all is `{ |_| … }`.

---

## 9  Prelude bug fixes

### 9.1  `take-while`, `drop-while`, `first`, `filter-lines`

- **Where** §17.1
- **Kind** bug fix
- **Risk** medium — these were genuinely broken
- **Action**
  - Old code had `_if !$pred $head { … } { … }` which over-applies
    the surrounding command and ill-types the force of a parameterised
    block.
  - New correct code uses `_if !{$pred $head} { … } { … }`.
  - Check the rest of your prelude for the same pattern and fix.

### 9.2  `first` fails when no item matches

- **Where** §17.1, RATIONALE "Early exit from `for`"
- **Kind** **semantic change**
- **Risk** medium
- **Action**
  - Previously `first` returned `unit` when no item matched the
    predicate. Now it **fails**.
  - This removes the sentinel-as-not-found type hole (`first`'s
    return type is now cleanly `F α`, not `F (α | Unit)` which ral
    doesn't have anyway).
  - Callers that want to tolerate no-match wrap in `try`.

### 9.3  `return unit` audit

- **Where** §17.1
- **Kind** clarification
- **Risk** low
- **Action** `return unit` is appropriate **only** as a fold
  accumulator placeholder when there's nothing to accumulate
  (`map-lines`, `filter-lines`, `each-line` all use it correctly).
  Anywhere else `return unit` was used as "no result" or "error"
  sentinel, replace with `fail`. See `first` above for the canonical
  example.

---

## 10  Type system

### 10.1  No variant types

- **Where** §20.1
- **Kind** confirmation
- **Risk** none
- **Action** If an earlier revision added variant type formers
  (`<l:A | …>`), they are gone. The type grammar is: primitives,
  `[A]`, `[String:A]`, closed record, open record, thunk `{B}`,
  Handle, type variable, row variable. That's it.

### 10.2  Scoped-label row types (Leijen 2005) — location

- **Where** §20.8 (was misplaced in §6)
- **Kind** refactor
- **Risk** none
- **Action** Type-theoretic content is now in §20.8 where it belongs.
  §6 just states the surface rule (explicit fields take priority)
  and cross-references.

### 10.3  SCC generalisation (cross-reference)

- **Where** §20.5
- **Kind** clarification
- **Risk** low
- **Action** Align §20.5's wording with the SCC rule from §3 (non-
  recursive SCC → generalise at bind; mutually-recursive SCC →
  monomorphic within group, generalise after fixed point). Don't
  introduce a separate "LetRec" construct.

---

## 11  Naming cleanup

### 11.1  `Str` → `String`

- **Where** throughout
- **Kind** rename
- **Risk** low
- **Action** Use `String` uniformly, including in type expressions
  (`[host: String, port: Int]`, `[String:A]`, etc.). `Str` was a
  notational abbreviation that's no longer used.

---

## 12  Minor prose fixes (no implementation impact)

Documentation-only. Implementation doesn't need to change; your own
comments and error messages might.

- §0 says "applied (for a parameterised block) or forced (for a
  nullary block) implicitly". Match this terminology in error
  messages.
- §2 literal-shadowing rule is now anchored to the lexical class
  (numeric BARE tokens recognised as values before name lookup).
- §13.4 "Spawned children die when the host process exits" (replaces
  the ambiguous "follow the host-process lifetime").

---

## Recommended migration order

1. **§0.1, §0.2, §0.3** — renames (mechanical grep).
2. **§11.1** — `Str` → `String`.
3. **§1.1** — grammar refactor (verify parser still passes all
   existing tests; no behaviour change).
4. **§2.1, §2.2** — pipeline semantics. Test the return-type table
   entry for every kind of stage.
5. **§3.1** — SCC elaboration. This is the highest-risk change in
   the elaborator. Write tests for the prelude: every
   forward-reference should elaborate cleanly.
6. **§2.4** — verify the `!{…}` hoisting semantics match. Write tests
   for the hoisting order with observable side effects.
7. **§5.2, §8.1** — add `_try-apply` primitive, rewrite `case`. These
   are paired.
8. **§9.1, §9.2** — prelude bug fixes.
9. **§7.1** — `ask` behavioural change. One test.
10. **§4.1** — delete `use-reload`, `clear-use-cache`.
11. **§5.4** — delete MCP serialisation.
12. **§6.1** — stop emitting net capability-check nodes.
13. **§5.3, §10.1** — verify tree types, confirm no variants.
14. **§12** — doc pass.

---

## Tests to write regardless

If you have none or few, here's the minimum regression fence for the
changes above:

- `first` on empty list → failure, caught by `try`.
- `ask` receiving Ctrl-D → failure, caught by `try`. `ask` receiving
  an empty line → returns `""`.
- `case` with a list-shape catch-all `{ |_| … }` actually catches.
- `case` mixing list-pattern and map-pattern clauses → compile-time
  type error (shape-class limitation).
- `!{$f $x}` as a command argument evaluates `$f $x` and substitutes
  its result. `!$f $x` parses differently (probably produces an
  error).
- Two `!{…}` atoms in one command evaluate left to right (test with
  observable side effect ordering).
- A forward reference in a `let` group resolves (e.g. `flat-map`
  calling `concat`).
- `_try` returns the correct `line`/`col` on both success and
  failure paths.
- Streaming reducers (`map-lines`) return `Unit`, not `String`.
- External command in a pipeline terminated with `| from-bytes`
  binds `Bytes`, not `String`.
