---
generated_at_commit: 22b85799e5e435dff38ea1a2e7ec56d443d9f62b
generated_at_date: 2026-11-11
covers_paths: [core/src/ir.rs, core/src/ir/]
---

# Map: core / IR

`core/src/ir.rs` is the [[design/cbpv|call-by-push-value]] intermediate
representation — the target of [[map/core/elaboration|elaboration]] and the input
to the [[map/core/evaluator|evaluator]].

A whole program is a `Toplevel { phrases, admits }`: `admits` holds imported
bindings' first-use contracts for readmission before execution; each
`Phrase` — `Define` (a top-level `let`, one closed `Scheme` per name the
pattern binds,
[[decisions/260603_session-scheme-continuity|session-scheme-continuity]]) or
`Run` (anything else) — runs in order, extending the
session environment the next phrase sees. `Toplevel::referenced_names` is
the phrase-level analogue of the `Comp`-level walk below, for the same lease
ledger.

**The tree is indexed by a phase.** `Val`, `CompKind`, `Prim`, `Exec`, their
operand types and shared-node aliases take `P: Phase = Checked`, a trait with
one associated type per annotated position: `Bind` (a `CompKind::Bind`'s or
`Phrase::Define`'s right-hand side), `Call` (a `CompKind::App`),
`BindingCall` (a `CompKind::BindingCall`), `Boundary` (`Exec.site`, an
`Option<P::Boundary>`) and `Membrane` (`Val::Protected.site`). `Checked`, the
phase the evaluator runs, has `Bind = Call = ()`, an uninhabited
`BindingCall` and `Boundary = Membrane = Arc<Site>`. The checker's own
`Inferring` phase holds holes and types there
([[map/core/typecheck|typecheck]]); readback alone turns it into `Checked`.
`CompKind::Protect.site` is `Arc<Site>` in every phase. `Serial` is the phases
whose trees serialise: the skipped annotations default, sites cross. `Toplevel`
is always checked. `Prim::map_operands` converts `Prim<P>` to `Prim<Q>`,
preserving operand spans; inference's individual sub-rules retain their own
operand order.

Values, computations and shared nodes:

- `Val` — inert data: `Unit`, `String`, `Int`, `Float`, `Bool`, lists, records,
  maps, variants, thunks, variables. A value can never diverge or perform I/O,
  and forming one costs O(text), never O(data): `Val::Thunk(Arc<ThunkNode>)`,
  `Val::List(Arc<ListNode>)`, `Val::Record`, and `Val::Map` (each
  `Arc<FieldsNode>`, entries sorted by key, stably, at elaboration) hold only a
  **plain** literal — no spread, no computed key. `Val` itself stays
  unspanned; every position onto which the checker narrows while emitting a
  constraint carries `Spanned<Val>`.
- `Node<S> { shape: S, occ: Occ }` is IR that closes: a shape plus what it
  mentions, computed once, where the node is built. `Node::new(shape)` walks
  `shape` through `Occ::of`, which sorts and dedups its `Mentions`, and hands
  back the `Arc<Self>` every holder shares; `shape()` lends the shape back,
  `occ()` (`pub(crate)`) the occ. `ThunkNode = Node<Arc<Comp>>`, `GroupNode =
  Node<Box<[(Name, Arc<Comp>)]>>`, `ListNode = Node<Box<[Spanned<Val>]>>`,
  `FieldsNode = Node<Box<[(Name, Spanned<Val>)]>>`. `PartialEq` compares
  shapes; serde carries the shape alone (`Node::from`/`Into` the shape), so
  decoding a node off the wire recomputes its occ rather than trusting one
  that rode along — `Val`'s own tests (T4) check a node crosses as its shape.
- `Occ(Arc<[Name]>)` is a node's mentioned names, sorted and distinct by
  construction. `Occ::of(shape)` is the shared builder for nodes and boxed
  residual lambdas. `len` and `contains` (binary search) support lookup
  without scanning.
- `Name = Arc<str>` is every identifier the IR binds or mentions:
  `Val::Variable`, a variant's label, the pattern's names (the syntax's own
  `Pattern`), a `Rec` group's members,
  `CommandName::Bare`, and `Env`'s keys. A bind, a capture and a label clone a
  pointer.
- `Val::Variant { label, payload: Box<Val> }` always carries a payload;
  elaboration supplies `Unit` for a bare tag. Runtime `Value::variant`
  normalises unit payloads to its compact bare-tag representation.
- `CompKind::Prim(Prim)` computes on value operands only. `Prim::List`
  holds `ValListElem` entries or spreads; `Prim::Map` holds
  `MapEntryV::{Entry(Spanned<Val>, Spanned<Val>), Spread(Spanned<Val>)}`;
  `Prim::Update` holds one record base and named replacement fields.
  Operand computations hoist into surrounding binds in written order.
  Assembly may cost O(data) and fail, but runs no operand computation.
  Plain literals remain values.
- `Comp` — effectful, sequenced computation. `Comp` wraps a `CompKind` plus an
  optional `Span` for error reporting (synthetic nodes carry `span: None`).

**Computations partition into control, primitives and shell.** The core
constructors are the control forms `Return`, `Force`, `Lam`, `App`, `Bind`,
`Seq`, `Rec`, `If`, `Case`, `Try`, `Guard`, `Capture`, corresponding to
`Extrinsic/Syntax/Term` constructors, and `Prim`, each of whose variants is an
`Extrinsic.Prims.Op`. The acquiring `Guard` is owned Agda divergence D4
([[decisions/261111_a-guard-acquires|a-guard-acquires]]), pending the
strong-model correspondence follow-up. Residual-function Box also awaits an
Agda follow-up; Agda's Let-fun still suspends the whole RHS. Native terminals
are runtime mechanisms, not IR constructors or mechanised rules.
`Prim::operands` and `map_operands` are the common operand traversals.

- `Guard { acquire, body, release }` holds three `Val` operands.
  `Mentions` traverses all three; readback preserves them.
  Acquisition is shielded, then body receives the resource, then release
  receives resource and ending. The result type and grade are the body's.
- `Lam` binds one `Name`; destructuring parameters use `Bind` after the
  lambda chain. `Pattern::{Wildcard, Name, List, Record}` and `FieldPattern`
  are defined in `ir/pattern.rs`.
- `App { head, args: Box<[Spanned<Val>]>, hole }` applies a computation to
  values, with no spread. `Bind` takes a pattern and a `hole`; `Seq` discards
  a computation's result without generalisation. Holes are skipped by serde.
- `Exec`, `Pipeline`, `Redirect`, `Within`, `Grant`, `Spawn`, `Gather`,
  `Audit`, `Tilde`, `Protect` and `BindingCall` form the shell layer.
  `BindingCall` is the only skipped computation variant: a bare head bound in
  scope while the checker decides its splits. Readback makes it an `Exec`,
  and it cannot occur in a checked tree. Builtin application never becomes IR;
  partial builtins are runtime `Value::Native(entry, args)`. Boundary builtins
  remain η-expanded over `Exec`, retaining their per-use checked site.
- `Within.opts` and `Grant.caps` are `Arc<FieldsNode>`, the same nodes as
  record literals. `Within.handlers` stays separate syntax; `None` and an
  explicitly empty handler list install different scopes.

See [[invariants/ir-pure-cbpv|the IR invariant]] for the correspondence rule.

**The checker builds explicit verdicts into the IR.** Inference constructs
nodes beside their types; readback applies the solution's decisions,
snapshots sites and settles schemes only after the unit succeeds. Site graphs
may retain free weak variables and delayed obligations. Every evaluated
`Toplevel` has passed this check.

- `CompKind::Pipeline` is a struct variant `{ stages }`. The form's value is
  its final stage's. There is no per-stage type field, because every
  interior edge is an operating-system byte pipe
  allocated from stage position and no rule relates one stage's type to its
  neighbour's
  ([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]],
  [[map/core/typecheck|typecheck]]).
