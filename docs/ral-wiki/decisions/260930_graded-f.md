---
status: accepted
generated_at_commit: 1776d222
supersedes: decisions/260930_capture-is-decided-by-syntax
---

# A command's value is its output: `F` carries a grade

**`F` carries a grade. `F^p A` produces a value `A` (printed `Returns A`);
`F^w Unit` produces output and nothing else (printed `Command`). Where a value is
demanded of a command, the command is captured and its output decoded as a
`String`; where a command is demanded, a `()`-producer runs as one.** The fact
"this computation's result is its output" lives in the type, so it passes through
abstraction, application and variables, and β/η preserve meaning.

## What was decided

- **The grade is a producer kind, not an effect.** `{ echo pre; return 5 } : F^p
  Int`: the grade says how the result is produced, not whether anything is
  written. That keeps the bind rule total and the arithmetic small.
- **Types.** `CompTy::Return(Grade, Ty)`, `Grade::{Value, Output, Var}`,
  `CompTy::pure` is `F^p`, `CompTy::command()` is `F^w Unit`. A grade variable
  prints from `ν ξ ο π`; `explain retry` is `∀α ν. Integer → {ν α} → ν α`.
- **Invariant `F^w A ⇒ A = Unit`.** Rule-enforced, not representational: every
  rule that introduces `w` does so at `Unit`, every rule that copies a grade
  copies the value type, every builtin scheme pairs a grade variable with one
  value variable. Grades are atomic (a grade variable binds only `p` or `w`), so
  there is no occurs check, no kind, and a grade is never weak.
- **Introductions.** Externals, `^name` and path heads are `Command`; so are
  `echo`, `to-json`, `to-string`, `to-line`, `to-lines`, `to-jsonl`, `to-csv`,
  `to-bytes`, `ints-to-bytes`, `help`, `explain`, `clear` and `reset`. `warn` is
  `Returns Unit` (stderr is not the byte channel). `spawn`/`watch`/`service`,
  `each`, `fold`, `fold-lines`, `audit` and `defer` are polymorphic in their
  body's grade and absorb it; `fail`, `exit` and `diverges` inhabit every grade;
  `map`, `filter` and `sort-list-by` demand a value.
- **The bind rule.** `M : F^p A` binds `A`; `M : F^w Unit` binds `String`,
  elaborated `cap M to d. decode d`; `M : F^ε̂ A` sets `ε̂ := p`. A `let` is a
  demand for a value, and this is the only defaulting in the system; there is no
  settle phase for grades. So `let x = M` is `let x = M | from-line` for any `M`
  that is a command.
- **One run-time coercion, one upcast.** `cap : F^w Unit ⇝ F^p String`, inserted
  only in checking mode, at a demand already resolved to `F^p`: a bind, an
  argument (a callee parameter ending in `F^p β` at the same arity against an
  argument ending in `F^w Unit`; a block in hand is η-wrapped), a join that
  settles on `p`. The upcast `F^p Unit ⇝ F^w Unit` is identity at run time and
  is admitted at joins, handler arms and arguments; from `F^p Unit` there is no path
  to `F^p String`, so the two coercions are coherent. Elsewhere a later
  demand is an ordinary `CompTyMismatch` whose hint names the fix.
- **Joins take two passes.** Infer every arm against a fresh result, resolve,
  then: any `F^p A` arm with `A` not `Unit` (or unresolved) makes the target
  `F^p`, capturing each `F^w Unit` arm; else any `F^w Unit` arm makes it
  `F^w Unit`, upcasting `F^p Unit` arms; else unify. `if $c { hostname } else {
  echo b }` is one `Command`; `if $c { echo $x } else { warn skip }` is a
  `Command` in any position; `try { fetch } { |e| return 'none' }` binds
  `fetch`'s output.
