---
status: active
supersedes: decisions/260618_host-seam-turn-observer
---

# A turn is a synchronous call; the host owns the loop, and completion is the call returning — not the channel closing

**Core exposes one synchronous, runtime-agnostic entry —
`Shell::run_turn(src, TurnRequest) -> TurnReport` — and each host drives it
however it likes. The request carries the turn policy (`script_name`,
capabilities, limits, byte IO, lifecycle hooks) and a turn-local
`SurfaceSink = Arc<dyn EventSink + Send + Sync>`. exarch runs the call on a
worker and multiplexes UI events with an explicit completion future — a scoped
worker plus one-shot while `Session` and `Provider` are borrowed, or
`spawn_blocking` once the driver owns the moved state — never with event-channel
disconnect as the done signal. The REPL calls `run_turn` straight on its prompt
thread.** The daemon-task hang dies because turn completion becomes a
control-flow fact — the call returned, or the worker sent its done value — that a
detached worker physically cannot influence. tokio never enters `ral_core`: the
only seam is `EventSink`, a synchronous trait that takes a `Value`. The
`surface` builtin is unchanged; only its *carrier* moves from a persistent,
clone-into-workers closure to a turn frame that same-thread children inherit and
detached workers replace with bounded deferred storage. This completes
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] (one entry, all
hosts are request suppliers) by making completion explicit, not by adding an
observer.

## As built

The **invariant** of this ADR shipped exactly — completion is an explicit
control-flow fact, never event-channel disconnect — but through a *different
mechanism* than the `tokio::select!` + `oneshot` driver sketched in Decision
part 2. The `select!`/`oneshot` code below, and the `AgentSink`/`blocking_send`
of part 4, are the **proposal shape**, not the code. What shipped:

- **Completion is a worker-set `AtomicBool`, polled — not a `oneshot` future.**
  `pump` (`exarch/src/bus.rs`) declares `done: AtomicBool`, runs the turn on a
  `std::thread::scope` worker that borrows `&mut Session`/`&Provider`, and the
  worker `store`s `done = true` after `work` returns or unwinds (`bus.rs:366`).
  `Sink::drive` (`bus.rs:303`) loops `recv_timeout(DRAIN_POLL)` and returns when
  `done` is set (disconnect is only a safety net); the TUI overrides `drive`
  with `drive_events` (`tui.rs:2432`), which polls `done` at ~60 fps between
  key/render work. `pump` and `drive` are **retained and re-pointed**, not
  deleted.
- **No tokio in the turn loop at all.** The event channel stays `std::sync::mpsc`
  (unbounded); tokio remains only in `provider.rs` for LLM transport. This
  *strengthens* this ADR's own "tokio out of core" goal — the runtime never even
  enters the host's turn loop, let alone core.

Why the mechanism was not adopted as literally sketched — established when the
`select!` driver was actually attempted:

- **The `oneshot` buys no correctness the flag lacks.** Both make completion a
  control-flow fact a detached worker cannot influence; the daemon hang is dead
  identically. The flag is the load-bearing decision in a smaller encoding.
- **The async loop needs a second crossterm input owner.** A `select!` input arm
  means a reader thread or `EventStream` alongside the synchronous idle/picker
  reads — a new concurrency surface (and a keystroke-loss boundary race unless
  *all* input is rerouted through one owner) that the single-threaded synchronous
  `drive_events` avoids by construction.
- **`blocking_send` would panic.** Token events are emitted from a callback that
  fires *inside* the provider's `runtime.block_on` (`session.rs:273`), i.e. in a
  runtime context, where `tokio::sync::mpsc::Sender::blocking_send` panics. The
  sound async equivalent is an *unbounded* `UnboundedSender::send` — which means
  the **bounded backpressure / meter-phase coalescing of part 4 is unreachable**
  without restructuring provider streaming. The proposal's headline reliability
  feature does not survive contact with the streaming path.

Net: the `select!` rewrite's only reachable wins over the as-built are literal
ADR fidelity and a modest TUI/headless driver DRY — neither a correctness gain,
paid for with tokio plumbing and a second input owner. The flag-based design is
kept. The Decision/Implementation sections below are preserved as the proposal
and its reasoning; read them against this note.

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
channel to decide the turn is over. `Shell` stays `Send`, the pump/`drive`
completion contract is deleted rather than worked around, and the genuinely good
part of the prior ADR — one `run_turn` both hosts share — survives intact.

## Decision

Five parts.

### 1. One synchronous, runtime-agnostic entry in core

