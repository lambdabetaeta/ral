---
status: proposed
---

# The REPL is a projection of live program state, not a transcript

**The interactive shell renders three dimensions a ral session already
computes — the *type* of each pipeline stage, the *dependency graph* between
bindings, and the *state* of every running computation — as a navigable
surface around a normal prompt, so the runtime's own knowledge becomes
legible and steerable instead of surfacing only on failure.** The prompt line
stays where shells have always put it; the surface is a reactive projection of
the live `Shell`, re-derived per keystroke and per turn. The three projections
share one graphic vocabulary with the exarch TUI
([[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]]): a
marginal rail, a value-ramp, collapsible blocks, and a matrix, so ral and
exarch read as one design system.

This is a design proposal, not a landed change. It specifies a *new
`Frontend`* — a third backend alongside `RustylineFrontend` and
`MinimalFrontend` (`repl/frontend.rs`) — that renders the projections while
implementing the same `Frontend::read` contract the session loop already
drives. Most of the data already exists in the runtime; the work is a
frontend that projects it, plus a few small substrate extensions named below.

## The diagnosis

The present REPL ([[map/repl|repl]]) is a *line editor over a transcript*.
`Session::turn` (`repl/session.rs`) renders a prompt, calls
`frontend.read`, and on a `Read::Line` hands the input to `step` →
`execute_input` → `shell.run_turn`. Three pieces of information the runtime
computes on the way are **discarded unless they constitute an error**:

- **Per-stage pipeline types.** `compile_and_typecheck` (`ral_core::lib`)
  typechecks the whole line every turn ([[map/core/typecheck|typecheck]]); a
  pipeline `ls | map file-info | filter …` is checked as one comp, and the
  *per-stage* types — the dataflow the user is debugging — are never shown.
  The user learns a stage is ill-typed only after Enter, from a post-hoc
  diagnostic.
- **The binding dependency graph.** `Ast::free_refs` (`syntax/free_refs`)
  returns the free-variable set of an AST — the edges between `let` bindings
  — and `syntax::group` already uses it to build `LetRec` groups. The REPL
  treats the session as append-only: a `let` mutates the environment and the
  line is history. The graph of which bindings depend on which is computed
  and thrown away.
- **The state of running work.** `HandleInner` / `HandleState`
  (`types/value.rs`) track every spawned computation; `SurfaceBuffer` holds
  the deferred surface events replayed on `await` / `race`
  ([[map/core/io-process|io-process]]); `JobTable` (`repl/jobs.rs`) holds
  pgid-parked groups ([[map/repl/jobs|jobs]]). The REPL exposes these only
  through the `jobs` / `fg` / `bg` commands — a table the user queries, not a
  surface the user watches.

The shared defect: **the runtime knows, and the REPL does not show.** Each
dimension is an enforcement layer that surfaces on failure and is invisible on
success.

## The frontend architecture

A new `StructuralFrontend` implements the `Frontend` trait. Where
`RustylineFrontend` is a line editor that hands a single row to the terminal,
the structural frontend is a **full-screen ratatui application** (the same
crate exarch uses) that owns the whole frame: raw mode, the alternate screen,
and a layout that places the prompt and the three projections on one canvas.

```
┌─────────────────────────────────────────────────────────────┐
│  (scrollback / transcript of past turns)                     │
│   => [src, tests, …]                                         │
│                                                              │
│  ┌─ Typed Spine (left rail, the line being composed) ─────┐  │
│  │  ls                  : list<path>                      │  │
│  │  ┃                                                   │  │
│  │  map file-info       : list<path> → list<map>         │  │
│  │  ┃                                                   │  │
│  │  filter { |m| … }    : list<map> → list<map>      ◂ flare│
│  └──────────────────────────────────────────────────────┘  │
│  ❯ sort-list-by { |m| $m[size] }                            │  ← prompt
│  ────────────────────────────────────────────────────────   │
│  Reactive Worksheet (binding DAG)        Handles Matrix     │
│   ● dirs   : list<path> = […]            ● h0 settled 42ln  │
│   ├─● sizes : list<map>  = […]           ● h1 pending …     │
│   └─● total : int        = 184           ○ h2 stopped       │
└─────────────────────────────────────────────────────────────┘
```