- **Pipes and redirects.** A stage before the last is accepted at `Unit` in any
  grade. A decoder mid-pipeline is refused by its own mark (T0078
  `DecoderMidPipeline`), not by grade. A stdout redirect discharges the grade:
  `M > f : F^p A`, so `let x = to-json 1 > f` binds `()`; stdin and stderr
  redirects and `2>&1` preserve it.
- **Scopes and handlers.** `within`, `grant` and `guard` pass the body's grade
  through; `audit` absorbs it and returns `F^p Report(α)`. An arm for an external
  stands in at `[String] → Command`; a `()`-returning stub is admitted and
  captures `""`; the installed scheme carries the head's producer kind.
- **The tail `Run` is never coerced.**
- **Deleted.** `capture_sites`, `capture.rs`, `head_writes`, `stage_returns`,
  `tail_writer`, `arm_writer`, `Output`/`with_output`/`output:`,
  `Inferencer::captured` and the `writer` field of the join reasons. `HeadClass`
  moves to `infer.rs`.

## The twelve decisions

1. Annotation on `F`, not a `Cmd` constructor: `Val` stays trivial and grade
   polymorphism is ordinary let-polymorphism.
2. Tail grading, not effect union: union grading needs a lub at every `;`, hence
   constraints, and would reopen "value or output?" for `F^w Int`.
3. One run-time coercion `F^w Unit ⇝ F^p String`, at demands resolved to `p`, in
   checking mode; one zero-cost upcast `F^p Unit ⇝ F^w Unit`, at joins,
   handler arms and arguments; never `Command ⇝ Returns ()`, which is incoherent with the
   first.
4. Bind defaults an unresolved grade to `p`; callers meet the demand at the
   argument. No pending constraints.
5. Pipe stages accept any grade at `Unit`; the decoder refusal is by the
   decoder's mark.
6. Redirects discharge.
7. `map`/`filter`/`sort-list-by` demand a value; `each`/`fold`/`fold-lines`/
   `spawn`/`watch`/`service`/`audit` absorb. `par` follows `map` by inference.
8. Handler arms take the head's producer kind.
9. Whole-RHS capture: `let x = M ≡ let x = M | from-line`.
10. The tail `Run` is never coerced.
11. Printing: `Returns A`, `Command`, `ν A`; no `F^` in user-facing text.
12. No compatibility: the wire and baked-prelude formats change freely.

## Why

The old design put the fact in a syntactic walk of the `let`'s right-hand side
([[decisions/260930_capture-is-decided-by-syntax|capture-is-decided-by-syntax]]),
so `let f = { echo hi }; let x = f` bound `()` and printed, `map { |f| echo $f }
[1, 2]` gave `[(), ()]`, `let x = !{ echo pre; echo host }` bound only `"host"`,
and a redirected writer silently captured `""`. Three walkers (`capture_sites`,
`stage_returns`, `tail_writer`) approximated one missing fact. A grade in the
type turns the walkers into ordinary unification and makes the user-visible rule
a single sentence.

## What it costs

- **A per-call wrapper.** A coerced block in hand is η-wrapped, allocating a
  closure per coerced argument, only where a value was demanded of a command.
- **Buffer limits reach more.** The 16 MiB capture cap and strict UTF-8 decode
  now apply to whole right-hand sides and to each `map` element.
- **Stored sites.** A `Site` shape now carries a grade; a seed from an older
  binary does not decode (accepted, decision 12).

## The known limitation

Joins are decided in program order with no pending constraints, so the verdict is
principal on the stated rule but not order-independent:
`{ |t| if c { echo hi } else { !$t }; let w = !$t; $w }` types `t : {Command}`,
while the same block with the `let` first types `t : {Returns String}`.

## Supersedes

[[decisions/260930_capture-is-decided-by-syntax|capture-is-decided-by-syntax]]:
who places the capture changes, from a syntactic walk to the type. Its
statements that pipes stay positional, that an arm stands in for what it
replaces, and that `{ … }` is a thunk in every position stand.

The plan is `dev/docs/plans/260930_graded-f.md`.