- `CompKind::If { cond, then, else_ }` and `CaseArm { tag, body }` carry their
  arms as `Spanned<Val>`: a thunk the form forces if it chooses it. A literal
  `{ … }` is that thunk, and so is a name holding one, so both are forced the
  same way; the elaborator refuses any other atom in arm position
  (`Elaborator::elab_arm`).
- `CompKind::Case { scrutinee, arms: Vec<CaseArm> }` is Levy's sum eliminator:
  a `CaseArm` is a tag and the thunk of a function of its payload to run, so
  the alternatives are a list fixed at parse time and every arm is a node the
  checker builds — an `if` with as many branches as the row has labels
  ([[decisions/260811_case-is-syntax-try-is-not|case-is-syntax-try-is-not]]).
- `Prim::Index { target, key }` carries one `Spanned<Key>`:
  `Label(Str)`, `Position(usize)` or `Data(Val)`. The key's form is explicit;
  no pass infers it from a `Val`. Readback and `Mentions` traverse only
  `Data`; the checker requires a record, list or map, respectively.
  An index chain is a sequence of these primitives joined by binds.
- `Exec { head, argv, redirects, site }` is an external or builtin call;
  `site` is the type the checker solved at a boundary call, which that door
  admits its value against, and `None` for every other head.
- `CompKind::Capture(Arc<Comp>)` is the kernel half of the checker's one
  coercion: run the body, which is `F^w Unit`, collect what it writes, and return
  those bytes exactly — total and lossless. `Prim::Decode(Val)` is the
  other half: read that `Bytes` value as text, one trailing terminator dropped
  and a strict UTF-8 decode, which is the partial step — the kernel's `decode`
  takes a value, so it reads a bound variable rather than nesting a `Comp`.
  [[map/core/typecheck|typecheck]]'s readback composes capture and decode
  (`captured_string`) as `Capture(body) to x. Prim::Decode(x)` around each
  command RHS a binding captures, and `Mentions`' walk descends into the `Capture`
  and the `Bind`. The reading is a node and not a command so that its meaning
  is fixed where the checker writes it
  ([[design/capture|grades and capture]],
  [[decisions/260811_a-coercion-is-syntax|a-coercion-is-syntax]],
  [[design/types|types]]). Neither has surface syntax.

