---
status: proposed
supersedes: decisions/260618_host-seam-turn-observer
---

# A turn is a synchronous call; the host owns the loop, and completion is the call returning — not the channel closing

**Core exposes one synchronous, runtime-agnostic entry —
`Shell::run_turn(src, &TurnRequest, &dyn EventSink) -> TurnReport` — and each host
drives it however it likes. exarch runs the turn on a `spawn_blocking` task inside
a `tokio::select!` loop whose completion arm is the *turn task's join future*, not
the event channel's disconnect; the REPL calls `run_turn` straight on its prompt
thread.** The daemon-task hang dies because turn completion becomes a control-flow
fact — the call returned — that a detached worker physically cannot influence;
there is no shared liveness object left between detachment and turn exit. tokio
never enters `ral_core`: the only seam is `EventSink`, a synchronous trait that
takes a `Value`. The `surface` builtin is unchanged; only its *carrier* moves
from a stored, cloned `Send` closure on `Shell` to the turn-scoped borrowed sink.
This completes
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] (one entry, all
hosts are request suppliers) by *removing a thread*, not by adding a type.

## Context

A Terminal-Bench run (`exarch-bench/.../2026-06-18__13-40-33`) hangs on the two
daemon tasks `kv-store-grpc` and `pypi-server`: both score reward `1.0`, the model
finishes in ~80 s, then exarch idles ~13.5 min until Harbor SIGKILLs it at the
900 s wall. `result.json` is 0 bytes; the session log ends on the model's final
no-tool-call message with **no `session_ended`** — the work was done, exarch could
not return.

[[decisions/260618_host-seam-turn-observer|host-seam-turn-observer]] diagnosed the
mechanism exactly and that diagnosis stands: exarch's `Sink::drive`
(`exarch/src/bus.rs`) loops `while let Ok(ev) = rx.recv()` and returns only on
channel **disconnect** — when the last `Sender<Event>` drops. The structured
`SurfaceSink` (`core/src/types/shell/mod.rs`, an `Arc<dyn Fn(Value)+Send+Sync>`)
captures a clone of the turn's `Emitter`, hence a clone of that `Sender`, and is
stored on `TurnState` and `.clone()`d into every detached `std::thread::spawn`
worker (`core/src/types/shell/inherit.rs:205,:214`). A server worker never
terminates → its `Sender` clone never drops → the channel never disconnects →
`drive` never returns → `pump` never returns → `run_turn` never returns →
`result.json`, printed only after the loop, is never written.

This is **endemic to an agent**, not a fluke. Agents spawn long-lived background
work — servers, watches, daemons — as a matter of course. Any architecture in
which turn completion depends on *every* event sender having dropped will hang the
moment the agent does the thing agents are for. The completion signal is the bug,
not the surface.

### Why this supersedes host-seam-turn-observer

That ADR kept the channel-as-completion loop and made the surface impossible to
move into a detached worker by recasting `SurfaceSink` to `Rc<dyn Fn(Value)>`
(`!Send`). The leak becomes a compile error — but at a cost it under-weighed.
`Rc` makes `Shell` `!Send`, and exarch's driver moves the whole `Session` (which
owns the `Shell`) into `pump`'s `Send`-bounded scoped worker:

```rust
// exarch/src/session.rs:421
let s: &mut Session = &mut *self;
match pump(sink, id, move |emit| s.apply(provider, p, &token, emit)) { … }
//        ^ pump requires `work: Send`; the closure captures &mut Session,
//          so Session: Send, so Shell: Send. `Rc` breaks this line.
```

So the `!Send` recast does not compile against exarch's existing threading; it
forces the surrounding refactor (frame construction migrated into core, the
`Value`-non-leak of
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
spent, `Shell` reworked) just to make the new bound tenable. And it guards the
wrong axis: the foreground eval *legitimately* crosses one thread boundary (onto
the pump worker); the illegitimate crossing is the nested **detached** one, which
differs by `'static`, not by `Send`. `!Send` bans both, which is why it collides
with the pump and why it *reduces* exarch's concurrency by pinning eval to the
driver thread.

