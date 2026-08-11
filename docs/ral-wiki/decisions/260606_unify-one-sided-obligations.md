---
status: active
---

# One-sided obligations complete the co-inductive unifier

The unifier admits cyclic value and computation types with **no occurs check**
([[internals/type-inference|type-inference]]); termination rests entirely on a
co-inductive guard (`Pairs`) that treats re-entry on an in-progress equality
obligation as immediate success — the cyclic fixed point. That guard memoized
only *symmetric* `Var`/`Var` root pairs. **That is incomplete: the same
equi-recursive type can be anchored at a ty-var on one side and a comp-var on
the other, so unification re-enters as variable-vs-concrete-structure and the
guard never fires** — the descent unrolls until the host stack overflows (a
`--check` crash, not a type error).

The shape that exposed it is the [[design/types|Stream]]. `from-lines`'
producer (`lines_step_ty`) closes the recursion at a **comp-var**:

    C = F Step(C)          // tail thunk wraps a comp-var; the value is inlined

A stream combinator written to take a `Stream` *value* rather than a `Thunk`
(`case $s`, recursing through `!$p[tail]`) closes the *same* recursion at a
**ty-var**:

    T = Step(F T)          // tail thunk wraps a Return whose payload is the ty-var

Unifying `T` against `Step(C)` re-enters as the obligations `T ~= Step(C)`
(ty-var vs concrete `Variant`) and `F T ~= C` (concrete `Return` vs comp-var) —
never a `Var`/`Var` pair. `apply_ty` already survived these via its `Visited`
stack; only unification lacked the matching guard.

**The fix memoizes one-sided obligations too:** a variable root paired with a
finite *structural key* of the other side (`TyKey` / `CompTyKey` / `RowKey`).
The key canonicalizes nested variables through `find` but never expands their
bindings, so it stays finite even when the root is bound to a cyclic structure;
equal keys denote the same obligation against the same anchor — the fixed point
to discharge. This is the regular-tree / bisimulation completion of the
procedure: it **unifies** the two anchorings rather than rejecting them, because
they denote one and the same equi-recursive type.

- **Rows stay inductive.** The recursive equality travels through field *types*
  (ty/comp), which already carry the guard; there is no recursive-row key and no
  recursive row.
- **The thesis holds, the corollary did not.** In CBPV, recursion lives at the
  computation level — the productive cycle closes through `Thunk(F …)`. But the
  *value* cycle can be pinned at a ty-var (a `Stream`-value-taking combinator),
  which the old `apply_ty` comment wrongly called unreachable. It is reachable;
  the unifier now handles it co-inductively rather than the program being
  ill-formed.
- **`PayloadRoute` gains `Hash`** so a resolved route can sit inside a
  structural key. `CompTyKey::Return` carries exactly the one field that
  matters to the fingerprint: `Return(PayloadRoute, Box<TyKey>)`. It once
  carried the pair `Return(PipeMode, PipeMode, Box<TyKey>)` — an `input` and
  an `output`, both since deleted
  ([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).
  **This was the single most dangerous line in the whole payload-route
  change.** The two deleted fields were exactly what the key fingerprinted;
  the route was not in the key at all under the old shape (it lived as one of
  the two `PipeMode`s being deleted, not as a separate slot to preserve). A
  naive deletion — drop the two fields being removed, leave the rest of the
  variant alone — would have produced `Return(Box<TyKey>)`: a key that
  forgets payload route entirely. Two comp types differing only in route
  would then fingerprint identically, so the co-inductive guard this page
  exists to make complete would instead treat re-entry on a route-changing
  cycle as the same obligation already in progress and let it through. The
  compiler would not catch this: every type still checks, every existing test
  still passes, because no test was written to distinguish a route-preserving
  cycle from a route-erasing one — the corner this key exists to guard is
  exactly the corner nothing else exercises. The fix is not a deletion but a
  rewrite: the route must be *added back* as the key's one payload field, not
  merely left over from the pair being removed.

This is what lets the prelude `stream-*` family present `Stream`-value
signatures — `case $s` directly, recursing through the forced tail `!$p[tail]`,
no thunk parameter and no inner helper — because the unifier no longer cares
whether the caller's stream is anchored at a value or a computation variable
(`stream-map $f $s` against a `from-lines` producer is exactly the regression
test).

See also [[design/types|types]], [[internals/type-inference|type-inference]],
[[map/core/typecheck|typecheck]], [[design/cbpv|cbpv]]; judgments in
`docs/SPEC.md` §17.