- **`Frontend::read` runs the loop.** The session calls `read(&mut Shell,
  &prompt, pending)`; the frontend renders the frame, polls keys, and returns
  `Read::Line` when the user submits the prompt. The projections are rendered
  *inside* `read`, so they update live while the user composes.
- **The live `Shell` is the data source.** `read` receives `&mut Shell`; the
  frontend snapshots `Env::binding_schemes()` (the `SessionSchemes` seed) and
  `Env::all_bindings()` from it for the Worksheet, and reads handle values
  for the Matrix. The Typed Spine re-runs `compile_and_typecheck` on the
  partial prompt buffer against those schemes.
- **Degradation is structural.** If the terminal cannot do raw mode or the
  alternate screen, or `RAL_INTERACTIVE_MODE` selects it, the session falls
  back to `MinimalFrontend` (the existing canonical-stdin path). The
  projections are additive chrome; the `Frontend` trait and both existing
  backends are unchanged. A `RAL_SURFACE=minimal|rustyline|structural` switch
  (mirroring the existing mode probe) selects the backend at boot.

## 1. The Typed Spine — per-stage types as you compose

The pipeline under the cursor renders as a vertical spine of per-stage types,
the type flowing between stages shown at each join. A stage whose output type
will not feed the next flares **before** Enter is pressed; the user fixes the
pipeline in place rather than reading a post-hoc error.

### Where the data comes from

The frontend calls `ral_core::compile_and_typecheck(partial_buffer,
session_schemes)` on the prompt text as the user types. On success the result
is a typed `Comp`; a pipeline elaborates to `CompKind::Pipeline { stages,
wires }` (`core/src/ir.rs`), where `stages` is one `Arc<Comp>` per stage. The
spine walks `stages` and renders each stage's value type.

**Substrate extension required.** The annotation pass
(`typecheck::annotate`) writes generalised schemes onto top-level `Bind`
nodes, but **not** onto individual pipeline stages — the per-stage value
types are inferred internally and discarded. The spine needs them. Two
options, in order of cleanliness:

- *Preferred:* extend the annotation pass to write each pipeline stage's
  inferred value type onto the `Pipeline` node (a `Vec` of value types
  parallel to `stages`/`wires`). This is one more field harvested alongside
  the existing `wires`, at the same pass.
- *Fallback:* the frontend typechecks each stage prefix in isolation, feeding
  the previous stage's output type as the input. More inference runs, but no
  core change.

Either way, no new *inference* — the checker already infers per-stage types
to accept or reject the pipeline; the work is to *retain* them.

### Type holes

A `?` typed as a placeholder is a first-class hole. The spine shows the
constraint it must satisfy (`? : map → bool`) — the typed analogue of a
Haskell hole. This needs the elaborator/checker to treat `?` as a fresh type
variable that unifies but is not generalised, and to report its inferred
constraint. A small extension; flag it as a follow-on, not a blocker for the
basic spine.

### Interaction and render

- **Debounce.** Re-inference runs on an idle timer (~80ms after the last
  keystroke), not every key. The partial buffer is small (one line), so
  `compile_and_typecheck` is cheap, but a parse-partial line produces a parse
  error; the spine suppresses render on parse failure and shows the last good
  spine dimmed.
- **Flare.** A stage whose output type does not unify with the next stage's
  input renders in the rail's error hue; the join between them carries a
  short mismatch note (`list<path> ≠ map`).
- **Layout.** One rail row per stage, left of the prompt, the cursor's stage
  highlighted. The rail is two columns wide (shape + type), matching
  exarch's marginal rail.

### Why it is load-bearing

The pipeline is ral's primary idiom ([[design/pipelines|pipelines]]); pipeline
debugging is the dominant shell activity. No shell shows inter-stage dataflow
inline as the user types. This is the keystone projection: most localised,
makes the type system legible on its own, and establishes the margin render
loop the other two plug into.

## 2. The Reactive Worksheet — bindings as a live dependency graph

The session's `let` bindings are laid out as a typed DAG; each node shows
`name : type = value-preview`. Editing one binding re-evaluates every
transitive dependent, its type and value updating in place; edges light as
they re-flow. The worksheet can be **forked** — two live programs on one
screen — which is persistence doing real work, not transcript navigation.