Add to the host-embedding seam
([[decisions/260610_host-embedding-api|host-embedding-api]]), in
`core/src/host.rs`:

```rust
pub type SurfaceSink = Arc<dyn EventSink>;

impl Shell {
    pub fn run_turn(&mut self, src: &str, req: TurnRequest<'_>) -> TurnReport;
}

pub struct TurnRequest<'a> {
    pub script_name: &'a str,             // "<stdin>" (REPL) | "<tool>" (exarch)
    pub caps: Capabilities,               // root() (REPL) | grant profile (exarch)
    pub turn_limit: Option<Duration>,     // None (REPL) | Some(30s) (exarch)
    pub detached_limit: Option<Duration>, // None (REPL) | Some(1h) (exarch)
    pub io: TurnIo,                       // Inherit (REPL/batch) | Capture (exarch)
    pub surface: Option<SurfaceSink>,     // no-op when absent
    pub lifecycle: Box<dyn TurnLifecycle + 'a>,
}

pub enum TurnIo {
    /// Clone the session's ambient streams. The REPL has already installed
    /// its rustyline `ExternalPrinter` with `set_stdout`; batch inherits the
    /// process streams.
    Inherit,
    /// Core mints stdout/stderr buffers and returns them in `TurnReport::Ran`.
    /// Stdin falls through to the terminal/source the shell already holds.
    Capture,
}

/// The structured-event surface. A *synchronous* trait taking a raw `Value`;
/// core's `surface` builtin emits to "the current turn's sink." Core names no
/// runtime type — the host decides whether `emit` prints, blocks, coalesces, or
/// crosses a channel.
pub trait EventSink: Send + Sync {
    fn emit(&self, ev: &Value);
}

impl EventSink for () {
    fn emit(&self, _ev: &Value) {}
}

/// One flat result the host matches once. `captured`/`timed_out` live on `Ran`,
/// where they mean something — a `Static` turn never ran.
pub enum TurnReport {
    Static { diagnostics: StaticDiagnostics },   // parse/type failure; status 1
    Ran {
        result: Settled<Value>,    // Ok | error | exit N | stopped
        status: i32,
        single_command: bool,
        captured: Option<Captured>, // Some when `io == TurnIo::Capture`
        timed_out: bool,
    },
}
pub struct Captured { pub stdout: Vec<u8>, pub stderr: Vec<u8> }
```

