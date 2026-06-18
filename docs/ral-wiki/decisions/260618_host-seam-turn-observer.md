---
status: proposed
---

# Both hosts drive a turn through one core entry, handing it a borrowed observer

**The REPL and exarch must drive a turn the same way: through a single core
entry — `Shell::run_turn(src, TurnRequest) -> TurnReport` — that builds the
frame, arms the reaper, installs a *borrowed, turn-scoped observer*, and hands
back a neutral report. Neither host constructs `TurnFrame`/`IoFrame`, mints a
`SurfaceSink`, arms `arm_lifetime`, or names `eval_turn`.** This finishes the
job [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] started —
that ADR lifted *evaluation* into core but left *frame construction* duplicated
in each host — and it kills the daemon-task hang as a side effect: the observer
exarch supplies is a borrow, a detached worker needs `'static`, so the observer
provably cannot escape into one. The coupling is not merely fixed, it is
unrepresentable.

## Context

A Terminal-Bench run (`exarch-bench/.../2026-06-18__13-40-33`) hangs on the two
daemon tasks, `kv-store-grpc` and `pypi-server`. Both score reward `1.0`, the
model finishes in ~80 s, then exarch idles ~13.5 min until Harbor SIGKILLs it at
the 900 s wall. Agent `result.json` is 0 bytes for both; `job.log` shows `failed
to read exarch result.json`; `exception.txt` shows Harbor blocked in
`process.communicate()` — **the exarch process itself never exited.** The session
log ends on the model's final no-tool-call message with **no `session_ended`**:
the work was done, exarch could not return.

