---
generated_at_commit: f9ac94f8
generated_at_date: 2026-11-12
covers_paths: [core/src/ir.rs, core/src/ir/]
---

# Map: core / IR

`core/src/ir.rs` is the [[design/cbpv|call-by-push-value]] intermediate
representation — the target of [[map/core/elaboration|elaboration]] and the input
to the [[map/core/evaluator|evaluator]].

A runtime program is a `Toplevel { phrases, admits }`: `admits` holds imported
bindings' first-use contracts for readmission before execution; each
`Phrase` — `Define` (a top-level `let`, one `Closed` scheme per name the
pattern binds,
[[internals/type-inference|type-inference]]) or
`Run` (anything else) — runs in order, extending the
session environment the next phrase sees. `Toplevel::referenced_names` is
the phrase-level analogue of the `Comp`-level walk below, for the same lease
ledger.

**One IR grammar is indexed independently by phase and representation.**
`Val`, `CompKind`, `Prim`, `Exec`, their operand types and shared-node aliases
take `P: Phase = Explicit` and `D: IrData = RuntimeData`. `Phase` has
one associated type per annotated position: `Bind` (a `CompKind::Bind`'s or
`Phrase::Define`'s right-hand side), `Call` (a `CompKind::App`),
`BindingCall` (a `CompKind::BindingCall`) and `Boundary` (`Exec.site`, an
`Option<P::Boundary>`). `Explicit` has `Bind = Call = ()`, an uninhabited
`BindingCall` and `Boundary = Arc<Site>`: it holds no unknowns, so
elaborator output, checked IR and runtime IR share it, and only checked IR
carries sites.
The checker's own `Inferring` phase holds holes and types there
([[map/core/typecheck|typecheck]]); readback alone turns it into `Explicit`.
`Serial` marks the phases whose trees serialise: the skipped annotations
default, sites cross. `Toplevel<D>` always uses `Explicit` phrases, regardless
of the containing tree's phase. `Prim::map_operands` converts `Prim<P, D>`
to `Prim<Q, D>` infallibly, preserving operand spans and the representation
axis; inference's individual sub-rules retain their own operand order.

`IrData` selects only the byte and scheme leaves: `RuntimeData` uses `Bytes`
and `Closed`; marshal-private `RawData` uses `String` and `Arc<Scheme>`.
`Val<P, D>::Bytes` holds `D::Bytes`; `DefineSchemes<D::Scheme>` holds the
representation's definition schemes. Serde bounds terminate at these
associated leaves, retaining explicit `P: Serial` and `Phrase`'s separate
scheme bounds, without requiring serde on representation markers or recursive
whole-tree bounds. Marshalling writes runtime IR directly and structurally
decodes the same grammar as raw IR. `marshal/read/ir.rs` admits it before
`ValClos::closure`, checking base64 and closing every nested linked-module
scheme while preserving metadata and JSON paths
([[map/core/transport|transport]]).

Values, computations and shared nodes:

- `Val` — inert data: `Unit`, `String`, `Int`, `Float`, `Bool`, lists, records,
  maps, variants, thunks, variables, and runtime constants `Bytes`, `Builtin`
  and `Handle`. The elaborator writes none of those three; `Builtin` and
  `Handle` are serde-skipped and occur only at a value closure's root,
  crossing as `marshal::wire::Value::{Builtin, Handle}`.
  Forming a value executes no computation. Collection and thunk syntax share
  their `Arc` nodes: `Val::Thunk(Arc<ThunkNode>)`,
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
  elaboration supplies `Unit` for a bare tag. Runtime `ValClos::variant`
  shares its payload's syntax and capture; `View::Variant` omits a resolved
  unit payload, including a variable bound to unit.
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
Agda follow-up; Agda's Let-fun still suspends the whole RHS. Runtime builtin
η/δ requires its own correspondence follow-up; it is not a mechanised Rust
refinement.
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
  `Audit`, `Tilde`, `Use` and `BindingCall` form the shell layer.
  `BindingCall` and runtime `Builtin` are serde-skipped in both directions.
  `BindingCall` is a bare head bound in scope while the checker decides its
  splits. Readback makes it an `Exec`, and it cannot occur in a checked tree.
  Runtime `CompKind::Builtin { builtin, args }` is δ: a cached builtin η-expansion's λs bind `args`,
  and its environment binds `builtin` to the entry itself. `Mentions` includes
  all these names; readback copies δ through. The elaborator writes none, and
  `ValClos::block` keeps η residuals from the checker.
  Partial builtins are boxed λ residuals; marshalling writes their builtin
  name, environment row and supplied count, then rebuilds the receiver's η
  suffix through `BuiltinEntry::residual`. δ itself never crosses.
  Boundary partials remain expanded over `Exec`, retaining their per-use
  checked site
  ([[decisions/261112_runtime-values-are-value-closures|runtime-values-are-value-closures]]).
- `Use(Arc<Module<D>>)` holds `Toplevel<D>` and its `SourceOrigin`; module
  phrases remain `Explicit` even inside an `Inferring` tree.
  Elaboration links the one written path at check time; no path lookup remains
  for evaluation. The module is checked and run under Σ alone, and its public
  schemes contain no weak variables. Raw module schemes are admitted through
  `Closed::new` recursively on child read. `Use` is total in every IR phase;
  `Toplevel::exported_schemes` supplies the importer's record type and the
  evaluator's public-name projection. Marshalling carries the linked tree
  and origin, so a closure containing `use` reads no file when resumed.
- `Within { opts, body }` and `Grant { caps, body }` carry their options as
  one spread-free `Arc<FieldsNode>`, the same node as a record literal.
  `handlers:` is an ordinary map-valued field; no handler-specific AST or IR
  survives. Readback traverses that field through the option record. An absent
  field and `handlers: [:]` still install different scopes.

See [[invariants/ir-pure-cbpv|the IR invariant]] for the correspondence rule.

**The checker builds explicit verdicts into the IR.** Inference constructs
nodes beside their types; readback applies the solution's decisions,
snapshots sites and settles schemes only after the unit succeeds. Site graphs
may retain free weak variables; they need not be ground. A stored name whose
scheme reaches a weak residual beneath a thunk or handle is statically
unusable in a later unit. Accepted residual occurrences are plain variables;
`Toplevel.admits` checks their existing data before execution, not future
arguments, results or handle payloads. Every evaluated `Toplevel` has passed
this check ([[invariants/schemes-leave-closed|scheme invariants]]).

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
returned-thunk splits, sites and schemes. The runtime builds no contract IR.

`CommandName` is the structured head for external dispatch (`Bare` / `Path` /
`TildePath`); `written()` gives it back as the source spelled it, `~`
unexpanded, for a diagnostic raised before there is a `HOME` to expand it
against.

The IR owns the vocabulary it shares with the AST (`ir/pattern.rs`,
`ir/op.rs`, `ir/redirect.rs`): `Pattern` (with `names()` the one walk, and
`Display`), `BinaryOp` (`Arith`/`Compare`/`Eq`, each category its own enum), and
`Redirects` with its stream-once binding. A pattern binds names, never carries
a computation, so there is no parser syntax for elaboration to strip out
([[invariants/ir-pure-cbpv|ir-pure-cbpv]]).
`Phrase<S = DefineSchemes, P = Explicit, D = RuntimeData>` keeps its scheme
parameter independent of phase and representation: elaboration yields `Unchecked`
(`Phrase<()>`, explicit-phase terms without schemes); inference returns
`Phrase<LiveSchemes, Inferring>` carrying unsettled schemes. `DefineSchemes`
defaults to `Closed` schemes. Successful readback makes runtime `Toplevel`;
child reads reconstruct it only after raw IR admission, so the evaluator
cannot be handed unchecked IR. `Exec::argv` is `Argv`, a newtype over list
slots admitting spreads (`positional()`, `arity()`). Value application instead has a fixed
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
value captures uses the same occurrence sets: `ValClos::close` and
`ValClos::thunk` restrict at formation; `close` additionally pulls Σ-only
direct names into ρ.
Component reads share that ρ without another restriction
([[decisions/260926_a-closure-keeps-only-what-it-mentions|a-closure-keeps-only-what-it-mentions]]).

This shape is what the prelude bake serialises with `postcard`; adding a field to
`CompKind`, `Val`, or `Pattern` invalidates every emitted blob (see
[[map/core|core]] and `core/src/shell/boot.rs`). `docs/SPEC.md` gives the formal CBPV account.