- `Val::Protected { value, site }` carries a contract's delayed obligations.
  While inferring, an imported weak value occurrence or `Exec` head carries
  `Membrane::Imported(ty)`; readback keeps the membrane only if the snapshot
  reaches a thunk or handle (`Site::has_delayed`). Re-inferred protection uses
  `Membrane::Kept(site)` to preserve the checked site.
  Forming a membrane admits nothing; it does not run a thunk or consume a handle.
  `CompKind::Protect { body, site }` checks and wraps the body's returned value,
  so generated thunk wrappers can guard arguments and results at their actual
  elimination points. Neither form has surface syntax; both retain their
  contract when re-inferred ([[map/core/typecheck|typecheck]]).

- `CompKind::Rec { group: Arc<GroupNode>, index }` is the `index`-th member of
  a recursive group — `x⃗ : U C⃗ ⊢ Mᵢ : Cᵢ`, typed `Cᵢₙdₑₓ` — an n-ary
  generalisation of Levy's `rec x. M`, which is a group of one. `group`'s occ
  is the union of every member's mentions, computed once when the elaborator
  builds the `GroupNode`; every `Rec` projection of one group shares that one
  `Arc`. Inference checks a group once and memoises its member schemes and
  pending group by source `Arc` identity; readback memoises the checked group
  by pending `Arc` identity.
  Each rebuilt group's occ is computed from its own shape.

Every verdict
the evaluator needs from the checker is explicit: capture/decode,
returned-thunk splits, protection nodes, sites and schemes.

`CommandName` is the structured head for external dispatch (`Bare` / `Path` /
`TildePath`); `written()` gives it back as the source spelled it, `~`
unexpanded, for a diagnostic raised before there is a `HOME` to expand it
against.

The IR owns the vocabulary it shares with the AST (`ir/pattern.rs`,
`ir/op.rs`, `ir/redirect.rs`): `Pattern` (with `names()` the one walk, and
`Display`), `BinaryOp` (`Arith`/`Compare`/`Eq`, each category its own enum), and
`Redirects` with its stream-once binding. A pattern binds names, never carries
a computation, so there is no parser syntax for elaboration to strip out
([[invariants/ir-pure-cbpv|ir-pure-cbpv]]). `Phrase<D = DefineSchemes, P = Checked>`
is indexed first by schemes, then phase: elaboration yields `Unchecked`
(`Phrase<()>`, checked-phase terms without schemes); inference returns
`Phrase<LiveSchemes, Inferring>` carrying unsettled schemes. `DefineSchemes`
holds `Closed` schemes. Only successful readback makes a `Toplevel`, so the
evaluator cannot be handed unchecked IR. `Exec::argv` is `Argv`, a newtype over list slots admitting
spreads (`positional()`, `arity()`). Value application instead has a fixed
slice of arguments.

`Mentions` (`pub(crate)`) is the trait a `Comp`, a `Val`, and each of the four
node shapes implement: `fn mentions(&self, out: &mut Vec<&Name>)`, one
exhaustive, wildcard-free arm per variant. `Node::new` uses `Occ::of` over the
shape it is building to compute that node's own `Occ`; a nested node's
`mentions` contributes its already-computed occ rather than being walked
again, so occ for a whole program is linear in program text. `Toplevel::
referenced_names` (`pub(crate)`) is the same walk, run over a phrase's `Comp`
top to bottom: the use-observation signal the
[[map/core/shell-state|binding-lease ledger]] renews on
([[decisions/260629_agent-binding-reaping|agent-binding-reaping]]). What a
closure keeps is no longer a walk of its own: `Closure::new` reads the occ
already sitting on the node that owns its body
([[decisions/260926_a-closure-keeps-only-what-it-mentions|a-closure-keeps-only-what-it-mentions]]).

This shape is what the prelude bake serialises with `postcard`; adding a field to
`CompKind`, `Val`, or `Pattern` invalidates every emitted blob (see
[[map/core|core]] and `core/src/shell/boot.rs`). `docs/SPEC.md` gives the formal CBPV account.
