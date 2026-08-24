---
status: active
---

# Depth is guarded where it multiplies

**A depth budget belongs to unification, not to every traversal of a type.**
`core/src/typecheck/` carries one ceiling, `MAX_UNIFY_DEPTH = 512` in
`unify.rs`, and the structural walks beside it — `apply_ty`, `free_ty`,
`generalize` — carry none. That asymmetry is the design, not an omission.

Two limits guard the two places where nesting can run away, and they guard
different quantities:

- *Syntactic nesting* is capped at `NESTING_DEPTH_LIMIT = 64`
  (`core/src/syntax.rs`), enforced at both the lexer's delimiter stack and the
  parser's `nested()` chokepoint. A program cannot write a deeper term.
- *Unification depth* is capped at `MAX_UNIFY_DEPTH = 512`, charged by `deeper()`
  on every descent into a strictly deeper subterm. This is the one traversal
  whose depth is not bounded by the source: a substitution composes types the
  program never wrote beside one another, so unification can descend far past
  anything the parser saw. The row occurs check is on this side of the line
  however much it reads like a structural walk: `row_occurs` is called from
  inside `unify_row_inner` and spends the caller's remaining budget, so it
  cannot share a traversal with the walks below.
- *The structural walks* are linear in a type the program built one constructor
  per statement. Their depth is the source file's own length. A budget there
  would re-guard a quantity the source already bounds, at the cost of turning
  `apply_ty`'s `Ty` into a `Result` through every call site.

## The witness that was asked for, and did not appear

A 2026-08-12 review raised the asymmetry as a latent stack overflow: a type deep
enough to need the guard is guarded on only one of the paths that walk it. It was
`PLAUSIBLE`, unreproduced. The reproduction attempt is recorded here so it is not
re-raised without one.

The adversarial shape is a type that nests one constructor per binding and never
sends the deep part through unification, so the structural walks see the full
depth while `deeper()` is never charged:

```ral
let x0 = 1
let x1 = [f: x0]
let x2 = [f: x1]
# … one line per level
```

Run under `ral --check`, release build, default 8 MB stack:

| nesting depth | wall clock | result |
| --- | --- | --- |
| 20,000 | ~1s | exit 0 |
| 50,000 | 54s | exit 0 |
| 150,000 | 11min | exit 0 |

No stack overflow at 293× `MAX_UNIFY_DEPTH`, and no `TypeTooDeep` either — which
is the confirming detail: the unify budget genuinely never fires on this shape,
so the walks were doing all the descending, unguarded, and survived. Reaching a
depth that threatens the stack needs a source file of the same order, and the
cost below makes such a file self-limiting long before the stack is.

## What the measurement surfaced: cost, not safety

**Checking is superlinear in nesting depth** — 3× the depth cost 12× the time.
The shape is consistent with generalisation at every binding walking a type that
has grown one constructor per binding, Θ(N²) constructor visits over the file;
that mechanism is inferred from the growth curve, not profiled.

It is not urgent for the same reason the stack claim is not: a 50,000-deep type
needs a 50,000-line file. It is recorded because it is the reproducible fact the
investigation actually produced, where the finding that prompted it was not.

See [[internals/type-inference|type inference]] for the algorithm these walks
belong to.