The cheaper move is to delete the fragile loop. Once turn completion is "the call
returned," the surface's thread-discipline stops mattering: a detached worker may
hold a sender clone forever and it changes nothing, because nothing waits on the
channel to decide the turn is over. `Shell` stays `Send`, the pump and its
`drive`/`Emitter`-as-transport machinery are *deleted* rather than worked around,
and the genuinely good part of the prior ADR — one `run_turn` both hosts share —
survives intact.

## Decision

Five parts.

### 1. One synchronous, runtime-agnostic entry in core

Add to the host-embedding seam
([[decisions/260610_host-embedding-api|host-embedding-api]]), in
`core/src/host.rs`:

```rust
impl Shell {
    pub fn run_turn(&mut self, src: &str, req: &TurnRequest, sink: &dyn EventSink)
        -> TurnReport;
}

pub struct TurnRequest<'a> {
    pub script_name: &'a str,             // "<stdin>" (REPL) | "<tool>" (exarch)
    pub caps: Capabilities,               // root() (REPL) | grant profile (exarch)
    pub turn_limit: Option<Duration>,     // None (REPL) | Some(30s) (exarch)
    pub detached_limit: Option<Duration>, // None (REPL) | Some(1h) (exarch)
    pub printer: Option<Arc<dyn ExternalWrite>>, // Some = live bytes (REPL);
                                          // None = capture, returned in `Ran`
}

/// The structured-event surface. A *synchronous* trait taking a raw `Value`;
/// core's `surface` builtin emits to "the current turn's sink." Core names no
/// runtime type — the host decides whether `emit` prints, drops, or crosses a
/// channel.
pub trait EventSink {
    fn emit(&self, ev: &Value) {}
}

/// One flat result the host matches once. `captured`/`timed_out` live on `Ran`,
/// where they mean something — a `Static` turn never ran.
pub enum TurnReport {
    Static { diagnostics: StaticDiagnostics },   // parse/type failure; status 1
    Ran {
        result: Settled<Value>,    // Ok | error | exit N | stopped
        status: i32,
        single_command: bool,
        captured: Option<Captured>, // Some when `printer == None`
        timed_out: bool,
    },
}
pub struct Captured { pub stdout: Vec<u8>, pub stderr: Vec<u8> }
```

