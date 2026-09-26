---
verified_at_commit: e1bace22
verified_at_date: 2026-09-26
anchors: [Machine, step_eval, eval_rules, step_return, step_halt, Frame, Focus, Terminal, Closure, Closure::new, Node::new, Env, restrict, Signature, lookup, form, run_phrases, Phrase, evaluate, apply, reserve, PipeNode, WireShell, NESTED_MACHINE_LIMIT, force, apply_handler, launch_thread_stage, Assemble]
---

# The evaluator: a CEK machine over computation closures

The evaluator is one abstract machine, `core/src/evaluator/machine.rs`. Its
state is a **focus** and a **stack**: `Machine { focus: Focus, stack:
Vec<Frame> }`, stepped against the store `&mut Shell` and the run's
`&Mooring`. Nothing else in the crate constructs a state or sees the stack;
the module's doors are `evaluate(comp, env, …)` — inject ⟨M, E⟩ over the
empty stack and step until it is empty — `apply(f, args, …)`, the same
from a closed value meeting arguments, its argument-free twin `force(v, …)`,
and `apply_handler`, `detach`'s one-shot handler call. All return
`Settled<Value>`.

**What is in focus is a computation closure.** `Focus::Eval { comp, env }`
pairs a [[map/core/ir|computation]] with the whole environment it was reached
in: transient — never stored, sent, or kept alive by a binding — and so a type
apart from `Closure`, the thunk value's. `Focus::Return(t)` is a
terminal meeting the frame above; `Focus::Halt(Break)` is a signal climbing
the stack. A terminal has two shapes, `Terminal::Value(v)` and
`Terminal::Lambda { comp, env }` — a λ is canonical at `A → C` and is never a
value, so a `Lam` in focus returns as `Lambda` and the frame above decides:
`Apply` consumes it by β, a computation-holed frame (`Redirect`, `Unmask`,
`Within`, `Grant`, `Try`) passes it through, a value-holed frame
(`To`, `Capture`, `Guard`, `Cleanup`, `Audit`) halts with the
bare-lambda error — unreachable for a checked program, since the checker
η-expands every arrow-typed computation into a thunked λ (SPEC §17.8).
`Decode` has no frame: the kernel's `decode` takes a value, so it closes and
reads it inline in `step_eval`, and the checker reaches it through a bind
over `Capture` rather than nesting the two.

**One thunk value.** `Value::Thunk(Closure)` is `⟨M, ρ|occ(M)⟩`, built only
by `Closure::new`, which `restrict`s the environment to `occ`, read off the
`ThunkNode` that owns `M` (`Val::Thunk(Arc<ThunkNode>)`) — occ(M) is
computed once, by `Node::new`, where the node is built, not walked at every
capture
([[decisions/260926_a-closure-keeps-only-what-it-mentions|a-closure-keeps-only-what-it-mentions]]).
`force` of it puts its computation and environment in focus and pushes
nothing, so `force(thunk M) = M` and a forced block's `cd` persists exactly
as a lambda's does ([[design/scoping|scoping]]). A literal `!{ … }` takes
that equation as a rule (U-β): `Force(Val::Thunk(node))` puts `node.shape()`
in focus under the current environment, closing nothing. Whether a thunk "is
a lambda" is read off the body's shape by `Comp::arrow`, never stored.

**A list, record or map literal is a value closure too.** `form(val, env,
sig)` (`core/src/evaluator/val.rs`) is CBPV's one-step rule for every value:
a constant is itself, a name is `lookup(name, env, sig)`, a variant forms its
payload, `thunk M` is `Closure::new`, and a plain literal is `List::literal`
or `Map::literal` — `⟨V, ρ|occ(V)⟩` — unless one of the names it *directly*
mentions (through variant payloads and nested literals, never inside a
thunk) is answered only by Σ: a `Literal`'s inspection reads ρ alone, with no
Σ fallback, so that name could never be read back out, and `form` builds the
literal eagerly instead, forming each element with `sig`. Inspecting a
`Literal`'s element is the distributive law, one layer: a name is lent from
ρ, a constant is built, and a nested literal or thunk closes over the *same*
ρ — two refcount bumps, not a second restriction.

