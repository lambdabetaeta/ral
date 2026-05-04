# Equi-recursive value types

## What works today

Computation types are equi-recursive.  A binding such as
`comp_ty_slots[N] = Bound(Fun(Int, Var(N)))` is allowed — the union-find
of comp vars tolerates cycles, and every traversal that descends through
`Thunk` / `Fun` / `Return` carries a `visited: HashSet<u32>` of comp-var
roots in the current expansion so a cycle returns a back-edge instead
of recursing forever.  Unification is co-inductive, carrying a
`HashSet<(u32,u32)>` of pairs already in progress so that unifying two
cyclic comp types reaches a fixed point.

This is what lets the canonical infinite-stream producer typecheck:

    let nats = { |n| step-cons $n { !{nats $[$n + 1]} } }

The cycle here is in *comp-var* space — `nats : Comp β` where
`β := Fun(Int, Return(F(Variant {.more: {head: Int, tail: U(β)}, .done | ρ})))`.
The element type (`Int`) is not recursive; the `U(β)` is just a thunk
of the comp the cycle anchors on.  Comp-level equi-rec handles it and
the test `recursive_stream_producer_typechecks` is green.

## What does not work

Value types are *not* equi-recursive.  The TyVar occurs check rejects
`α := T(α)` for any `T` whose tree contains a free occurrence of `α` —
including occurrences guarded by a `Thunk`.  The relevant code is
`occurs_ty_inner` in `core/src/typecheck/unify.rs:394`:

    fn occurs_ty_inner(&mut self, v: VarTag, ty: &Ty, visited: &mut HashSet<u32>) -> bool {
        match self.resolve_ty(ty) {
            Ty::Var(u) => v.0 == u,
            Ty::List(a) | Ty::Map(a) | Ty::Handle(a) => self.occurs_ty_inner(v, &a, visited),
            Ty::Record(r) | Ty::Variant(r) => self.occurs_row_fields(v, &r, visited),
            Ty::Thunk(b) => self.occurs_comp_inner(v, &b, visited),
            _ => false,
        }
    }

A pre-emptive note: do **not** "fix" this by short-circuiting the
`Ty::Thunk` arm to `false`.  In CBPV `Thunk = U(B)` — a value that
packages a computation — and that wrapper is not a fix-point operator.
The TyVar still occurs free inside the thunk's comp body; pretending it
doesn't yields cyclic value-type bindings that the rest of the pipeline
(`resolve_ty`, `apply_ty_inner`, `apply_row_inner`, `unify_ty_inner`,
`unify_row_inner`) then walks as a tree and recurses into forever.
That's an actual experiment, run on 2026-05-02: removing the Thunk
descent in the occurs check made `recursive_stream_consumer` typecheck
but tripped a stack overflow in `arith_float_mul`, because some other
unification produced a value-type cycle that traversal could not break.

## The test that wants this

`core/tests/typecheck.rs:542` — `recursive_stream_consumer`:

    let drain = { |s|
        case !$s [
            .more: { |p| !{drain $p[tail]} },
            .done: { |_| return unit }
        ]
    }

`drain : U(α) -> F(Unit)` for some α.  Inside, `case !$s` forces $s and
matches on the resulting value; the .more arm extracts `$p[tail] : U(α)`
(it must be a thunk of the same Step the next iteration will force) and
calls `drain $p[tail]`.  That call demands the argument unify with
`U(α)`; the variant scrutinee is `Variant {.more: {head: t, tail: U(α)},
.done | ρ}`; therefore α resolves to that variant — a TyVar bound to a
type containing itself only inside a thunk.

This is the canonical streaming consumer.  There is no way to write it
that anchors the recursion in comp-var space the way the producer does:
`drain` *must* take its argument as a value (a thunk) so it can be
forced and cased upon.  Without value-side equi-rec, the language has
producers but no consumers expressible in source.

## What needs to change

Make value-type space equi-recursive in the same shape as comp-type
space already is.  Concretely, in `core/src/typecheck/unify.rs`:

1. **Allow TyVar bindings to cyclic types.**  Drop the
   `RecursiveType` rejection in `unify_ty_inner` for the bind cases
   (lines 490–510).  *But only as the last step* — first make sure
   every traversal that consumes a TyVar binding terminates on cycles.

2. **Cycle-protect every value-side traversal.**  Add a
   `visited: HashSet<u32>` of TyVar roots threaded through:
   - `resolve_ty` (currently a tail-recursion through `Ty::Var`
     bindings — once bindings can cycle, the recursion no longer
     terminates without a guard).
   - `apply_ty_inner` (line 284).  Mirror the comp-side trick at
     `apply_comp_ty_inner` (line 296): if the input is a `Ty::Var`
     whose root is already in `visited`, return `Ty::Var(root)` as a
     back-edge instead of expanding.  Otherwise mark the root visited
     before descending.
   - `apply_row_inner` (line 336) — once a row's field type is a
     cyclic value type, the row walker will revisit it through that
     field's contents on subsequent expansions.  Already takes a
     `visited` for row-var roots; extend or add another for ty-var
     roots, depending on whether one set across kinds is sound (it
     probably is — the IDs come from disjoint stores).

3. **Cycle-protect unification.**  `unify_ty_inner` already takes a
   `pairs: HashSet<(u32, u32)>` for comp pairs.  Reuse the same set
   (or add a sibling) for `(TyVar, TyVar)` pairs already in progress,
   so unifying two cyclic value types co-inductively reaches a
   fixed-point instead of looping.  Mirrors what
   `unify_comp_ty_inner` already does for comp pairs.

4. **Drop the occurs check on TyVar entirely**, OR keep it as a
   diagnostic for the cases where the cycle is *not* productively
   guarded.  Not strictly required for soundness once traversals are
   cycle-aware, but the comp side keeps no occurs check at all (see
   the comment at unify.rs:21–25), and parity is the simpler
   invariant.  Recommend dropping it.

5. **Row unification cycles.**  `unify_row_inner` and
   `occurs_row_fields` walk row spines and field types.  Once field
   types can cycle, both need the same `visited` discipline.  The
   Rémy-style row rewrite already handles row-var cycles; ty-var
   cycles inside fields are new.

6. **Scheme generalisation and instantiation.**  `apply_*` in scheme
   construction must not loop when a binding is cyclic; instantiation
   must reproduce the cycle in the fresh skeleton.  The comp side
   already does this — see `Scheme` quantifier construction in
   `scheme.rs` and `0dd6fa2 typecheck: quantify schemes over comp-ty
   vars and cyclic bindings`.  Generalising over cyclic ty vars
   should follow the same pattern.

## How to verify

`docker exec shell-dev cargo test -p ral-core` — the only currently-
failing test is `recursive_stream_consumer`; this change should make
it pass.  No other test should regress.  In particular run
`arith_float_mul`, `step_pipeline_rejects_non_recursive_tail`, and
the full `typecheck` suite — those triggered fallout the last time
the occurs check was loosened naively.

A useful integration probe is

    docker exec shell-dev ./target/debug/ral -c '
      let drain = { |s| case !$s [.more: { |p| drain $p[tail] }, .done: { |_| return unit }] }
      drain { !{from-lines < core/src/types/shell.rs} }
    '

It exercises the consumer through a real producer.  (At present this
also crashes for an unrelated reason: case-arm bodies are not in tail
position, so the host stack still blows around 1000 elements even when
the types check.  A separate change adds TCO across case arms; do not
conflate the two.)

## Style

Keep the diff localised to `core/src/typecheck/unify.rs` and
`core/src/typecheck/scheme.rs` (and tests if you add any).  The
existing comp-side machinery is the model — don't invent new shapes,
mirror what's there.  Update the file-level comment at unify.rs:1–15,
which currently states "Value types remain non-recursive — the occurs
check on `TyVar` is preserved": that sentence is the load-bearing
documentation of the old invariant and must change in lock-step with
the code.