[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
already cut two couplings between a turn and a detached worker — the **cancel
scope** (workers hang at the durable root, not the foreground) and the **data
pipe** (`spawn_child` gives the worker its own buffers, so there is no turn-owned
pipe to hold open,
[[internals/output-capture-and-detachment|output-capture-and-detachment]]). That
ADR states the governing invariant: *"Detachment may hold only root-owned or
handle-owned resources, **never a foreground frame's capture state.**"* This bug
is a **third** piece of foreground-frame capture state that leaked into
detachment — the structured-event surface — and it leaked because exarch builds
the turn frame by hand and injects that surface itself.

### The mechanism, exactly

`Session::apply` runs the whole round-trip loop internally
(`exarch/src/session.rs`), so there is one event channel per turn, owned by
`pump`.

1. `pump` (`exarch/src/bus.rs:305`) opens `channel()`, runs the work on a
   `std::thread::scope` worker, and on the main thread calls `sink.drive(rx)`
   (`bus.rs:330`). The default `drive` is `while let Ok(ev) = rx.recv()`
   (`bus.rs:293`) — it returns only on channel **disconnect**, i.e. when the last
   `Sender<Event>` drops. Its doc says so: *"drains the channel until the worker
   drops its sender."*
2. `Emitter: Clone` clones its `Sender` (`bus.rs:245`). When the model runs a
   `spawn`, exarch's `run_shell` builds
   `surface = Arc::new({ let emit = emit.clone(); move |v: RalValue| … })`
   (`exarch/src/shell_eval.rs:80`) — a **`'static` `SurfaceSink`**
   (`core/src/types/shell/mod.rs`, `Arc<dyn Fn(Value)+Send+Sync>`) capturing a
   clone of the turn's `Emitter`, hence a clone of the `Sender`.
3. exarch stuffs that surface into a core `TurnFrame { io: IoFrame::Capture { …,
   surface: Some(surface) } }` (`shell_eval.rs:102`) and calls `eval_turn`.
4. Core's `Shell::spawn_thread` (`core/src/types/shell/inherit.rs:195`) clones
   `self.turn.surface` into each detached worker (`:205`, `:214`). Because the
   surface is `'static`, this compiles and the worker now holds exarch's `Sender`
   for as long as it lives.
5. A server never terminates → the `Sender` never drops → `drive` never returns →
   `pump` never returns → `run_turn` never returns. `result.json` is printed only
   *after* the turn loop (`exarch/src/headless.rs`), so nothing is written.

The natural experiment is in the same run: **torch** also `spawn`ed a worker (a
`pip install`) and did *not* hang — `pip` finished, dropped its `Sender`, and the
channel disconnected (its `result.json` is intact, `stop_reason: step_cap`).

### Why the seam was open: 260616 unified evaluation, not the frame

[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] lifted one
top-level turn into `ral_core` as `eval_turn(shell, src, frame) -> TurnOutcome`
(`core/src/turn.rs`) and declared both hosts *"frame suppliers over one
evaluator."* That removed the duplicated *spine*. But each host still assembles
the **frame** itself:

- The REPL's `execute_input` hand-builds `TurnFrame { io: IoFrame::Inherit, … }`
  (`ral/src/repl/exec.rs:102-109`).
- exarch's `run_shell` hand-builds the capture buffers (`shell_eval.rs:66-67`),
  arms and disarms the wall (`arm_lifetime` at `:75`, `drop(wall)` at `:121`),
  mints the `'static` surface closure (`:80-87`), assembles
  `TurnFrame { io: IoFrame::Capture { … } }` (`:102-114`), calls `eval_turn`
  (`:116`), and decodes core `Value` into rail events with `value_to_kind`
  (`:248-301`).

So 260616's unification stopped one level short. The frame — the IO regime, the
reaper arming, the surface — is exactly where the two hosts still diverge by
hand, and it is exactly where this bug lives: a host that mints its own
`'static` surface can leak it into a worker. **Fixing this for exarch alone would
re-open the split one level up**: a richer seam (observer, reaper-in-core) for
exarch, the old hand-rolled frame for the REPL. The fix is to lift frame
construction behind one entry too, so both hosts become *request* suppliers, not
*frame* suppliers.

## Decision

Add one entry to the host-embedding seam
([[decisions/260610_host-embedding-api|host-embedding-api]]), in `core/src/host.rs`,
that both hosts call:

```rust
impl Shell {
    pub fn run_turn(&mut self, src: &str, req: TurnRequest<'_>) -> TurnReport;
}

pub struct TurnRequest<'a> {
    pub script_name: &'a str,          // "<stdin>" (REPL) | "<tool>" (exarch)
    pub caps: Capabilities,            // root() (REPL) | grant profile (exarch)
    pub io: IoMode,                    // Inherit (REPL) | Capture (exarch)
    pub wall: Option<Duration>,        // None (REPL) | Some(30s) (exarch)
    pub detached_ceiling: Option<Duration>, // None (REPL) | Some(1h) (exarch)
    pub observer: Option<&'a dyn TurnObserver>, // None (REPL) | Some(rail) (exarch)
    pub lifecycle: &'a mut dyn TurnLifecycle,   // REPL hooks+jobs | () for exarch
}

pub enum IoMode { Inherit, Capture }   // Capture => core-owned buffers, Terminal stdin

/// A borrowed, turn-scoped sink for whatever the running turn emits to the
/// `surface` builtin. It carries the raw `Value`; what an emission *means*
/// (a patch, a write, a meter) is the host's vocabulary, decoded host-side.
pub trait TurnObserver { fn on_surface(&self, v: &Value); }   // <-- borrowed, not 'static

pub struct TurnReport {
    pub outcome: TurnOutcome,          // Static{diagnostics} | Runtime{ result: Settled<Value>, .. }
    pub captured: Option<Captured>,    // Some iff io == Capture; None under Inherit
    pub timed_out: bool,
}
pub struct Captured { pub stdout: Vec<u8>, pub stderr: Vec<u8> }
```

Four parts, in priority order.

1. **Core owns the frame; the host supplies a request and reads a report.**
   `run_turn` builds the `IoFrame` (under `Capture`, core mints the
   `Sink::Buffer` stdout/stderr and a `Source::Terminal` stdin; under `Inherit`,
   it runs on the session's live streams), installs the observer as the turn's
   surface, arms the wall and `detached_ceiling` on `process::reaper`, calls the
   existing `eval_turn`, then `mem::take`s the buffers into `Captured` and reports
   `timed_out`. Everything `run_shell` and `execute_input` do *to assemble a
   frame* moves here once. `TurnOutcome` is unchanged — it stays the neutral
   classification both hosts already render.

2. **The observer is a borrow, so it cannot reach a detached worker — by the
   type system, not by discipline.** `observer: &dyn TurnObserver` is
   non-`'static`. A `spawn` worker is a `std::thread::spawn` closure requiring
   `'static` (`inherit.rs:211`), and `spawn_thread` clones `self.turn.surface`
   into it (`inherit.rs:205`,`:214`). If the turn's surface slot holds the
   borrowed observer for the eval's lifetime rather than a `'static`
   `SurfaceSink`, that clone no longer typechecks, and the worker takes `None` —
   exactly as `spawn_child` already does for IO buffers. The leak that caused the
   hang becomes a compile error. This is the concurrency ADR's "detachment holds
   no foreground-frame capture state" invariant, finally enforced by lifetime
   instead of comment.

3. **`Value` may cross — and *only* `Value`.** This deliberately relaxes
   [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
   on the turn-eval path: `TurnReport.outcome` carries a `Settled<Value>`, and
   exarch may read and render it (its raw-string-vs-pretty-JSON choice,
   `shell_eval.rs:211-227`, stays exarch's presentation concern). In exchange we
   get **one** neutral report serving both hosts instead of a `Value`-free report
   forked for exarch and a `Value`-rich path kept for the REPL. The relaxation is
   scoped to `Value`. **Nothing else of core's internal representation crosses**:
   no `TurnFrame`, `IoFrame`, `SurfaceSink`, `Sink`, `Source`, `arm_lifetime`, or
   `eval_turn` is named by either host on this path. A pleasant consequence:
   because the observer carries raw `Value`, **core never learns exarch's rail
   vocabulary** — `value_to_kind` and the `` `patch ``/`` `wrote ``/`` `task
   ``/`` `meter `` shapes stay *in exarch*, in its `TurnObserver` impl. Core
   forwards a `Value`; what it means is decoded where it is rendered. The surface
   mechanism in core stays generic ("the turn may emit `Value`s to a borrowed
   observer"); the event taxonomy stays the host's.

4. **The observer is optional, and surfacing stays exarch's.** `edit` and the
   `surface` builtin are exarch affordances (installed via `EXARCH_BUILTINS` and
   the agent library, per
   [[decisions/260617_watch-repl-builtin|watch-repl-builtin]]'s
   registration-by-host model); nothing the REPL can run ever calls `surface`, so
   the REPL passes `observer: None` and loses nothing. But the *channel* lives on
   the shared `TurnRequest`, not in exarch, so a future REPL use — say, rendering
   `meter` progress from a long pipeline — is a one-line `Some(&…)`, never a
   re-fork. REPL `watch` is untouched and orthogonal: it streams to its own
   durable `LineFramed` sink, a host-owned resource registered out-of-band, not
   the per-turn observer ([[decisions/260617_watch-repl-builtin|watch-repl-builtin]]).

This *removes paths* rather than adding one: there is no longer a host-built
frame, a `'static` surface, or a reaper-arming dance in either host. Across the
seam travel only the host's currency (`Capabilities`, `Duration`s,
`&dyn TurnObserver`, `IoMode`, `&mut dyn TurnLifecycle`, the `TurnReport` with its
captured bytes), the opaque `&mut Shell` embedding handle, and — by the
concession — `Value`.

## Why one API covers everything

Walking every policy axis the prior ADRs identified, and showing each is a
*field of the request* or *orthogonal*, never a reason to fork:

- **IO regime** → `IoMode`. `Inherit` gives the REPL its live streams and
  `captured: None`; `Capture` gives exarch core-owned buffers returned as
  `captured: Some`. The two cases are the existing `IoFrame` sum
  (`core/src/turn.rs:35`), now selected by a host-currency enum.
- **Cancellation / foreground scope** → built inside `run_turn` as a child of the
  durable root (the scope rule of
  [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]), with the
  foreground/root signal slots published per turn by the existing `TurnGuard`
  (`core/src/turn.rs`). Both hosts get Ctrl-C / Esc / Ctrl-`\` routing for free;
  neither names a `CancelScope`. The `wall` arms a reaper entry or, when `None`,
  leaves the scope unarmed.
- **Capabilities** → `caps`, judged only by `capability::check_*`
  ([[decisions/260605_witness-collapse|witness-collapse]]). REPL `root()` (the ⊤
  identity), exarch its grant profile.
- **Lifecycle** → `&mut dyn TurnLifecycle` (`core/src/turn.rs:52`). The REPL
  supplies `pre-exec`/`chpwd`/`post-exec` and `Escape::Stopped` job registration;
  exarch supplies the `()` no-op. (The originating sketch dropped this axis; the
  common form must keep it — the REPL needs it.)
- **Detached-worker ceiling** → `detached_ceiling`; the death-clock armed on the
  shared `process::reaper` inside `run_turn`. REPL `None`, exarch `Some(1h)`.
- **Structured surface** → `Option<&dyn TurnObserver>`; the borrow makes the hang
  unrepresentable for *both* hosts, and the decode stays host-side.
- **`watch` / durable streaming** → untouched. A REPL-only registered builtin
  with its own durable sink; not a per-turn axis, so the common API needs nothing
  from it.
- **Outcome classification** → computed once in core (`TurnOutcome`); both hosts
  only *render*. The duplication 260616 killed stays killed.
- **Return value** → crosses as `Value` in `TurnReport.outcome`; one return type,
  no façade split.

The only deliberate asymmetry left is *rendering*: the REPL pretty-prints the
`Value` themed (`print_result`, `exec.rs:22`) and registers stopped jobs, while
exarch caps the captured bytes into a `ToolResult`, synthesises the timeout
message off `timed_out`, and appends sandbox denials. That is genuinely
host-specific presentation, fed by one neutral report — not a second evaluator.

## Implementation plan

### Core — `core/src/host.rs`, `core/src/turn.rs`, `inherit.rs`

1. Define `TurnRequest`, `IoMode`, `TurnObserver`, `TurnReport`, `Captured` in
   `host.rs` (the home [[decisions/260610_host-embedding-api|host-embedding-api]]
   established). `TurnObserver::on_surface(&Value)` carries raw `Value`; core
   defines no event taxonomy.
2. Add `Shell::run_turn`: from a `TurnRequest`, build the `TurnFrame` (mint
   `Capture` buffers + `Terminal` stdin, or `Inherit`), install the borrowed
   observer as the surface, arm the wall and ceiling on `process::reaper`, call
   `eval_turn`, collect `Captured` via `mem::take`, and read `timed_out` from the
   foreground scope's `CancelCause::Deadline`. Return a `TurnReport`. The wall
   arm/disarm currently in `shell_eval.rs:75,121` lives here.
3. Make the turn's surface slot carry the borrowed observer for the eval's
   lifetime rather than a `'static` `SurfaceSink`. The minimal form: give
   `TurnState`'s surface field the observer's lifetime so `spawn_thread`'s
   `'static` clone (`inherit.rs:214`) no longer typechecks against it, forcing the
   worker to take `None`. If threading a lifetime through the ephemeral
   `TurnState` proves too invasive, fall back to the **one-line structural fix**:
   `spawn_thread` simply does not copy the foreground surface into the child (it
   already builds a fresh child and copies only selected fields, `inherit.rs:205-218`).
   The borrowed form is preferred — it makes the invariant *unrepresentable*; the
   one-liner makes it merely *true*.

### exarch — `shell_eval.rs` collapses to an adapter

`run_shell` becomes: build a `TurnRequest { script_name: "<tool>", caps, io:
Capture, wall: Some(30s), detached_ceiling: Some(1h), observer: Some(&rail),
lifecycle: &mut () }`, call `shell.run_turn(cmd, req)`, and map the `TurnReport`
to the existing `Outcome`/`ToolResult` (cap the `captured` bytes, synthesise the
124 message from `timed_out`, append sandbox denials, render the `Value`). The
`RailObserver` is a thin `TurnObserver` impl wrapping the `Emitter`; it owns
`value_to_kind` (moved off the free function into the impl, `shell_eval.rs:248-301`)
and emits `Kind`s onto the rail. The buffer wiring, `arm_lifetime`, the surface
closure, the `TurnFrame`/`IoFrame` construction, and the `eval_turn` call all
leave `shell_eval.rs`. The `DETACHED_WORKER_CEILING` constant and the wall move
into the `TurnRequest`.

### ral — `repl/exec.rs` collapses to an adapter

`execute_input` becomes: build a `TurnRequest { script_name: "<stdin>", caps:
root(), io: Inherit, wall: None, detached_ceiling: None, observer: None,
lifecycle: &mut ReplLifecycle { runtime } }`, call `shell.run_turn(trimmed, req)`,
and match `report.outcome` exactly as today — `print_result` on `Ok`, exit code on
`Escape::Exit`, job registration on `Escape::Stopped`, ariadne on `Break::Error`
and on `Static` diagnostics. The hand-built `TurnFrame { Inherit }`
(`exec.rs:102-109`) is deleted; `captured` is `None`, as expected under
`Inherit`.

### What legitimately still crosses the seam

`Capabilities` (core-built from `--base`, carried opaquely, judged only by
`capability::check_*`); `Duration`s; `IoMode`; `&dyn TurnObserver`;
`&mut dyn TurnLifecycle`; the `TurnReport` (its `Captured` bytes are host
currency); the `&mut Shell` embedding handle (method-only); and — by deliberate
concession — `Value`, inside `TurnReport.outcome`. No host names `TurnFrame`,
`IoFrame`, `SurfaceSink`, `Sink`, `Source`, `arm_lifetime`, or `eval_turn`.

## Test plan

- **Type-level (the class):** a `// @compile-fail` fixture that tries to leak a
  turn observer into a `spawn` worker must fail to compile — the borrowed surface
  slot is itself the guard. If the one-line fallback is taken instead, an
  integration test stands in.
- **Integration (the instance):** a stub turn whose script is
  `spawn { <blocks forever> }` then a no-tool-call message; assert `run_turn`
  returns within a tight bound, a `TurnReport` is produced, and (driven through
  exarch's `Session`) `session_ended` is recorded. Name in the spirit of the
  concurrency ADR's
  `await_unwinds_on_foreground_cancel_sparing_the_worker`, e.g.
  `live_spawn_worker_does_not_pin_the_turn_observer`.
- **Parity (the unification):** one test drives the *same* source through
  `run_turn` under `IoMode::Inherit` (REPL shape) and `IoMode::Capture` (exarch
  shape) on a shared `Shell` and asserts the `outcome` classification agrees —
  pinning that the two hosts cannot re-diverge on the spine.
- **Seam (the regression that hid it):** assert neither host imports `TurnFrame`,
  `IoFrame`, `SurfaceSink`, `Sink`, `Source`, `arm_lifetime`, or `eval_turn` — a
  grep test in the spirit of
  [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]],
  narrowed to allow `Value`.
- **End-to-end:** re-run `kv-store-grpc` and `pypi-server`; assert no
  `AgentTimeoutError`, non-empty `result.json`, reward still `1.0` (the server
  must still be listening — detachment is preserved, only the *wait* is gone).

## Consequences

- The hang is impossible by construction: exarch's transport never reaches a core
  worker, because the observer it supplies cannot outlive the eval that borrows
  it — and the REPL could not make the mistake either.
- [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] is completed:
  both hosts are *request* suppliers over one driver. `shell_eval.rs` and
  `exec.rs` shrink to adapters — build a request, render a report — and the frame,
  reaper, and surface machinery they duplicated lives once in core.
- [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
  is *narrowed, not honoured*: `Value` now crosses on this path. The benefit is
  one shared API; the cost is that the `Value`-non-leak is no longer enforced by
  type here. Net of the concession, core ends up knowing *less* — it loses the
  rail event vocabulary, which moves into exarch's observer.
- `run_turn` returns the moment the model is done, so `result.json` and
  `session_ended` are written; the report's separate "flush usage incrementally"
  item drops in priority (it matters only if the model genuinely runs to the
  wall).
- The 1 h `detached_ceiling` is unchanged and correct as a backstop; it is no
  longer load-bearing for turn exit. Do **not** lower it to mask the bug.
- `internals/output-capture-and-detachment` needs a one-line correction: "no
  turn-owned pipe" was true, but the turn-owned **observer** was the
  foreground-frame capture state a detached worker could pin; it no longer can.

## Alternatives considered

- **Close the seam for exarch only** (the originating shape of this ADR: an
  exarch-facing `run_command` returning a `Value`-free report, REPL left on its
  hand-built `Inherit` frame). Rejected: it re-opens the two-path split
  260616 closed, one level up — a rich seam for exarch, the old frame for the
  REPL — and every shared-spine fix risks re-diverging. The borrowed-observer
  hang fix should protect both hosts, not one.
- **Keep `Value` out of exarch; fork the return type** (a `Value`-free
  `TurnReport` for exarch, a `Value`-rich outcome for the REPL, over one engine
  via two façades). Workable, and it preserves
  [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
  — but two return types for one driver, and a core-side `Value→host-currency`
  renderer, for a purity the team chose not to buy here. Superseded by the
  `Value`-may-cross concession.
- **A core `TurnEvent` taxonomy** (`Task`/`Patch`/`Wrote`/`Meter` in core, decoded
  by `run_turn`, observer receives host currency). Rejected once `Value` may
  cross: it puts exarch's *rail* vocabulary in core. Carrying raw `Value` to a
  borrowed observer keeps core generic and the taxonomy where it is rendered.
- **Patch the driver: an owned `Drive::Done` completion signal on exarch's event
  channel** (the worker sends an explicit terminator; `drive` stops on it, never
  on refcount). Fixes exarch's *own* loop robustly and is a good independent
  invariant — but it leaves the seam open: exarch still builds the frame and
  injects a `'static` surface. It treats a boundary leak as a transport bug. Keep
  it as optional defence-in-depth for `pump`, not the structural fix.
- **`Weak` surface** (the worker holds `Weak<Sender>`). Fixes the instance, keeps
  refcount-as-lifecycle, and is unnecessary once the observer is a borrow — by
  [[decisions/260430_typed-state-flow-wrappers|typed-state-flow-wrappers]]'s
  restraint rule, don't add a wrapper that prevents no remaining mistake.
- **Set `surface = None` on detached workers in `spawn_thread`, leave the frame
  in the hosts.** This is the one-line fallback above; it fixes the bug by
  convention in one spot but keeps both hosts hand-rolling frames. Acceptable as
  an interim, not the target.
- **Lower `detached_ceiling` below the host wall.** Rejected: orthogonal, fights a
  prior decision, and still leaves the turn *waiting* on a worker. The turn must
  not wait at all.

## Scope and honesty about cost

This is a real refactor of one seam, not a keystroke: a host-API surface in
`core/src/host.rs`, `run_turn` absorbing the frame/reaper/surface construction
from *both* hosts, and the borrowed-surface lifetime change in `TurnState` /
`spawn_thread`. It is bounded — the per-turn eval path — and it pays for itself
by deleting two hand-rolled frames and finishing an already-active decision. Two
honest costs: it spends
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]'s
`Value`-non-leak on this path (a deliberate trade for one common API), and the
`ral` batch path (`ral/src/main.rs`, still calling `eval_top_level` directly — the
gap 260616 also flagged) is a natural third `run_turn` client but is left out of
this ADR's scope.

See also
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] (the
unification this completes),
[[decisions/260610_host-embedding-api|host-embedding-api]] (the seam this
extends),
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
(the data-boundary rule this narrows),
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(the "detachment holds no foreground capture state" invariant, now
lifetime-enforced),
[[decisions/260617_watch-repl-builtin|watch-repl-builtin]] (the REPL's durable
streaming, kept separate),
[[internals/output-capture-and-detachment|output-capture-and-detachment]] (the
data-pipe story this completes for the observer), [[map/exarch|map: exarch]],
[[map/repl/loop|repl/loop]].
