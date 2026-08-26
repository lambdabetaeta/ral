---
verified_at_commit: 1dd9935f
verified_at_date: 2026-08-26
anchors: [Machine, step_eval, step_return, step_halt, Frame, Focus, Terminal, Closure, Env, run_phrases, Phrase, evaluate, apply, reserve, PipeNode, WireShell, NESTED_MACHINE_LIMIT]
---

# The evaluator: a CEK machine over computation closures

The evaluator is one abstract machine, `core/src/evaluator/machine.rs`. Its
state is a **focus** and a **stack**: `Machine { focus: Focus, stack:
Vec<Frame> }`, stepped against the store `&mut Shell` and the run's
`&Mooring`. Nothing else in the crate constructs a state or sees the stack;
the module's two doors are `evaluate(closure, …)` — inject ⟨M, E⟩ over the
empty stack and step until it is empty — and `apply(f, args, …)`, the same
from a closed value meeting arguments. Both return `Settled<Value>`.

**What is in focus is a closure.** `Closure { comp: Arc<Comp>, env: Env }`
pairs a [[map/core/ir|computation]] with the environment its free variables
read. `Focus::Eval(closure)` is a computation to step; `Focus::Return(t)` is a
terminal meeting the frame above; `Focus::Halt(Break)` is a signal climbing
the stack. A terminal has two shapes, `Terminal::Value(v)` and
`Terminal::Lambda(closure)` — a λ is canonical at `A → C` and is never a
value, so a `Lam` in focus returns as `Lambda` and the frame above decides:
`Apply` consumes it by β, a computation-holed frame (`Redirect`, `Unmask`,
`Within`, `Grant`, `Try`, `Chain`) passes it through, a value-holed frame
(`To`, `Decode`, `Capture`, `Source`, `Guard`, `Cleanup`, `Audit`) halts with
the bare-lambda error — unreachable for a checked program, since the checker
η-expands every arrow-typed computation into a thunked λ (SPEC §17.8, S3).

**One thunk value.** `Value::Thunk(Closure)` is a computation closure held
as data; `force` of it puts the closure in focus and pushes nothing, so
`force(thunk M) = M` and a forced block's `cd` persists exactly as a
lambda's does ([[design/scoping|scoping]]). Whether a thunk "is a lambda" is
read off the body's shape by `Comp::arrow`, never stored.

**`step` is the tables.** `Machine::step` dispatches on the focus:
`step_eval` has one match arm per `CompKind` (the ξ-rules: `Return` closes
its value, `Bind` swaps stdout to the ambient sink and pushes `To`, `App`
closes its arguments then pushes `Apply` and evaluates the head, `Rec`
unfolds the n-ary group, `Exec` classifies the head through the lexical
environment, `Pipeline` launches and pushes `Pipe`, the six handler forms
close their operands, install, push their frame and force the body …);
`step_return` and `step_halt` have one arm per `Frame` — the two columns of
the frame table. No arm calls another arm; no arm loops. A rule that raises
stamps the span of the node that pushed the frame.

**Frames hold environments, which is what makes extent structural.** `M to
x. N` pushes `To { bind, env: E, prev_stdout }` *before* M runs; when M
returns a value, `E[x ↦ v]` is built from the frame's own `E`, so `x`
scopes over `N` and nothing else whatever M did. `Chain`, `Apply`, `Source`,
`Try` and `Guard` likewise carry the `Env` they resume under. Frames hold
`Arc`s into the IR, never cloned IR, and undo tokens, never a `Context`
clone: `Redirect(Box<RedirectState>)` tears down and settles its writes,
`Within(WithinUndo)` restores env overrides, dir and handlers, `Grant` pops
the capability stack, `Unmask` restores the masked handler, `Try`/`Audit`
close their trail scope. `Frame` is at most 128 bytes (asserted at compile
time; `Redirect`, `Unmask`, `Pipe` and the `Env` of `Try`/`Guard` are boxed).

**Tail calls push nothing.** β binds the parameter into the closure's
environment and puts the body in focus; an `Apply` frame is pushed only when
arguments remain (currying). So a call in tail position costs no frame, and
depth is simply `stack.len()`. `reserve` is the cap — `session.stack_limit`,
default 100 000 frames, the `--recursion-limit` knob — and every pushing rule
calls it *before* any effect (sink swap, redirect entry, grant push), so a
refused push leaks nothing; `push` itself cannot fail.

