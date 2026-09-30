---
generated_at_commit: 8d868e18
generated_at_date: 2026-09-30
covers_paths: [core/src/ir.rs]
---

# Map: core / IR

`core/src/ir.rs` is the [[design/cbpv|call-by-push-value]] intermediate
representation — the target of [[map/core/elaboration|elaboration]] and the input
to the [[map/core/evaluator|evaluator]].

A whole program is a `Toplevel { phrases: Vec<Spanned<Phrase>> }`: each
`Phrase` — `Define` (a top-level `let`, one closed `Scheme` per name the
pattern binds,
[[decisions/260603_session-scheme-continuity|session-scheme-continuity]]) or
`Run` (anything else) — runs in order, extending the
session environment the next phrase sees. `Toplevel::referenced_names` is
the phrase-level analogue of the `Comp`-level walk below, for the same lease
ledger.

The two categories:

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
  `shape` through `Mentions`, sorts and dedups the names into `Occ`, and hands
  back the `Arc<Self>` every holder shares; `shape()` lends the shape back,
  `occ()` (`pub(crate)`) the occ. `ThunkNode = Node<Arc<Comp>>`, `GroupNode =
  Node<Box<[(Name, Arc<Comp>)]>>`, `ListNode = Node<Box<[Spanned<Val>]>>`,
  `FieldsNode = Node<Box<[(Name, Spanned<Val>)]>>`. `PartialEq` compares
  shapes; serde carries the shape alone (`Node::from`/`Into` the shape), so
  decoding a node off the wire recomputes its occ rather than trusting one
  that rode along — `Val`'s own tests (T4) check a node crosses as its shape.
- `Occ(Arc<[Name]>)` is a node's mentioned names, sorted and distinct by
  construction — built only inside `Node::new`. `len` and `contains` (binary
  search) are its whole `pub(crate)` surface; a lookup by name never scans.
- `Name = Arc<str>` is every identifier the IR binds or mentions:
  `Val::Variable`, a variant's label, the pattern's names (the syntax's own
  `Pattern`, since `IrPattern = Pattern`), a `Rec` group's members,
  `CommandName::Bare`, and `Env`'s keys. A bind, a capture and a label clone a
  pointer.
- `CompKind::Assemble(Assembly)` is the one rule that builds a collection some
  of whose parts are spread or keyed at run time: a primitive computation, not
  a value, since it costs O(data) and can fail. `Assembly::List(Vec<ValListElem>)`,
  `::Record(Vec<ValRecordEntry>)`, and `::Map(Vec<ValMapEntry>)` are the spread-
  and computed-key-bearing element enums — `ValRecordEntry::Field(String, …)`
  against `ValMapEntry::Entry(Val, …)`, so a record cannot hold a key computed
  at run time and the [[design/records-and-maps|record/map]] classification is
  settled by the parser rather than re-derived. These three enums live only in
  `Assembly` and in `Args` (positional call arguments, which always admit a
  spread); both read as `MapPart`s where the one runtime carrier is built. An
  entry carries its value's span, because the surface captures no key span.
  The elaborator's literal arms (`elaborator.rs`) emit `Return(Val::…)` for a
  plain literal and `Assemble(…)` otherwise; `hoist` names a non-`Return` comp
  reached in value position, so `[1, ...$xs]` becomes `Assemble(…) to t. …
  t …` with no new plumbing.
- `Comp` — effectful, sequenced computation. `Comp` wraps a `CompKind` plus an
  optional `Span` for error reporting (synthetic nodes carry `span: None`).

The checker's verdict rides on the IR too, as **ground** annotations written by
`annotate`. Because the inference pass is unconditional — every evaluated IR is
annotated ([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]])
— the slots are not optional: "the checker has not run yet" is not a
representable state.

- `CompKind::Pipeline` is a struct variant `{ stages, stage_types: Vec<Ty> }`.
  `stage_types` holds one value type per stage, parallel to `stages`, as typing
  metadata for the structural REPL rather than a transport channel; the
  elaborator fills it with `Unit` placeholders the annotation pass overwrites.
  The form's value is its final stage's. There is nothing per-stage to
  annotate, because every interior edge is an operating-system byte pipe
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
  checker can annotate — an `if` with as many branches as the row has labels
  ([[decisions/260811_case-is-syntax-try-is-not|case-is-syntax-try-is-not]]).
- `Exec { head, args, redirects, site }` is an external or builtin call;
  `site` is the type the checker solved at a boundary call, which that door
  admits its value against, and `None` for every other head.
- `CompKind::Capture(Arc<Comp>)` is the kernel half of the checker's one
  coercion: run the body, which is `F Unit`, collect what it writes, and return
  those bytes exactly — total and lossless. `CompKind::Decode(Val)` is the
  other half: read that `Bytes` value as text, one trailing terminator dropped
  and a strict UTF-8 decode, which is the partial step — the kernel's `decode`
  takes a value, so it reads a bound variable rather than nesting a `Comp`.
  Neither has surface syntax; [[map/core/typecheck|typecheck]]'s `annotate`
  composes them as `Capture(body) to x. Decode(x)` around each command
  `capture_sites` recorded, and `Mentions`' walk descends into the `Capture`
  and the `Bind`. The reading is a node and not a command so that its meaning
  is fixed where the checker writes it
  ([[decisions/260930_capture-is-decided-by-syntax|capture-is-decided-by-syntax]],
  [[decisions/260811_a-coercion-is-syntax|a-coercion-is-syntax]],
  [[design/types|types]]).

- `CompKind::Rec { group: Arc<GroupNode>, index }` is the `index`-th member of
  a recursive group — `x⃗ : U C⃗ ⊢ Mᵢ : Cᵢ`, typed `Cᵢₙdₑₓ` — an n-ary
  generalisation of Levy's `rec x. M`, which is a group of one. `group`'s occ
  is the union of every member's mentions, computed once when the elaborator
  builds the `GroupNode`; every `Rec` projection of one group shares that one
  `Arc`, and `annotate` (`typecheck/annotate.rs`) preserves the sharing by
  memoizing its rebuild per source `Arc`'s identity.

Every verdict
the evaluator needs from the checker is explicit syntax: a `Capture`/`Decode` pair, a site, a scheme.

`CommandName` is the structured head for external dispatch (`Bare` / `Path` /
`TildePath`); `written()` gives it back as the source spelled it, `~`
unexpanded, for a diagnostic raised before there is a `HOME` to expand it
against.

`IrPattern = Pattern` — the same `Pattern` shape the AST uses, under the IR's
own name: a pattern binds names, never carries a computation, so there is no
parser syntax for elaboration to strip out
([[invariants/ir-pure-cbpv|ir-pure-cbpv]]).

`Mentions` (`pub(crate)`) is the trait a `Comp`, a `Val`, and each of the four
node shapes implement: `fn mentions(&self, out: &mut Vec<&Name>)`, one
exhaustive, wildcard-free arm per variant. `Node::new` runs it once, over the
shape it is building, to compute that node's own `Occ`; a nested node's
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
[[map/core|core]] and `core/src/boot.rs`). `docs/SPEC.md` gives the formal CBPV account.