**`step` is the tables.** `Machine::step` dispatches on the focus:
`eval_rules` has one match arm per `CompKind` (the ξ-rules: `Return` closes
its value, `Bind` swaps stdout to the ambient sink and pushes `To`, `App`
closes its arguments then pushes `Apply` and evaluates the head, `Rec`
unfolds the n-ary group, `Exec` classifies the head through the lexical
environment, `Pipeline` launches and joins its node, the six handler forms
close their operands, install, push their frame and force the body …);
`Assemble` is the one rule that does O(data) work over what looks like value
syntax: a list, record, or map literal with a spread or a computed key
elaborates to it rather than to `Return`, and `evaluator/assemble.rs` builds
the collection ([[map/core/evaluator|evaluator]]) — a plain literal, with
none of those, stays a `Return` and closes for free;
`step_return` and `step_halt` have one arm per `Frame` — the two columns of
the frame table. No arm calls another arm; no arm loops.

**A rule cannot leave unlocated.** `eval_rules` returns `Result<Focus,
Break>`, so every rule raises with `?` rather than by building a `Focus::Halt`
of its own; `step_eval` is its sole caller and does
`unwrap_or_else(Focus::Halt)` then `stamp_focus(focus, comp.span)`. There is
exactly one exit, so a raising rule is stamped with the span of the node it
is the rule for, and no arm can be written that skips the stamp. `step_case`
and `step_exec` are rules under the same discipline and return the same
`Result`. (Before this shape, a bare `return` in the `Rec` arm bypassed the
stamp and cancelling a recursive definition rendered without a caret.)

**`To` holds its environment, which is what makes extent structural.** `M to
x. N` pushes `To { bind, env: E, prev_stdout }` *before* M runs; when M
returns a value, `E[x ↦ v]` is built from the frame's own `E`, so `x`
scopes over `N` and nothing else whatever M did. It is the only frame with
syntax to close over, so the only one carrying an `Env`. `Cleanup` is
the kernel's `to _` with a settled rest: it drops the cleanup's value and
resumes the outcome it holds (`βguard-val`). Frames hold
`Arc`s into the IR, never cloned IR, and undo tokens, never a `Context`
clone: `Redirect(Box<RedirectState>)` tears down and settles its writes,
`Within(WithinUndo)` restores env overrides, the whole cwd cell and handlers, `Grant` pops
the capability stack, `Unmask` restores the masked handler, `Audit`
closes its trail scope and restores the capture policy; `Try` holds only its
handler. `Frame` is at most 128 bytes (asserted at compile
time; `Redirect` and `Unmask` are boxed).

`a ? b ? c` has no frame of its own: it elaborates to nested `try` (kernel
`_؟_`, `Core.Derived`), right-associated so the last arm stays in tail
position.

**Tail calls push nothing.** β binds the parameter into the closure's
environment and puts the body in focus; an `Apply` frame is pushed only when
arguments remain (currying). So a call in tail position costs no frame, and
depth is simply `stack.len()`. `reserve` is the cap — `session.stack_limit`,
default 100 000 frames, the `--recursion-limit` knob — and every pushing rule
calls it *before* any effect (sink swap, redirect entry, grant push), so a
refused push leaks nothing; `push` itself cannot fail.

**Recursion is `rec`, n-ary.** `Rec { group: Arc<GroupNode>, index }` reads
occ(g) off `group` — computed once, at the node's build, over every member's
body — and restricts ρ to it: one `restrict` call, whose result every sibling's
thunk shares in this unfold. The member in focus binds to `comp` itself,
`Arc::clone`d, not rebuilt; every other member's thunk is a fresh `Rec` node
over the same `group` (`rec_node`). The n bindings go into ρ|occ(g) with one
`Env::extend`. A recursive reference forces its name, which re-enters `Rec`
over the forced closure's own environment — already ρ|occ(g), so `restrict`
answers the identity and every later unfold shares that one root. Bodies are
never rewritten; a group of one is Levy's `rec f. M`, and binds its own
member to the node already in focus, allocating nothing. Cancellation is
polled here and at `Bind`, `App`, `Exec` advance and β, so `let f = { !f };
!f` is interruptible.