`run_turn` translates the public request into the **internal** `TurnFrame` /
`IoFrame`: `Inherit` clones the ambient streams already installed on the
`Shell`, while `Capture` mints `Sink::Buffer` captures. It installs
`surface` only in the turn frame, arms `turn_limit`/`detached_limit` on
`process::reaper`, calls the internal `eval_turn`, and flattens its
`TurnOutcome` into a `TurnReport`, folding in the captured bytes and
`timed_out` it alone knows. `TurnOutcome` (`eval_turn`'s `Static | Runtime`)
stays internal; `Settled<Value>` is reused, not re-spelled. Everything
`run_shell`, `execute_input`, and `main.rs` do today to assemble frames moves
behind this one method once.

Two facets, two disciplines, by *where each lives*, with no lifetime gymnastics:

- **Bytes** are IO state. They may legitimately detach — that is what `watch`
  does — so the live byte sinks stay `Send + Sync` and cloneable.
- **The surface** is a turn-local `Arc<dyn EventSink + Send + Sync>`, not a
  persistent `Shell` capability. It has no borrowed lifetime to infect
  `TurnState`, so `Shell` stays `Send`; it has no liveness role, so a clone can
  never define turn completion. Same-thread children may clone it. Detached
  workers do not receive it.

### 2. Completion is the call returning; the host owns the loop

exarch already owns a tokio runtime and already drives provider calls with
`tokio::select!` (`exarch/src/provider.rs:591,621`). The turn loop joins that
world at the host boundary, but it must preserve the current borrow shape:
`Session::run_turn` lends `&mut Session` and `&Provider` to the worker today, so
the first implementation uses a scoped blocking worker plus a one-shot
completion future. If the driver later moves owned state into the worker, the
same loop can replace that carrier with `spawn_blocking`; the invariant is the
same either way: **completion is an explicit done future, not bus disconnect**.

```rust
// exarch, one turn:
let (tx, mut rx) = tokio::sync::mpsc::channel(EVENT_BACKLOG);
let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();

std::thread::scope(|scope| {
    scope.spawn(|| {
        let report = catch_unwind_as_report(|| {
            shell.run_turn(src, TurnRequest {
                io: TurnIo::Capture,
                surface: Some(Arc::new(AgentSink(tx.clone()))),
                ..req
            })
        });
        let _ = done_tx.send(report);
    });

    runtime.block_on(async {
        let mut frame = tokio::time::interval(Duration::from_millis(33)); // ~30 fps
        loop {
            tokio::select! {
                Some(ev) = rx.recv()    => ui.apply(ev),      // reactive: render events
                _        = frame.tick() => ui.animate(),      // reactive: spinner/elapsed
                _        = cancel.cancelled() =>              // Esc / Ctrl-C / timeout
                              process::request_foreground_cancel(CancelCause::Interrupt),
                report   = &mut done_rx => {                  // COMPLETION: explicit done
                    while let Ok(ev) = rx.try_recv() { ui.apply(ev); }
                    break report.unwrap_or_else(worker_panic_report);
                }
            }
        }
    })
})
```

The loop exits on `done_rx` resolving — or, in an owned-state rewrite, the
blocking task's join future resolving. A detached worker holding a clone of `tx`
has **zero** bearing on whether that future resolves: `kv-store-grpc` may hold a
sender forever, the turn still returns the instant eval finishes. The
`while let Ok(ev) = rx.recv()` "all senders dropped = done" semantics is
**deleted**. Headless drives the same loop without a frame timer; it must still
drain events while waiting for done, because semantic cards use backpressure.

### 3. The surface carrier becomes turn-scoped, not stored-and-cloned

Keep a `surface: Option<SurfaceSink>` slot on `TurnState`
(`core/src/types/shell/mod.rs:167`), but change what it means. The slot is
installed only by `run_turn`; `set_surface` (`host.rs:76`) goes away, and no host
stores a surface on the persistent session. `TurnState::inherit_from`
(`mod.rs:190-192`) still clones the sink for same-thread child evaluation: it is
the same turn, so the event handler is still live. The detached path
(`inherit.rs:205,:214`) stops copying the live sink. A detached worker instead
gets a bounded `surface_buf` beside `stdout_buf`/`stderr_buf`; the first
`await` on the handle drains that buffer through the caller's *current* surface
and marks it replayed. `poll` observes completion without replaying, repeated
`await` does not duplicate cards, and an un-awaited worker emits no cards. Bytes
remain returned values (`stdout`/`stderr`); structured surface remains a
once-only host effect.

### 4. tokio stops at exarch's `EventSink` impl

`ral_core` names no `tokio`, `spawn_blocking`, `mpsc`, `select`, or host rail
taxonomy. exarch's sink wraps the channel and decides reliability after decoding
the `Value`:

```rust
struct AgentSink(tokio::sync::mpsc::Sender<Kind>);
impl EventSink for AgentSink {
    fn emit(&self, ev: &Value) {
        let Some(kind) = value_to_kind(ev) else { return; };
        match kind {
            Kind::Meter { .. } => coalesce_meter(&self.0, kind), // progress: latest wins
            Kind::Phase(_) => coalesce_phase(&self.0, kind),     // progress: latest wins
            _ => { let _ = self.0.blocking_send(kind); }         // semantic: lossless
        }
    }
}
```

`value_to_kind` and the `` `patch ``/`` `wrote ``/`` `task ``/`` `meter ``
vocabulary stay *in exarch*. Patch/write/task cards are semantic events and are
not dropped; meters and phases are progress events and may coalesce under load.
The REPL's sink prints or no-ops, and the REPL never touches the async loop: it
calls `run_turn` straight on its prompt thread.

### 5. One ordered event stream; `Value` is the only core thing that crosses

What travels the seam is the host's own currency — `Capabilities`, `Duration`s,
`TurnIo`, `SurfaceSink`, `TurnLifecycle`, the `TurnReport` — the opaque
`&mut Shell` handle, and, by the same concession the `surface` builtin already
makes today, raw `Value`. Core learns no rail taxonomy. Hosts no longer name
`TurnFrame`, `IoFrame`, `arm_lifetime`, or `eval_turn`; ordinary byte sinks remain
available through the existing host accessors for ambient REPL setup.

## Why one API covers everything

Walking the policy axes, each is a request field or orthogonal:

- **IO regime** → `TurnIo`: `Inherit` = ambient bytes (REPL/batch; the
  rustyline printer is already installed on the `Shell`), `Capture` = core-minted
  buffers returned in `Ran`'s `captured`. No `printer` flag is needed.
- **Structured surface** → `SurfaceSink`: a turn-local
  `Arc<dyn EventSink + Send + Sync>`; the same trait the REPL no-ops and exarch
  routes to its rail.
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

1. Define `EventSink`, `SurfaceSink`, `TurnIo`, `TurnRequest`, the flat
   `TurnReport`, and `Captured` in `host.rs`. `emit(&Value)` carries raw
   `Value`; core defines no taxonomy. The companion API cutover deletes core
   `TurnOutcome` rather than keeping it as an internal mirror.
2. Add `Shell::run_turn`: materialise `TurnIo::Inherit | Capture` into private
   `Io`/`CaptureBuffers`, install `surface` on the new `TurnState`, arm
   `turn_limit`/`detached_limit` on `process::reaper`, evaluate the turn, read
   `timed_out` from the foreground scope's `CancelCause::Deadline`, flatten into
   `TurnReport`, and drain captures when present. The limit arm/disarm dance
   leaves `shell_eval.rs`.
3. Recast the stored `SurfaceSink`: it remains turn state, but becomes
   `Arc<dyn EventSink + Send + Sync>` installed only by `run_turn`. Delete
   `set_surface`; keep same-thread inheritance in `mod.rs:192`; delete the live
   detached copy in `inherit.rs:205,:214`.
4. In `spawn_child` (`builtins/concurrency.rs`), allocate a bounded
   `surface_buf` beside the byte buffers. Drain it into `CompletedHandle`, replay
   it once on `await` through the caller's current surface, and make `poll`
   non-replaying. The overflow marker is itself one surface event, so a runaway
   detached meter cannot allocate without bound.
5. Confirm `ral_core` still names no runtime type (a grep guard in CI).

### exarch — delete the bespoke pump; the loop becomes `select!`

`bus.rs`'s `pump` and the channel-as-completion default `drive` are deleted;
`Event`/`Kind` and the emitter API remain exarch's presentation bus, but no bus
sender is a completion token. `session.rs:run_turn` drives the `select!` loop of
part 2 with a scoped worker + one-shot first; `spawn_blocking` is a later carrier
only if `Session`/`Provider` ownership is moved into the task. `shell_eval.rs`
collapses to an adapter: build `TurnRequest { script_name: "<tool>",
turn_limit: Some(30s), detached_limit: Some(1h), io: TurnIo::Capture,
surface: Some(AgentSink), lifecycle: Box::new(()), .. }`, match `TurnReport`
into the existing `Outcome`/`ToolResult` (cap `captured` bytes, synthesise the
124 message from `timed_out`, append sandbox denials, render the `Value`).
`headless.rs` keeps the same driver with a non-animating UI. exarch's
`DETACHED_WORKER_CEILING` and 30 s wall become request fields.

### ral REPL — `repl/exec.rs` collapses to an adapter

`execute_input` builds `TurnRequest { script_name: "<stdin>", caps: root(),
turn_limit: None, detached_limit: None, io: TurnIo::Inherit, surface:
Some(print_or_noop_sink), lifecycle: Box::new(ReplLifecycle { .. }) }`, calls
`run_turn` **synchronously**, and matches `TurnReport` as today (`print_result`,
exit, `Escape::Stopped` → job, `Break::Error` → ariadne). The hand-built
`TurnFrame` (`exec.rs:102-109`) is deleted; the REPL only ever sees `Static` or
`Ran { captured: None, .. }`. No async runtime is involved.

### ral batch — `main.rs` becomes the third `run_turn` client

`ral/src/main.rs:480` still calls `eval_top_level` directly (the gap
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] flagged). Fold it
in: `TurnIo::Inherit`, a no-op `EventSink`, `Box::new(())` lifecycle, and
synchronous `run_turn`; audit still drains `shell.take_audit_fragment()` after the
turn.