`run_turn` builds the `IoFrame` (the `printer` becomes `Sink::External` when
`Some`, else core mints `Sink::Buffer` captures), installs `sink` as the turn's
surface, arms `turn_limit`/`detached_limit` on `process::reaper`, calls the
**internal** `eval_turn`, and flattens its `TurnOutcome` into a `TurnReport`,
folding in the captured bytes and `timed_out` it alone knows. `TurnOutcome`
(`eval_turn`'s `Static | Runtime`) stays internal; `Settled<Value>` is reused, not
re-spelled. Everything `run_shell` and `execute_input` do today to assemble a
frame moves here once.

Two facets, two disciplines, by *where each lives*, with no type gymnastics:

- **Bytes** are request currency (`printer: Arc<dyn ExternalWrite>`, already
  `Send + Sync`, `core/src/io/sink.rs:34`). They may legitimately detach — that is
  what `watch` does — so they stay `Send`.
- **The surface** is a *borrowed* turn sink (`&dyn EventSink`), never stored on the
  persistent `Shell`, never detached. It is a dynamically-scoped handler installed
  for the extent of the call — which is exactly what it always was, minus the
  clone-into-workers.

### 2. Completion is the call returning; the host owns the loop

exarch already runs a tokio multi-thread runtime and already drives provider calls
with `tokio::select!` and `spawn_blocking` (`exarch/src/provider.rs:591,621`,
`oauth/browser.rs:28`). The turn loop *joins that world* rather than introducing
it. The CPU-bound, synchronous `eval_turn` runs on a blocking task; the reactive
UI is a `select!` loop:

```rust
// exarch, one turn:
let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();   // tx is a plain Send sender
let mut eval = tokio::task::spawn_blocking(move || shell.run_turn(src, &req, &Sink(tx)));
let mut frame = tokio::time::interval(Duration::from_millis(33)); // ~30 fps
loop {
    tokio::select! {
        Some(ev) = rx.recv()    => ui.apply(ev),     // reactive: render as events arrive
        _        = frame.tick() => ui.animate(),       // reactive: spinner/elapsed each frame
        _        = cancel.cancelled() =>               // Esc / Ctrl-C / timeout, one arm
                      process::request_foreground_cancel(CancelCause::Interrupt),
        report   = &mut eval    => {                   // COMPLETION: the *task* resolved
            while let Ok(ev) = rx.try_recv() { ui.apply(ev); } // drain the tail
            break report.unwrap();
        }
    }
}
```

The loop exits on `report = &mut eval` — the eval task's join future resolving.
A detached worker holding a clone of `tx` has **zero** bearing on whether that
future resolves: `kv-store-grpc` may hold a sender forever, the turn still returns
the instant eval finishes. The `while let Ok(ev) = rx.recv()` "all senders dropped
= done" semantics is **deleted**. Headless drives the same loop with a UI impl
that does not animate (or simply blocks on the join and drains `rx`).

### 3. The surface carrier becomes turn-scoped, not stored-and-cloned

Replace the `surface: Option<SurfaceSink>` field on `TurnState`
(`core/src/types/shell/mod.rs:167`) — and `set_surface` (`host.rs:76`), the child
inherit-clone (`mod.rs:192`), and the detached copy (`inherit.rs:205,:214`) — with
the borrowed `&dyn EventSink` threaded through the turn install. The `surface`
builtin (`core/src/builtins/misc.rs:530`) emits to the current turn's sink. A
same-thread child sees it (same turn, same borrow); a detached worker does **not**
get the live sink — it gets a worker-local buffer for deferred `await`-replay (a
`surface_buf` beside `stdout_buf`, the §13.3 byte-replay rule extended to
structured events), or nothing if it is never awaited, exactly as its bytes are
dropped if never awaited.

### 4. tokio stops at exarch's `EventSink` impl

`ral_core` names no `tokio`, `spawn_blocking`, `mpsc`, `select`, or `Send`-on-a-
surface. exarch's sink wraps the channel:

```rust
struct AgentSink(tokio::sync::mpsc::UnboundedSender<Kind>);
impl EventSink for AgentSink {
    fn emit(&self, ev: &Value) {
        if let Some(kind) = value_to_kind(ev) { let _ = self.0.send(kind); } // non-blocking
    }
}
```

`value_to_kind` and the `` `patch ``/`` `wrote ``/`` `task ``/`` `meter `` vocabulary
stay *in exarch*. The REPL's sink prints, or is the default no-op — surfacing is
exarch's, not the REPL's — and the REPL never touches the async loop: it calls
`run_turn` straight on its prompt thread.

### 5. One ordered event stream; `Value` is the only core thing that crosses

What travels the seam is the host's own currency — `Capabilities`, `Duration`s,
`Arc<dyn ExternalWrite>`, the `TurnReport` — the opaque `&mut Shell` handle, and,
by the same concession the `surface` builtin already makes today, raw `Value`.
Core learns no rail taxonomy. Nothing else of core's representation crosses: no
`TurnFrame`, `IoFrame`, `Sink`, `Source`, `arm_lifetime`, or `eval_turn` is named
by any host.

## Why one API covers everything

Walking the policy axes, each is a request field or orthogonal:

- **IO regime** → `printer`: `Some` = live bytes (REPL), `None` = capture (exarch,
  bytes returned in `Ran`'s `captured`). The `IoFrame::Inherit | Capture` sum
  collapses into the request; no `IoMode` flag is needed.
- **Structured surface** → the borrowed `&dyn EventSink`; the same trait the
  REPL no-ops and exarch routes to its rail.
- **Cancellation** → one `select!` arm on the existing per-root-turn token
  ([[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]]); Esc / Ctrl-C /
  deadline all land there. No host names a `CancelScope`.
- **Capabilities / limits / script name** → request fields.
- **`watch`** → untouched (part 1): a REPL-only builtin over `Send` byte sinks
  ([[decisions/260617_watch-repl-builtin|watch-repl-builtin]]).
- **Outcome classification** → computed once in core, flattened to `TurnReport`;
  hosts only render.
- **Reactivity** → a host concern, in the host's loop (the frame timer), not a
  core type.

This is also where exarch's already-proposed direction lands: the idle wait in
[[decisions/260617_scheduled-wakeups|scheduled-wakeups]] is *already* "a select
over `{input, inbox}`." The turn loop is the same shape one level in — the agent
*is* an event multiplexer, and `select!` is its natural spine.

## Implementation plan

### core — `host.rs`, `turn.rs`, `types/shell/mod.rs`, `inherit.rs`, `builtins/misc.rs`, `builtins/concurrency.rs`

1. Define `EventSink`, `TurnRequest`, the flat `TurnReport`, and `Captured` in
   `host.rs`. `emit(&Value)` carries raw `Value`; core defines no taxonomy. Keep
   `TurnOutcome` as `eval_turn`'s internal return.
2. Add `Shell::run_turn`: build the `IoFrame` (`printer` → `Sink::External`, else
   mint `Sink::Buffer` + `Source::Terminal`), install the borrowed `sink` as the
   turn surface, arm `turn_limit`/`detached_limit` on `process::reaper`, call
   `eval_turn`, read `timed_out` from the foreground scope's
   `CancelCause::Deadline`, flatten into `TurnReport`. The limit arm/disarm dance
   leaves `shell_eval.rs`.
3. Replace the stored `SurfaceSink`: thread `&dyn EventSink` through the turn
   install; the `surface` builtin emits to it. Delete `set_surface`, the surface
   clone in `mod.rs:192`, and the detached copy in `inherit.rs:205,:214` — the
   worker's live surface is gone.
4. In `spawn_child` (`builtins/concurrency.rs`), allocate a
   `surface_buf: Arc<Mutex<Vec<Value>>>` beside the byte buffers; the worker
   appends `Value`s to it; the `await` drain replays them to the caller's surface,
   alongside the existing `stdout_buf`/`stderr_buf` replay.
5. Confirm `ral_core` still names no runtime type (a grep guard in CI).

### exarch — delete the bespoke pump; the loop becomes `select!`

`bus.rs`'s `pump` and the channel-as-completion default `drive` are deleted;
`Emitter` stops being turn transport. `session.rs:run_turn` drives the `select!`
loop of part 2 with `spawn_blocking`. `shell_eval.rs` collapses to an adapter:
build `TurnRequest { script_name: "<tool>", turn_limit: Some(30s), detached_limit:
Some(1h), printer: None, .. }`, supply an `AgentSink` (owns `value_to_kind`),
match `TurnReport` into the existing `Outcome`/`ToolResult` (cap `captured` bytes,
synthesise the 124 message from `timed_out`, append sandbox denials, render the
`Value`). `headless.rs` keeps the same driver with a non-animating UI. exarch's
`DETACHED_WORKER_CEILING` and 30 s wall become request fields.

### ral REPL — `repl/exec.rs` collapses to an adapter

`execute_input` builds `TurnRequest { script_name: "<stdin>", caps: root(),
turn_limit: None, detached_limit: None, printer: Some(rustyline_printer) }`,
supplies a print/no-op `EventSink`, calls `run_turn` **synchronously**, and
matches `TurnReport` as today (`print_result`, exit, `Escape::Stopped` → job,
`Break::Error` → ariadne). The hand-built `TurnFrame` (`exec.rs:102-109`) is
deleted; the REPL only ever sees `Static` or `Ran { captured: None, .. }`. No
async runtime is involved.

### ral batch — `main.rs` becomes the third `run_turn` client

`ral/src/main.rs:480` still calls `eval_top_level` directly (the gap
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] flagged). Fold it
in: a capturing/no-op `EventSink`, `printer` per mode, synchronous `run_turn`.

## Test plan

- **The instance:** a stub turn whose script is `spawn { <blocks forever> }` then a
  no-tool-call message; assert the driver returns within a tight bound, a
  `TurnReport::Ran` is produced, and (through exarch's `Session`) `session_ended`
  is recorded. Name in the concurrency ADR's spirit:
  `detached_worker_cannot_outlive_turn_completion`.
- **Completion-not-disconnect:** drive the `select!` loop with a fake detached
  holder that keeps a `tx` clone alive; assert the loop returns on the join arm
  while the sender still lives.
- **`watch` regression:** a watched worker still streams live after the turn
  returns — bytes are `Send` and detachment of *bytes* is intentional.
- **Deferred surface:** `spawn { edit … }` then `await $h` replays the `` `patch ``
  card; an un-awaited worker emits no card; the file is written either way.
- **Parity (the unification):** the same source through `run_turn` with
  `printer: Some` (REPL shape) and `printer: None` (exarch shape) on one `Shell`;
  assert the `TurnReport` classification agrees.
- **Seam / tokio boundary:** grep tests that `ral_core` names no
  `tokio`/`spawn_blocking`/`mpsc`/`select`, and that no host names `TurnFrame`,
  `IoFrame`, `Sink`, `Source`, `arm_lifetime`, or `eval_turn` — the
  [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
  grep, narrowed to allow `Value`.
- **End-to-end:** re-run `kv-store-grpc` and `pypi-server`; assert no
  `AgentTimeoutError`, non-empty `result.json`, reward still `1.0`.

## Consequences

- The hang is impossible by construction: turn completion is the call returning /
  the join future resolving — a control-flow fact no background thread can change.
  There is no shared liveness object between detachment and turn exit. To
  reintroduce the class a maintainer would have to write a `select!` with no
  completion arm, a visibly wrong loop, not copy one innocuous line.
- exarch becomes genuinely reactive and async: one `select!` loop per turn, a
  frame-timer animation that looks alive between events, and a single cancel arm —
  built on the tokio runtime exarch already runs. Headless and TUI share the loop.
- [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] is completed:
  one `run_turn`, three hosts (REPL, exarch, batch) as request suppliers;
  `shell_eval.rs` and `exec.rs` shrink to adapters; `IoMode` never needs to exist.
- `Shell` stays `Send`/`Sync`: no `Rc`, no lifetime infection of `TurnState`, and
  the `pump`/`Session` collision the prior ADR hit (`session.rs:421`) is moot —
  `pump` is deleted.
- tokio stays out of `ral_core`: the seam is a synchronous `EventSink` taking
  `Value`. The host owns its concurrency model; core is a synchronous evaluator
  with one runtime-agnostic turn entry — strengthening, not bending,
  [[decisions/260610_host-embedding-api|host-embedding-api]].
- The `surface` builtin is unchanged language-side; only its carrier moved from
  stored-and-cloned to turn-scoped. `internals/output-capture-and-detachment` needs
  a one-line correction: the surface is now turn-local, not a `Shell` field cloned
  into workers.
- [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
  is *recorded as-is*, not freshly spent: the `surface` builtin already emits
  `Value` today, so `Value` already crosses this path; the rail vocabulary lives in
  exarch's `value_to_kind`, and core stays generic.
- exarch deletes more than it adds: `pump`, the channel-as-completion `drive`, and
  `Emitter`-as-transport are gone.
- The 1 h `detached_limit` is unchanged as a backstop, no longer load-bearing for
  turn exit. Do **not** lower it to mask anything.

## Alternatives considered

- **The `!Send` surface of
  [[decisions/260618_host-seam-turn-observer|host-seam-turn-observer]]
  (superseded).** Hardens the surface so it cannot move into a detached worker —
  but it makes `Shell` `!Send`, which fails to compile against exarch's
  `pump`/`Session` move (`session.rs:421`) and so drags in frame migration and the
  `Value`-non-leak spend; and it guards thread-confinement when the bug lives on
  *detachment* (`'static`), reducing concurrency by pinning eval to the driver
  thread. Deleting the loop is cheaper and keeps `Shell` `Send`.
- **Minimal point-fix: `surface = None` in `spawn_thread`, keep `Arc`.** Fixes the
  instance in ~1 line and keeps the codebase consistent with
  [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]],
  but leaves the channel-as-completion loop and its silent-footgun shape. A fine
  interim; subsumed here.
- **`Drive::Done` sentinel through the existing channel.** The synchronous
  analogue of completion-not-disconnect — the foreground worker emits a terminator
  and `drive` stops on it. Works, but preserves the bespoke `pump`/`drive` instead
  of the reactive `select!` loop an agent wants.
- **Synchronous collapse — eval and render on one thread, no channel at all.**
  Simplest, and it kills the bug outright, but it gives up live animation during a
  single blocking tool call. The reactive UI is wanted, so the async loop stays.
- **Make `eval_turn` itself `async`.** Rejected: the evaluator is CPU-bound
  tree-walking; on the reactor it would starve the very UI we want reactive, and
  coloring the whole evaluator async is a large rewrite for negative benefit. Async
  at the edges, synchronous in the core, bridged by `spawn_blocking` — which is
  what async is *for*.
- **A core `TurnEvent` taxonomy decoded by core.** Rejected: it puts exarch's rail
  vocabulary in core. Raw `Value` to `emit` keeps core generic and matches the
  existing `surface` builtin.

## Scope and honesty about cost

This is a real refactor of exarch's event loop — `pump` and the
channel-as-completion `drive` are deleted and replaced by the `select!` driver —
plus the core `run_turn`/`EventSink`/`TurnReport` surface absorbing frame
construction from all three hosts. The surprising part, again, is how much of the
fix is *deletion*: the `Emitter`-as-transport machinery goes, and `Shell` stays
`Send`. Honest costs, none fatal:

- **Blocking-pool pressure.** Each concurrent turn (parallel tool dispatch,
  sub-agents) consumes a `spawn_blocking` thread. exarch's multi-thread runtime
  already sizes a blocking pool generously, but deeply nested sub-agents warrant a
  bound and a test.
- **Backpressure.** Use a bounded channel and *coalesce* surface events under load
  (the latest meter wins) so a fast emitter cannot outrun the renderer; a
  non-blocking `try_send` from the sync sink drops intermediate frames, which is
  correct for a progress surface.
- **Cooperative cancellation must still reach the sync eval promptly** — it does,
  through the existing cancel scopes plus `EINTR` on blocking syscalls
  ([[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]]).
- **One ordered stream.** exarch captures bytes today (nothing echoes live), so
  byte/surface interleaving is moot now; a future TUI that streams bytes live
  should route both through one `Event` enum to keep on-screen order.

See also
[[decisions/260618_host-seam-turn-observer|host-seam-turn-observer]] (the analysis
this supersedes),
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] (the unification
this completes),
[[decisions/260610_host-embedding-api|host-embedding-api]] (the seam this extends),
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
(the data-boundary rule, recorded not respent),
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(the "detachment holds no foreground capture state" invariant, now moot because
nothing waits on the channel),
[[decisions/260617_watch-repl-builtin|watch-repl-builtin]] (the byte streaming kept
intact),
[[decisions/260617_scheduled-wakeups|scheduled-wakeups]] (the select-over-inbox
direction this turn loop matches),
[[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]] (the cancel token
the loop's cancel arm reads),
[[internals/output-capture-and-detachment|output-capture-and-detachment]] (the
data-pipe story this completes for the surface), [[map/exarch|map: exarch]],
[[map/repl/loop|repl/loop]].