**The environment is a map, and it is not the store.** `Env`
(`core/src/types/env.rs`) is ρ alone: a sorted array of at most `SMALL` (8) bindings,
a persistent hash map past it, chosen by size and invisible to its readers.
`bind`/`extend` produce a fresh environment that disturbs none a closure
captured; `clone` is O(1); `restrict` narrows it to a closure's names,
identity when nothing is dropped. Σ — the language natives and the frozen
prelude — is `Signature` (`core/src/types/signature.rs`), one per shell,
never part of any environment; `crate::types::lookup(name, env, sig)` reads
ρ, then Σ's prelude, then Σ's natives, and every rule that resolves a name
(`Val::Variable` in `form`, `Exec`'s bare-head lookup in
`command_call::resolve`) goes through it. The **store** is everything else
on `Shell`: `sig`, sinks, the dynamic `Context` (grants, handlers, env
overrides, cwd, args, modules, hooks), the trail, workers, leases. `Context`
is read in O(1) by capability checks and command dispatch and changed only
by frames holding their own undo; neither it nor Σ is ever part of a closure
([[map/core/shell-state|shell-state]]).

**The top level is a sequence of phrases** (`core/src/evaluator.rs`,
`run_phrases`). A `Toplevel` is `Phrase::{Define, Run}`; each phrase
is a closed computation over the session environment `shell.env`, and a
`Define` extends that environment *for every phrase after it, in this run
and every later one* — installed as it lands, so a `use` in the next phrase
sees it, and a run that halts has installed exactly the `Define`s that ran.
A block is a right-nested `Bind` chain, `a; b` being `a to _. b`, so a `let`
inside a block scopes over the rest of the block by structure. `run_phrases`
takes a `Mode` — `Session`, `Local`, `Module`, `Prelude` — which alone
decides leases and the PATH-shadow check.

**Boundaries.** Three things start a fresh machine over the empty stack: a
run-door phrase, a worker thread (`spawn`/`watch`/`service`), and a pipeline
stage thread (`runtime/pipeline/thread.rs`). A native that applies a user function — the
collection combinators, hook dispatch — runs a *nested*
machine on the host stack through `machine::apply`; `NESTED_MACHINE_LIMIT`
(calibrated by `nested_machines_fit_a_worker_stack` on a 2 MiB thread, a
quarter of the 8 MiB `Shell::spawn_thread` gives a worker) caps
that nesting with a clean error. No native reads a lexical environment:
`help` and `explain` reflect on the session.

**Pipes are nodes between machines** ([[internals/pipeline-execution|pipeline
execution]]). A multi-stage pipeline is a configuration: an external stage is
a process in the pipeline's group, and a ral-written stage is a thread
(`launch_thread_stage` via `Shell::spawn_thread`) whose shell starts from the
parent's session and runs `machine::evaluate(comp, env, …)` — the stage's
subterm under the node's own environment — over the empty stack. The parent's
`Pipeline` rule holds a `PipeNode` — the process group, the running stages,
the yield mode — which it `launch`es and `join`s (collect, then finish) in one
step: no frame, because nothing runs beneath the node; the outcome climbs the
parent's frames like any other rule's terminal. **No frame ever crosses**: not
to a stage, whose stack is empty by construction, nor to a hatched engine,
which receives the engine seed's `WireShell { env, stack_limit, context }`
(`core/src/engine_seed.rs`) — one environment, ρ alone, interned by its
allocation's identity; Σ never crosses, so the receiver answers a decoded
closure's Σ names from its own.

**Panics and cancellation.** `evaluate`/`apply` wrap the step loop in
`catch_unwind`; on a panic every frame is `abandon`ed top-down — sinks
restored, redirects torn down, trail scopes closed, undo applied — and the
run door restores its checkpoint `(env, context)`, so a panic
commits nothing. The depth counter is lowered on both paths.

See also [[design/cbpv|cbpv]], [[design/scoping|scoping]],
[[design/control-operators|control-operators]],
[[decisions/260826_the-evaluator-steps-closures|the-evaluator-steps-closures]];
code maps [[map/core/evaluator|evaluator]],
[[map/core/shell-state|shell-state]], [[map/core/runtime|runtime]]. The
formal account is `docs/SPEC.md` §17.8. The Agda kernel (`dev/agda`) is a
substitution-based CK machine and stays one: a closure machine is a
refinement to be proved against it, not its foundation.
