# The IR is pure call-by-push-value

The intermediate representation is structurally pure
[[design/cbpv|call-by-push-value]]: values, computations, force and thunk, and
the dedicated nodes for the five [[design/control-operators|control operators]].
**A surface convenience whose unfolding is purely syntactic does not survive
into the IR** — it unfolds at elaboration into the existing primitives:

- currying sugar `{ |x y| M }` lowers to nested lambdas;
- a control operator lowers to its structural IR node, not a name-keyed builtin;
- the byte/value pipeline forms lower to one pipeline node, the byte-vs-value
  distinction recovered by mode inference.

Two conveniences are resolved by *runtime values* and therefore stay in the IR
as their own constructors, not as unfoldings:

- a spread is a value-level element constructor (`ValListElem::Spread`,
  `ValMapEntry::Spread`, spread `Args`) — how many slots it fills depends on
  the length of the spread value;
- string interpolation is a computation node (`CompKind::Interpolation`)
  whose part-computations evaluate and concatenate.

The elaborator is the one place that knows about surface forms; everything
downstream — typechecker, evaluator — sees only the IR's fixed constructor set.

This keeps the core small and the semantics regular. When adding a surface
convenience, lower it during elaboration into what the IR already has; the IR
grows a constructor only when the convenience's meaning depends on runtime
values.

See also [[invariants/fixed-arity|fixed-arity]]. Cite: `docs/SPEC.md` §3, §17.1 (grammar /
scope-stage), §IR.