**Recursion is `rec`, n-ary.** `Rec { group, index }` binds every member's
name to the thunk of its own projection and runs the chosen member; a
recursive reference forces its name, which re-enters `Rec` and re-extends
from the outer environment. Bodies are never rewritten; a group of one is
Levy's `rec f. M`. Cancellation is polled here and at `Bind`, `App`, `Exec`,
`Source`, `Chain` advance and β, so `let f = { !f }; !f` is interruptible.

**The environment is a map, and it is not the store.** `Env`
(`core/src/types/env.rs`) is three tiers — the language natives, the frozen
prelude, and a persistent `imbl::HashMap` of everything bound since. `bind`
is an insert that disturbs no environment a closure captured; `clone` is
O(1). The **store** is everything else on `Shell`: sinks, `$?`, the dynamic
`Context` (grants, handlers, env overrides, cwd, args, modules, hooks), the
trail, workers, leases. `Context` is read in O(1) by capability checks and
command dispatch and changed only by frames holding their own undo; it is
never part of a closure ([[map/core/shell-state|shell-state]]).

**The top level is a sequence of phrases** (`core/src/evaluator.rs`,
`run_phrases`). A `Toplevel` is `Phrase::{Define, Source, Run}`; each phrase
is a closed computation over the session environment `shell.env`, and a
`Define` extends that environment *for every phrase after it, in this run
and every later one* — installed as it lands, so a `use` in the next phrase
sees it, and a run that halts has installed exactly the `Define`s that ran.
A block is a right-nested `Bind` chain, `a; b` being `a to _. b`, so a `let`
inside a block scopes over the rest of the block by structure. `source` is
a form: `Phrase::Source` at the top level, `CompKind::Source { path, rest }`
in a block, its `Define`s scoping over `rest`; a file that halts halts its
caller after the definitions before the halt are installed. `run_phrases`
takes a `Mode` — `Session`, `Local`, `Module`, `Prelude` — which alone
decides leases and the PATH-shadow check.

**Boundaries.** Three things start a fresh machine over the empty stack: a
run-door phrase, a worker thread (`spawn`/`watch`/`service`), and a pipeline
stage child (`child_eval`). A native that applies a user function — the
collection combinators, hook dispatch, pattern defaults — runs a *nested*
machine on the host stack through `machine::apply`; `NESTED_MACHINE_LIMIT`
(set by `nested_machines_fit_a_worker_stack` against a 2 MiB thread) caps
that nesting with a clean error. Natives that need the lexical environment
(`help`, `explain`) receive it as a parameter.

**Pipes are nodes between machines** ([[internals/pipeline-execution|pipeline
execution]]). A multi-stage pipeline is a configuration: each stage is a
machine over the empty stack in its own process, and the parent holds
`Frame::Pipe(PipeNode)` — the process group, the running stages, the yield
mode — which `join`s (collect, then finish) when its placeholder terminal
meets it, so the pipeline's outcome climbs the parent's frames like any
other and a halt in the join unwinds the same stack. What crosses to a stage
is `WireShell { env, last_status, stack_limit, context }`: the bindings tier
of one environment, interned by the identity of its root, seated under the
receiver's own natives and prelude — the two constant tiers never cross —
and **no frame ever crosses**: a stage's stack is empty by construction.

**Panics and cancellation.** `evaluate`/`apply` wrap the step loop in
`catch_unwind`; on a panic every frame is `abandon`ed top-down — sinks
restored, redirects torn down, trail scopes closed, undo applied — and the
run door restores its checkpoint `(env, context, last_status)`, so a panic
commits nothing. The depth counter is lowered on both paths.

See also [[design/cbpv|cbpv]], [[design/scoping|scoping]],
[[design/control-operators|control-operators]],
[[decisions/260826_the-evaluator-steps-closures|the-evaluator-steps-closures]];
code maps [[map/core/evaluator|evaluator]],
[[map/core/shell-state|shell-state]], [[map/core/runtime|runtime]]. The
formal account is `docs/SPEC.md` §17.8; the kernel in
`dev/abstract-machines` is due to be rewritten as this machine.