### Where the data comes from

- **Nodes:** `Env::all_bindings()` gives every live name→value pair;
  `Env::binding_schemes()` gives each name's `Option<Scheme>`
  (`types/env.rs`). Together they are the node set with types.
- **Edges:** `Ast::free_refs` extracts, for a binding's RHS, the set of names
  it references freely — the dependency edges. `syntax::group` already uses
  this to form `LetRec` groups, so the analysis is proven and in place.

**Substrate extension required.** After evaluation the `Env` stores `Value`s,
not ASTs — the free-ref edges are computed at compile time and discarded. The
worksheet needs the edges reconstructable. The cleanest fix: **retain the
free-ref set per top-level binding alongside the value and scheme.** Either
add a field to `Binding` (`types/env.rs`), or keep a parallel worksheet model
in the REPL that records `(name, free_refs, scheme, source_text)` at each
successful top-level `Bind`. The parallel model is lower-risk (no core type
change) and is recommended for the first cut.

### Re-flow

When the user edits a binding's definition (selecting its node loads its
source into the prompt area; submitting rebinds it), the worksheet:

1. Recompiles and re-evaluates the edited binding's RHS through `run_turn`
   (or a lighter eval path that installs into a snapshot env).
2. Walks the DAG forward along `free_refs` edges to find transitive
   dependents.
3. Re-evaluates each dependent in topological order, updating its value and
   type preview in place.

`Shell: Clone` + the persistent `imbl`/`Arc<Env>` backing
([[decisions/260617_turn-local-state|turn-local-state]]) make each
re-evaluation snapshot-isolated: a re-flow cannot corrupt a sibling's state,
and a fork is a refcount bump, not a copy.

### Side-effect safety

A binding whose RHS is a command, a `spawn`, or any effectful computation
must not re-run silently when an upstream binding changes. **Such bindings
are *manual* by default** — they display their last value but do not re-flow
on upstream change; the user explicitly forces them.

The classification of "effectful" reuses the mode system, not a new
heuristic: a binding whose RHS compiles to a `CompKind::Exec`, `Scope`, or
carries a non-`Empty` output mode (`rhs_output` on the `Bind` node) is
effectful. Pure bindings (`map`, `filter`, arithmetic, list ops) re-flow
freely. This is the same verdict the typechecker already records
([[design/pipelines|pipelines]], [[map/core/typecheck|typecheck]]).

### Fork

A forked worksheet is a second `Shell` snapshot (cheap, persistent). Edits in
one branch do not mutate the other; both render simultaneously (split panel
or tab). This is the un-Warp move: not transcript navigation, but state
branching — only possible because ral's environment is a persistent data
structure. bash's env is a mutable string map overwritten in place; a block
shell built over bash could not fork state at any cost.

### Interaction and render

- **Navigate:** expand/collapse nodes (collapsible blocks, exarch-style);
  the DAG renders as an indented tree with dependents nested under their
  parents.
- **Edit:** selecting a node loads its source into the prompt; submitting
  rebinds and triggers re-flow.
- **Pin:** a node can be pinned to manual to freeze it while experimenting
  upstream.
- **Fork:** a key gesture splits the worksheet into two independent
  environments.