## Test plan

- **The instance:** a stub turn whose script is `spawn { <blocks forever> }` then a
  no-tool-call message; assert the driver returns within a tight bound, a
  `TurnReport::Ran` is produced, and (through exarch's `Session`) `session_ended`
  is recorded. Name in the concurrency ADR's spirit:
  `detached_worker_cannot_outlive_turn_completion`.
- **Completion-not-disconnect:** drive the `select!` loop with a fake detached
  holder that keeps an event `tx` clone alive; assert the loop returns on the
  explicit done future while the sender still lives.
- **Borrow-preserving carrier:** a test-shaped `Session::run_turn` borrows
  `&mut Session` and `&Provider` through the scoped worker, proving the initial
  carrier does not require `'static` ownership the current call graph lacks.
- **`watch` regression:** a watched worker still streams live after the turn
  returns — bytes are `Send` and detachment of *bytes* is intentional.
- **Deferred surface:** `spawn { edit … }` then the first `await $h` replays the
  `` `patch `` card exactly once; `poll $h` and a second `await $h` do not replay
  it; an un-awaited worker emits no card; the file is written either way.
- **Parity (the unification):** the same source through `run_turn` with
  `TurnIo::Inherit` (REPL/batch shape) and `TurnIo::Capture` (exarch shape) on one
  `Shell`; assert the `TurnReport` classification agrees and REPL lifecycle hooks
  still fire.
- **Seam / tokio boundary:** grep tests that `ral_core` names no
  `tokio`/`spawn_blocking`/`mpsc`/`select`, and that exarch's tool-eval path names
  no `TurnFrame`, `IoFrame`, `arm_lifetime`, or `eval_turn` — the
  [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
  grep, narrowed to allow `Value` and the REPL's ambient `Sink::External` setup.
- **End-to-end:** re-run `kv-store-grpc` and `pypi-server`; assert no
  `AgentTimeoutError`, non-empty `result.json`, reward still `1.0`.

## Consequences

- The hang is impossible by construction: turn completion is the call returning /
  the explicit done future resolving — a control-flow fact no background thread
  can change. There is no shared liveness object between detachment and turn
  exit. To reintroduce the class a maintainer would have to write a `select!`
  with no completion arm, a visibly wrong loop, not copy one innocuous line.
- exarch becomes genuinely reactive: one `select!` loop per turn, a frame-timer
  animation that looks alive between events, and a single cancel arm. Headless and
  TUI share the loop.
- [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] is completed:
  one `run_turn`, three hosts (REPL, exarch, batch) as request suppliers;
  `shell_eval.rs`, `exec.rs`, and `main.rs` shrink to adapters.
  [[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]] records the
  companion API cutover: `TurnFrame`, `IoFrame`, and core `TurnOutcome` collapse
  rather than surviving as a second host vocabulary.
- `Shell` stays `Send`/`Sync`: no `Rc`, no borrowed sink lifetime in
  `TurnState`, and no false requirement that the current `Session` borrow become
  `'static`. The pump/`Session` collision the prior ADR hit (`session.rs:421`) is
  moot because completion no longer flows through `drive`.
- tokio stays out of `ral_core`: the seam is a synchronous `EventSink` taking
  `Value`. The host owns its concurrency model; core is a synchronous evaluator
  with one runtime-agnostic turn entry — strengthening, not bending,
  [[decisions/260610_host-embedding-api|host-embedding-api]].
- The `surface` builtin is unchanged language-side; only its carrier moved from
  stored-and-cloned to turn-scoped. `internals/output-capture-and-detachment` needs
  a one-line correction: the surface is now turn-local, inherited by same-thread
  children, and buffered once for detached `await`.
- [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
  is *recorded as-is*, not freshly spent: the `surface` builtin already emits
  `Value` today, so `Value` already crosses this path; the rail vocabulary lives in
  exarch's `value_to_kind`, and core stays generic.
- exarch deletes more than it adds: `pump` and the channel-as-completion `drive`
  are gone; the event bus remains only a presentation bus.
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
  coloring the whole evaluator async is a large rewrite for negative benefit.
  Async at the edges, synchronous in the core, bridged by a worker (scoped while
  the host borrows state, `spawn_blocking` when it owns state) — which is what
  async is *for*.
- **A core `TurnEvent` taxonomy decoded by core.** Rejected: it puts exarch's rail
  vocabulary in core. Raw `Value` to `emit` keeps core generic and matches the
  existing `surface` builtin.

## Scope and honesty about cost

This is a real refactor of exarch's event loop — `pump` and the
channel-as-completion `drive` are deleted and replaced by the `select!` driver —
plus the core `run_turn`/`EventSink`/`TurnReport` surface absorbing frame
construction from all three hosts. The surprising part, again, is how much of the
fix is *deletion*: completion stops being a side effect of the presentation bus,
and `Shell` stays `Send`. Honest costs, none fatal:

- **Worker pressure.** The initial exarch carrier is one scoped blocking worker
  per active root turn so it can borrow `&mut Session`; an owned-state rewrite may
  switch that carrier to `spawn_blocking`. Either way, deeply nested sub-agents
  warrant a host-side bound and a test.
- **Backpressure.** Use a bounded event channel. Semantic cards (`patch`, `wrote`,
  `task`, tool/result/error/usage) block rather than disappear; progress cards
  (`meter`, `phase`) coalesce so a fast emitter cannot outrun the renderer. The
  detached `surface_buf` is bounded too and records one overflow marker instead
  of growing forever.
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
[[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]] (the API collapse
this unlocks),
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