- **Preview:** each node shows a truncated value preview
  (`fmt::Display for Value`); large values collapse to a summary with a
  size bar (exarch's size-as-quantitative-variable idiom).

### Why it is load-bearing

It is the feature that makes the REPL feel like a different species of tool,
and it is only possible on a typed (free-ref analysis is meaningful),
functional (bindings are values, not assignments), persistent (fork and
re-flow are cheap) substrate — exactly ral's profile. It is Observable-meets-
shell: there is no transcript, no "previous command" to scroll to, no blocks.

## 3. The Handles Matrix — concurrency as a dashboard

Every live spawn and pgid job renders as a matrix row (the exarch agent×step
matrix transposed), with state glyphs, streaming captured stdout/stderr, and
rail actions. Pointing at a handle value in the scrollback and acting on it
— `await`, `race`, `cancel`, `fg` / `bg` — turns first-class concurrency
into a steerable surface.

### Where the data comes from

- **Handles:** `Value::Handle(HandleInner)` (`types/value.rs`) is the
  first-class handle returned by `spawn` / `par`. `HandleState`
  (Running / Settled / Cancelled) gives the row state; `CompletedHandle`
  holds the cached result and captured bytes for a settled handle;
  `SurfaceBuffer` holds the deferred surface events that stream on
  `await` / `race` ([[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-primitives-detached-vs-structured]]).
- **Pgid jobs:** `JobTable` / `Job` / `JobState` (`repl/jobs.rs`) track
  process groups parked by the kernel (Ctrl-Z); `wait_foreground` and the
  `fg` / `bg` / `disown` captured builtins drive them.

**Seam to close.** `Frontend::read` receives `&mut Shell` but not the
`JobTable`, which `Session` holds separately (`self.jobs`). To render pgid
jobs, the job table must reach the frontend. Two options:

- Thread `&Arc<Mutex<JobTable>>` into `read` (extend the `Frontend` trait
  signature), mirroring how `execute_input` already receives it. Clean but
  touches the trait and both existing backends.
- Have the matrix read only handles from the env + a handle registry on the
  shell, and treat pgid jobs as a separate `jobs`-command overlay. Lower
  blast radius, but splits the surface.

The first is recommended — the trait change is mechanical and both backends
already have access to the table through the session.

**Handle enumeration.** Handles live as values in the env (from `spawn`)
*and* as detached workers in the runtime. The matrix needs to list all live
handles. A handle registry on `Shell` (or walking `all_bindings()` for
`Value::Handle`) supplies the value-side; detached workers need a runtime
enumeration hook. Flag the latter as a substrate extension if detached-worker
visibility is required in the first cut.

### Streaming

A pending handle's captured stdout/stderr streams into its row live. The
`SurfaceBuffer` replay path already exists (`await` / `race` replay deferred
events through the awaiting turn's surface); the matrix reads the buffer for
display without consuming it. Settled rows carry their `CompletedHandle`
cached value; cancelled rows dim out.

### Interaction and render

- **Rows:** one per handle/job — state glyph (`●` running, `✓` settled, `✗`
  failed, `○` stopped), label, line count / duration, action hints.
- **Point-at-value:** a handle value rendered in the scrollback is
  selectable; actions (`await`, `cancel`) target it.
- **`race`:** drag two rows together to race them (the first to settle wins,
  the other is cancelled) — the `race` builtin as a gesture.
- **`fg` / `bg`:** for pgid jobs, foreground/background via the job table.
- **Layout:** a strip below the worksheet, collapsing to nothing when no
  work is live (the common case).

### Why it is load-bearing

First-class concurrency is ral's most distinctive runtime feature and the one
the REPL hides most completely. The matrix replaces `jobs` / `fg` / `bg` as
the *primary* job-control surface rather than a query the user remembers to
run. The `jobs` / `fg` / `bg` commands remain available as keyboard shortcuts
into the matrix.

## The shared visual system

All three projections borrow exarch's component vocabulary
([[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]]) so
ral and exarch read as one design system:

- the **marginal rail** (Typed Spine, and the worksheet's per-node rail
  glyph);
- the **value-ramp** (worksheet size bars, matrix magnitude);
- **collapsible blocks** (worksheet nodes, matrix rows);
- the **matrix** (handles);
- **OSC-52 yank** (copy a value preview or subtree to the clipboard).

The margin is the persistent canvas; the prompt line stays where shells have
always put it. Colour is suppressed automatically when
`ral_core::ansi::use_ui_color` returns false, so the projections store
unconditional colour and degrade on dumb terminals
([[map/repl/frontend|frontend]]).

## Pointer interaction

The mouse's job is to **point at the structural objects the three projections
render**, not to chase references through text. The REPL rendered those
objects, so it knows what each one is — a value, a binding node, a handle row
— and pointing at a known object is faster than keyboard-navigating a graph.
Three interactions, no more:

- **Click a rendered value → insert its reference at the prompt.** A handle
  inserts `$h0`; a binding inserts `$name`; a click-drag over a slice of a
  list inserts `$dirs[0:3]`; a map entry inserts `$m[size]`. The frontend
  keeps a hit-map from rendered cell to the `Value` it depicts (exarch's
  element-identification machinery already does this), so the reference is
  exact — the value's `Value` variant is known, not guessed from text.
- **Click a worksheet node → load its source into the prompt** to edit and
  rebind it, triggering re-flow. Graph navigation by pointing, which is what
  graphs want; arrow-key traversal of a DAG is the clunky alternative.
- **Click a matrix row → focus it; drag two rows together → `race`.** Spatial
  actions on a spatial layout. Row verbs (`await` / `cancel` / `fg` / `bg`)
  remain on the keyboard.

The Typed Spine needs no pointer — it is read-only and the user is typing,
not pointing.

### What was rejected: the content-addressed plumber

An earlier draft proposed an acme-style plumber: a right-click (or
⌘-click) that *chases* the value under the cursor by dispatching on its type
— a path opens in `$EDITOR`, a handle jumps to its matrix row, a binding
jumps to its DAG node — with a programmable rule registry. It is rejected.
acme's plumber chases references because acme is an *editor* and code is
dense with cross-references; a shell's primary activity is running commands
and reading output, not chasing references, so the plumber's value is a
secondary activity imported from the wrong substrate. Worse, it leaks
exactly where the shell lives: command output (`ls`, `rg`, `find`) is
untyped bytes with no `Value` behind it, so type-driven dispatch collapses
to heuristic token-matching there — the acme behaviour the typed substrate
was meant to improve on. The three-verb discipline (select / act / chase),
the modifier-key middle-button substitute, and the typed snarf buffer were
all scaffolding for the plumber and go with it.

The surviving design is smaller and honest: the pointer acts only on values
the REPL *rendered*, whose type it therefore knows. There is no "is this a
file?" question, because the pointer never divines meaning from text — it
points at known objects. Cross-turn copy falls to the system clipboard; no
typed buffer is built. Plain click is the only button the trackpad needs;
the structural gestures (drag-slice, drag-to-race) are native trackpad
moves, and no middle button or modifier substitute is required.

## The substrate, not the wrapper

Every projection is a **projection of runtime state that already exists** —
types from the checker, edges from `free_refs`, handles from the concurrency
runtime. The REPL stops throwing them away. This is the opposite of the
block-shell approach, which reconstructs blocks and IDE affordances from byte
streams over a substrate (bash) that cannot support any of them structurally.
ral's whole bet is that the interesting shell UX lives at the *language*
layer, not the *terminal* layer; the three projections are that bet made
visible.

## Substrate extensions required

These are the changes beyond the new frontend. Each is small and local:

1. **Per-stage pipeline types** (Typed Spine). Extend `typecheck::annotate`
   to write each pipeline stage's inferred value type onto the `Pipeline`
   node, parallel to the existing `wires`. One field, one pass.
2. **Binding dependency edges** (Worksheet). Retain the `free_refs` set per
   top-level binding — either a field on `Binding` (`types/env.rs`) or a
   parallel REPL-side worksheet model recording `(name, free_refs, scheme,
   source)`. Recommended: the parallel model first, to avoid a core type
   change.
3. **Job-table access in the frontend** (Matrix). Thread
   `&Arc<Mutex<JobTable>>` into `Frontend::read`, or otherwise expose pgid
   jobs to the frontend. Mechanical trait change.
4. **Detached-worker enumeration** (Matrix, optional first cut). A runtime
   hook to list live detached handles beyond those held in env values.

## What changes, what stays

- **New:** `StructuralFrontend` — a ratatui `Frontend` impl rendering the
  three projections; a `RAL_SURFACE` selector at boot; the worksheet model
  (nodes + edges + pin/fork state); the matrix's handle/job dashboard.
- **Changed:** the session loop feeds the frontend the same compile/scheme
  results it already computes; `Frontend::read` may gain a job-table
  argument; the annotation pass retains per-stage types; the prompt render
  cedes its left margin to the rail.
- **Retained:** the `Frontend` trait and both existing backends; the per-turn
  `step` / `execute_input` / `run_turn` pipeline; the concurrency runtime
  and its handle/job tables; the plugin keybinding and lifecycle-hook
  surface; batch execution.
- **Unchanged:** the typechecker and evaluator internals; the `Shell` state
  model; the capability enforcement gates. The projections read; they do not
  mutate the runtime (except via the user's explicit worksheet re-flow, which
  goes through `run_turn` like any other evaluation).

## Implementation plan

Documentation of intended work, not a commitment to build now. Each parcel
compiles and tests alone. The Typed Spine is the keystone: most localised,
establishes the margin render loop, and makes the type system legible on its
own.

```
0  Scaffold        StructuralFrontend: ratatui frame, prompt area, event
                   loop inside Frontend::read; RAL_SURFACE selector;
                   degradation to MinimalFrontend verified first.
1  Typed Spine     per-stage types from compile_and_typecheck + Pipeline
                   nodes; substrate ext #1 (annotate pass); flare on
                   mismatch; debounced re-inference. Keystone.
2  Handles Matrix  dashboard over HandleInner + JobTable; streaming
                   SurfaceBuffer; substrate ext #3 (job-table in read);
                   point-at-value actions.
3  Reactive Worksheet  binding DAG from free_refs + SessionSchemes;
                   substrate ext #2 (edge retention); transitive re-flow
                   with pinning; effectful-by-default classification via
                   mode system; worksheet fork.
```

Phase 0 and the Typed Spine (1) land first. The Handles Matrix (2) is
independent and follows. The Worksheet (3) is last: its re-flow semantics
are the most subtle and it depends on the edge-retention substrate change.
Type holes are a follow-on to the spine, not a blocker.

## Test plan

- **The spine shows inter-stage types.** A three-stage pipeline renders the
  type at each join; a stage whose output will not feed the next flares
  before Enter. (Substrate ext #1 verified here.)
- **The matrix tracks live work.** A `par` over three items shows three rows
  with correct `HandleState` transitions (pending → settled/cancelled); a
  stopped pgid job shows `stopped` and responds to `fg`. Captured bytes
  stream into a pending row.
- **The worksheet re-flows.** Editing an upstream binding re-evaluates its
  transitive dependents and updates their types and previews; a pinned node
  does not re-evaluate; a side-effecting binding is manual by default and
  does not re-run on upstream change.
- **The worksheet forks.** A forked worksheet holds an independent
  environment; edits in one branch do not mutate the other; both render
  simultaneously.
- **Degradation.** Under `RAL_SURFACE=minimal` and on a dumb terminal, every
  projection collapses and the prompt behaves as the existing line-editor.

## Risks

- **Per-keystroke cost.** The spine re-infers on the partial buffer; the
  worksheet re-flows on edits. Both must feel instant. Debouncing handles
  the spine; the worksheet's re-flow is user-initiated (on submit, not per
  key), so it is not on the hot path. A pathological deep DAG could re-flow
  slowly — cap re-flow depth or render progress.
- **Worksheet side-effect classification.** The mode-system verdict must be
  sound: a binding that looks pure but calls an effectful alias must be
  classified effectful. The verdict is the checker's, not a new heuristic,
  but the classification must cover aliases/handlers, not just bare `Exec`.
- **Detached-worker visibility.** Handles held only by the runtime (not in
  an env binding) may not appear in the matrix without a runtime enumeration
  hook. First cut may limit the matrix to env-held handles + pgid jobs.

## See also

[[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]] (the
Bertin encoding this borrows, and the exarch surface ral's margin shares a
vocabulary with), [[decisions/260603_session-scheme-continuity|session-scheme-continuity]]
(the per-turn `SessionSchemes` the Typed Spine and Worksheet read),
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-primitives-detached-vs-structured]]
(the handle model the Matrix projects),
[[decisions/260617_turn-local-state|turn-local-state]] (the `Shell` state
lifetimes the Worksheet's snapshot isolation rests on),
[[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]]
(the REPL/builtin boundary the projection layer respects),
[[design/types|types]], [[design/pipelines|pipelines]],
[[design/scoping|scoping]], [[map/repl|repl]], [[map/repl/frontend|frontend]],
[[map/repl/loop|loop]], [[map/repl/jobs|jobs]], [[map/core/typecheck|typecheck]],
[[map/core/io-process|io-process]], [[map/exarch/frontend|exarch frontend]].
