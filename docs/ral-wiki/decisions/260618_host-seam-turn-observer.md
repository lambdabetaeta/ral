---
status: superseded
superseded_by: decisions/260618_run-turn-host-loop
---

# Both hosts drive a turn through one core entry; the surface is `!Send`

> Superseded by
> [[decisions/260618_run-turn-host-loop|a turn is a synchronous call; the host owns the loop]].
> The diagnosis here stands, but making the surface `!Send` forces `Shell: !Send`,
> which collides with exarch's `pump`/`Session` move (`session.rs:421`). The
> successor deletes the channel-as-completion loop instead — completion becomes the
> call returning, so the surface's thread-discipline stops mattering, `Shell` stays
> `Send`, and `pump`/`drive`/`Emitter`-as-transport are removed rather than worked
> around. The shared `run_turn` entry is kept.

**The REPL and exarch must drive a turn the same way — through one core entry,
`Shell::run_turn(src, TurnRequest) -> TurnReport`, supplying a single
`TurnObserver` and reading back a neutral report — and the structured surface
inside that observer must be `!Send`, so it physically cannot reach a detached
worker.** Core owns the frame, the reaper, and the IO; the host owns only what it
does with the bytes and the events. The daemon-task hang — a `'static`,
`Send` surface that a `spawn` worker captured and pinned — becomes a *compile
error*: a `!Send` sink cannot be moved into a `std::thread::spawn` closure. This
also finishes [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]],
which lifted *evaluation* into core but left *frame construction* duplicated in
each host. `watch` is untouched: byte sinks stay `Send`, and streaming bytes to a
detached worker is exactly what `watch` legitimately does.

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
pipe** (`spawn_child` gives the worker its own buffers,
[[internals/output-capture-and-detachment|output-capture-and-detachment]]). That
ADR states the governing invariant: *"Detachment may hold only root-owned or
handle-owned resources, **never a foreground frame's capture state.**"* This bug
is a **third** piece of foreground-frame capture state that leaked into
detachment — the structured-event surface — and it leaked because exarch builds
the turn frame by hand and injects a `'static`, `Send` surface that core then
clones into every worker.

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
2. `Emitter: Clone` clones its `Sender` (`bus.rs:245`). On a `spawn`, exarch's
   `run_shell` builds `surface = Arc::new({ let emit = emit.clone(); move |v| … })`
   (`exarch/src/shell_eval.rs:80`) — a **`'static`, `Send` `SurfaceSink`**
   (`core/src/types/shell/mod.rs`, `Arc<dyn Fn(Value)+Send+Sync>`) capturing a
   clone of the turn's `Emitter`, hence a clone of the `Sender`.
3. exarch stuffs it into a core `TurnFrame { io: IoFrame::Capture { …, surface:
   Some(surface) } }` (`shell_eval.rs:102`) and calls `eval_turn`.
4. Core's `Shell::spawn_thread` (`core/src/types/shell/inherit.rs:195`) clones
   `self.turn.surface` (`:205`) and moves it into each detached worker's
   `std::thread::spawn` closure (`:211`, `:214`). Because the surface is `'static`
   *and `Send`*, this compiles, and the worker now holds exarch's `Sender` for as
   long as it lives.
5. A server never terminates → the `Sender` never drops → `drive` never returns →
   `pump` never returns → `run_turn` never returns. `result.json` is printed only
   *after* the loop (`exarch/src/headless.rs`), so nothing is written.

The natural experiment is in the same run: **torch** also `spawn`ed a worker (a
`pip install`) and did *not* hang — `pip` finished, dropped its `Sender`, the
channel disconnected (its `result.json` is intact, `stop_reason: step_cap`).

### Why the seam was open: 260616 unified evaluation, not the frame

[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] lifted one
top-level turn into `ral_core` as `eval_turn(shell, src, frame) -> TurnOutcome`
(`core/src/turn.rs`) and declared both hosts *"frame suppliers over one
evaluator."* That removed the duplicated *spine*. But each host still assembles
the **frame** itself: the REPL hand-builds `TurnFrame { io: IoFrame::Inherit, … }`
(`ral/src/repl/exec.rs:102-109`); exarch hand-builds the capture buffers
(`shell_eval.rs:66-67`), arms and disarms the wall (`:75`, `:121`), mints the
`'static` surface closure (`:80-87`), assembles `TurnFrame { Capture { … } }`
(`:102-114`), calls `eval_turn` (`:116`), and decodes core `Value` into rail
events with `value_to_kind` (`:248-301`).

So 260616 stopped one level short. The frame — the IO regime, the reaper, the
surface — is exactly where the two hosts still diverge by hand, and exactly where
this bug lives. Fixing it for exarch alone would re-open the split one level up.
The fix is to lift frame construction behind one entry too, so both hosts become
*request* suppliers.

## Decision

Add one entry to the host-embedding seam
([[decisions/260610_host-embedding-api|host-embedding-api]]), in `core/src/host.rs`:

```rust
impl Shell {
    pub fn run_turn(&mut self, src: &str, req: TurnRequest<'_>) -> TurnReport;
}

pub struct TurnRequest<'a> {
    pub script_name: &'a str,          // "<stdin>" (REPL) | "<tool>" (exarch)
    pub caps: Capabilities,            // root() (REPL) | grant profile (exarch)
    pub turn_limit: Option<Duration>,  // None (REPL) | Some(30s) (exarch)
    pub detached_limit: Option<Duration>, // None (REPL) | Some(1h) (exarch)
    pub observer: &'a dyn TurnObserver, // one IO object; see below
    pub lifecycle: &'a mut dyn TurnLifecycle, // REPL hooks+jobs | () for exarch
}

pub trait TurnObserver {
    /// Where the turn's bytes go. A `Send` destination — the live printer
    /// (REPL) or `None` to have core capture into buffers it returns in the
    /// report (exarch). `Send`, because a detached worker may legitimately hold
    /// it; it must therefore carry no turn-scoped transport.
    fn printer(&self) -> Option<Arc<dyn ExternalWrite>> { None }
    /// Structured events for the foreground turn. Core wraps this `!Send`, so
    /// it can never reach a detached worker. `Value` is permitted to cross.
    fn on_surface(&self, _v: &Value) {}
}

/// One flat result the host matches once. `captured`/`timed_out` live on `Ran`,
/// where they are meaningful — a `Static` turn never ran, so it neither captured
/// output nor hit a limit.
pub enum TurnReport {
    Static { diagnostics: StaticDiagnostics },   // parse/type failure; status is 1
    Ran {
        result: Settled<Value>,   // the control-flow sum: Ok | error | exit | stopped
        status: i32,
        single_command: bool,
        captured: Option<Captured>, // Some when the observer chose capture (printer == None)
        timed_out: bool,
    },
}
pub struct Captured { pub stdout: Vec<u8>, pub stderr: Vec<u8> }
```

`TurnReport` is the one outcome type the *seam* exposes. The existing
`TurnOutcome` (`eval_turn`'s `Static | Runtime` return) stays **internal**: it is
a fine place to test classification without a frame, and `run_turn` translates it
— folding in the captured bytes and `timed_out` it alone knows — into the flat
`TurnReport`. The host never unwraps twice, and `Settled<Value>` is reused rather
than re-spelled (it already encodes Ok / error / `exit N` / stopped).

Five parts.

1. **Core owns the frame; the host supplies a request and reads a report.**
   `run_turn` builds the `IoFrame` — installs the observer's `printer` as
   `Sink::External` (live), or mints `Sink::Buffer` captures when `printer` is
   `None` — wires `on_surface` as the turn's surface, arms `turn_limit` and
   `detached_limit` on `process::reaper`, calls the internal `eval_turn`, then
   flattens its `TurnOutcome` into a `TurnReport`, attaching any captured bytes
   and `timed_out`. Everything `run_shell` and `execute_input` do to assemble a
   frame moves here once.

2. **The surface is `!Send`; the hang becomes a compile error.** Recast
   `SurfaceSink` from `Arc<dyn Fn(Value) + Send + Sync>` to `Rc<dyn Fn(Value)>`
   (`core/src/types/shell/mod.rs`). The surface is only ever invoked on the
   foreground eval thread, so it never needed `Send`. With `Rc`,
   `spawn_thread`'s `self.turn.surface.clone()` cannot be moved into the worker's
   `std::thread::spawn` closure (`inherit.rs:211`,`:214`) — that closure must be
   `Send`, and capturing an `Rc` makes it `!Send`. The leak line stops
   compiling; the worker cannot inherit the foreground surface. It gets its own
   buffering surface instead (part 5), exactly as `spawn_child` already gives it
   its own byte buffers rather than the foreground's. This is the concurrency ADR's
   "detachment holds no foreground capture state" invariant, finally enforced by
   the type system rather than a comment — and `Send` is the right tool: the
   constraint was always *thread confinement*, not a lifetime. (A per-turn borrow
   cannot live on `Shell`, which outlives and is reused across turns; `!Send` needs
   nothing from `Shell`'s type.)

3. **Bytes stay `Send`; that is why `watch` is untouched.** Byte sinks
   (`Sink::External`, `Sink::Buffer`) remain `Arc`/`Send`. `watch` clones
   `turn.io.stdout` into its detached worker (`concurrency.rs:71-78`) and keeps
   streaming live to the durable printer — legitimate detached byte streaming,
   unchanged ([[decisions/260617_watch-repl-builtin|watch-repl-builtin]]). The
   `Send`/`!Send` line now states the real rule exactly: **a live byte sink may
   detach; the live foreground surface may not.** (A worker still *buffers* its
   own surface events for deferred replay — part 5.)

4. **`Value` may cross — and *only* `Value`.** This deliberately narrows
   [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
   on the turn-eval path: `on_surface(&Value)` hands exarch the raw value, and
   `TurnReport::Ran` carries a `Settled<Value>` exarch may render
   (`shell_eval.rs:211-227`). In exchange, one neutral report serves both hosts.
   A pleasant consequence of carrying raw `Value`: **core never learns exarch's
   rail vocabulary** — `value_to_kind` and the `` `patch ``/`` `wrote
   ``/`` `task ``/`` `meter `` shapes stay *in exarch*, inside its `TurnObserver`
   impl. The relaxation is scoped to `Value`; **nothing else of core's internal
   representation crosses** — no `TurnFrame`, `IoFrame`, `SurfaceSink`, `Sink`,
   `Source`, `arm_lifetime`, or `eval_turn` is named by either host.

5. **A detached worker still surfaces — deferred, like its bytes.** The worker
   gets no *live* surface, but `spawn_child` gives its handle a `surface_buf`
   beside `stdout_buf`/`stderr_buf`, and the worker's surface is a worker-local
   `Rc` closure that appends `Value`s to it. On `await`, the foreground drains
   `surface_buf` and replays each event to *its* surface — the §13.3 byte-replay
   rule, extended to structured events. So `spawn { … edit … }` then `await $h`
   draws the diff card; an un-awaited worker drops it, exactly as its bytes are
   dropped. No hang returns: the worker holds a *passive* `Send` buffer, never the
   live event `Sender`, so `drive` still waits only on the foreground surface.
   (`edit`'s file write is a side effect independent of the surface; it happens
   regardless — only the rail card is deferred.)

This *removes paths*: there is no longer a host-built frame, a `'static` surface,
or a reaper-arming dance in either host, and no `IoMode` regime flag — the byte
destination is the observer's `printer` choice. Across the seam travel only the
host's currency (`Capabilities`, `Duration`s, `Arc<dyn ExternalWrite>`,
`&dyn TurnObserver`, `&mut dyn TurnLifecycle`, the `TurnReport`), the opaque
`&mut Shell` handle, and — by the concession — `Value`.

## Why one API covers everything, and why the merge stops here

Walking the policy axes, each is a request field or orthogonal:

- **IO regime** → the observer's `printer`: `Some` = live (REPL), `None` =
  capture (exarch, bytes returned in `Ran`'s `captured`). The `IoFrame::Inherit |
  Capture` sum collapses into one host object; the `IoMode` flag is gone.
- **Structured surface** → `on_surface`, wrapped `!Send` by core — the hang fix,
  for both hosts.
- **Cancellation / foreground scope** → built inside `run_turn` as a child of the
  durable root, with the foreground/root signal slots published per turn by the
  existing `TurnGuard` (`core/src/turn.rs`). Both hosts get Ctrl-C / Esc /
  Ctrl-`\` routing for free; neither names a `CancelScope`.
- **Capabilities / `turn_limit` / `detached_limit` / lifecycle** → request fields.
  Lifecycle stays `&mut dyn TurnLifecycle` (REPL hooks + job registration; `()`
  for exarch).
- **`watch`** → untouched (part 3); a REPL-only builtin over `Send` byte sinks.
- **Outcome classification** → computed once in core; both hosts only render.

**Why the collapse stops at "one object," not "one callback."** It is tempting to
make the observer fully symmetric — `on_stdout(&[u8])`, `on_stderr(&[u8])`,
`on_surface(&Value)` — but that is exactly what the hang forbids. A per-write byte
callback must be `Send` (it is invoked from foreground pump threads), so the byte
path would hold the observer; `watch` would clone that into a detached worker; and
since exarch's observer holds its event `Sender` for `on_surface`, the worker
would pin the channel again. So the byte facet must be a plain `Send` sink that
carries *no* transport (a printer or a buffer), and the surface facet must be the
`!Send` callback. One host object, two thread-disciplines — the asymmetry lives in
the types, where the `watch`/hang argument justifies it.

## Implementation plan

### Core — `host.rs`, `turn.rs`, `inherit.rs`, `concurrency.rs`, `types/shell/mod.rs`

1. Define `TurnObserver`, `TurnRequest`, the flat `TurnReport` enum, and
   `Captured` in `host.rs`. `on_surface(&Value)` carries raw `Value`; core
   defines no event taxonomy. Keep `TurnOutcome` as `eval_turn`'s internal return
   and translate it to `TurnReport` inside `run_turn`.
2. Change `SurfaceSink` to `Rc<dyn Fn(Value)>` (`types/shell/mod.rs`). Delete the
   surface copy in `spawn_thread` (`inherit.rs:205`,`:214`) — it no longer
   compiles, which is the point; the worker's surface is `None`.
3. Add `Shell::run_turn`: from a `TurnRequest`, build the `IoFrame`
   (`printer` → `Sink::External`, else mint `Sink::Buffer` + `Source::Terminal`),
   wrap `on_surface` in the `Rc` surface, arm `turn_limit` and `detached_limit` on
   `process::reaper`, call `eval_turn`, read `timed_out` from the foreground
   scope's `CancelCause::Deadline`, and flatten into a `TurnReport`. The
   limit arm/disarm now lives here, not in `shell_eval.rs`.
4. Verify `Shell`/`TurnState` going `!Send`/`!Sync` (from the `Rc` field) breaks
   nothing: `spawn_thread` builds its child in-thread (`from_captured`) and
   captures only `Arc` fields, so the worker closure stays `Send` once the surface
   copy is gone. If some other site requires `Shell: Send`, fall back to keeping
   `SurfaceSink = Arc` but not copying it in `spawn_thread` (safe, not
   compile-enforced), or publishing the surface per-turn like the cancel slots
   (`turn.rs:156`).
5. In `spawn_child` (`concurrency.rs`), allocate a `surface_buf:
   Arc<Mutex<Vec<Value>>>` beside the byte buffers and store it on `HandleInner`.
   Inside the worker closure (where it already sets `child.turn.io.stdout`,
   `:96`), set `child.turn.surface = Rc::new({ let b = surface_buf.clone(); move
   |v| { let _ = b.lock().map(|mut g| g.push(v)); } })` — the `Rc` is born on the
   worker thread, so `!Send` is fine; it captures only the `Send` buffer. In the
   `await` drain (where `stdout_buf`/`stderr_buf` replay to the caller's sinks),
   replay `surface_buf` to the caller's `turn.surface`.

### exarch — `shell_eval.rs` collapses to an adapter

`run_shell` becomes: build `TurnRequest { script_name: "<tool>", caps, turn_limit:
Some(30s), detached_limit: Some(1h), observer: &AgentObserver, lifecycle: &mut
() }`, call `shell.run_turn(cmd, req)`, match the `TurnReport` into the existing
`Outcome`/`ToolResult` (cap the `Ran` `captured` bytes, synthesise the 124 message
from `timed_out`, append sandbox denials, render the `Value`). `AgentObserver`
implements `TurnObserver`: `printer` returns `None` (capture); `on_surface`
owns `value_to_kind` (moved off the free function, `:248-301`) and emits `Kind`s
through the `Emitter`. The buffer wiring, `arm_lifetime`, the surface closure, the
`TurnFrame`/`IoFrame` construction, and the `eval_turn` call all leave
`shell_eval.rs`. exarch's `DETACHED_WORKER_CEILING` constant and 30 s wall become
the request's `detached_limit` and `turn_limit`.

### ral — `repl/exec.rs` collapses to an adapter

`execute_input` builds `TurnRequest { script_name: "<stdin>", caps: root(),
turn_limit: None, detached_limit: None, observer: &ReplObserver, lifecycle: &mut
ReplLifecycle { runtime } }` and matches the `TurnReport` as today
(`print_result`, exit, `Escape::Stopped` → job, `Break::Error` → ariadne).
`ReplObserver::printer` returns the rustyline external printer; `on_surface` is
the default no-op (surfacing is exarch's, not the REPL's). The hand-built
`TurnFrame` (`exec.rs:102-109`) is deleted; the REPL only ever sees
`TurnReport::Static` or `Ran { captured: None, .. }`.

### What legitimately still crosses the seam

`Capabilities`; `Duration`s; `Arc<dyn ExternalWrite>` (the existing frontend
trait, already host currency); `&dyn TurnObserver`; `&mut dyn TurnLifecycle`; the
`TurnReport` (its `Captured` bytes are host currency); the `&mut Shell` handle;
and — by deliberate concession — `Value`. No host names `TurnFrame`, `IoFrame`,
`SurfaceSink`, `Sink`, `Source`, `arm_lifetime`, or `eval_turn`.

## Test plan

- **Type-level (the class):** a `// @compile-fail` fixture that tries to move a
  surface into a `spawn` worker must fail to compile — the `!Send` `Rc` is the
  guard. Also a static assertion that `SurfaceSink: !Send`.
- **Integration (the instance):** a stub turn whose script is
  `spawn { <blocks forever> }` then a no-tool-call message; assert `run_turn`
  returns within a tight bound, a `TurnReport::Ran` is produced, and (through
  exarch's `Session`) `session_ended` is recorded. Name in the spirit of the
  concurrency ADR's `await_unwinds_on_foreground_cancel_sparing_the_worker`, e.g.
  `live_spawn_worker_cannot_capture_the_surface`.
- **`watch` regression:** a REPL-side test that a `watch`ed worker still streams
  live after the turn returns — bytes are `Send` and detachment of *bytes* is
  intentional.
- **Deferred surface:** `spawn { edit … }` then `await $h` replays the `` `patch ``
  event to the foreground rail; assert the file is written either way, and that an
  un-awaited worker emits no card — surface events follow the byte-replay rule.
- **Parity (the unification):** drive the same source through `run_turn` with
  `printer: Some` (REPL shape) and `printer: None` (exarch shape) on one `Shell`
  and assert the `TurnReport` classification agrees.
- **Seam:** assert neither host imports `TurnFrame`, `IoFrame`, `SurfaceSink`,
  `Sink`, `Source`, `arm_lifetime`, or `eval_turn` — a grep test in the spirit of
  [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]],
  narrowed to allow `Value`.
- **End-to-end:** re-run `kv-store-grpc` and `pypi-server`; assert no
  `AgentTimeoutError`, non-empty `result.json`, reward still `1.0`.

## Consequences

- The hang is impossible by construction, for the surface, *by `!Send`*: exarch's
  transport cannot be moved onto a worker thread. The REPL could not make the
  mistake either.
- [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] is completed:
  both hosts are *request* suppliers over one driver. `shell_eval.rs` and
  `exec.rs` shrink to adapters; the frame/reaper/surface machinery lives once in
  core. `IoMode` never needs to exist — the byte destination is the observer's
  `printer` choice.
- The seam exposes **one** outcome type. `TurnReport` is a flat `Static | Ran`
  the host matches once, with `captured`/`timed_out` on `Ran` where they apply;
  `TurnOutcome` is demoted to `eval_turn`'s internal classification. `Settled<Value>`
  is reused, not re-spelled.
- [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
  is *narrowed*: `Value` crosses on this path. Net of it, core knows *less* — the
  rail vocabulary moves into exarch's observer.
- `watch` and all byte streaming are untouched; the `Send`/`!Send` split is the
  durable statement of which IO may detach.
- A detached worker's surface events are not lost: they buffer on the handle and
  replay on `await`, mirroring the byte-replay rule (§13.3). Only a worker that is
  never awaited drops them — exactly as it already drops its buffered bytes.
- `run_turn` returns the moment the model is done, so `result.json` and
  `session_ended` are written.
- The 1 h `detached_limit` is unchanged as a backstop, no longer load-bearing
  for turn exit. Do **not** lower it to mask the bug.
- `internals/output-capture-and-detachment` needs a one-line correction: the
  turn-owned **surface** was the foreground-frame capture state a detached worker
  could pin; it now cannot, by type.
- `Shell` becomes `!Send`/`!Sync` (the `Rc` surface). Believed contained; see the
  implementation plan's verification step and its fallbacks.

## Alternatives considered

- **Borrowed surface (`&'turn dyn`), lifetime-enforced.** The first instinct, and
  what an earlier draft assumed. Rejected: a per-turn borrow cannot live on the
  persistent `Shell`, which is reused across turns, so it would force a lifetime
  through `Shell`/`TurnState`/`Io` (or a scoped pump and a borrowed `Sink`). `Rc`
  / `!Send` achieves the same compile-time guarantee with no lifetime and no `Sink`
  change — the constraint was thread confinement, not lifetime.
- **`TurnReport` wraps `TurnOutcome` plus two fields** (the previous draft:
  `struct TurnReport { outcome, captured, timed_out }`). Rejected: it makes the
  host unwrap twice and leaves `captured`/`timed_out` meaningful for only one of
  `TurnOutcome`'s variants — what does `timed_out` mean for a parse error? The
  flat `Static | Ran` enum puts those fields on `Ran`, gives the host one match,
  and keeps `TurnOutcome` as an internal classification detail.
- **Retire `watch`.** It is the only detached worker that holds a turn byte sink,
  so retiring it would make the byte path purely per-turn too and let bytes and
  surface merge into one object. Rejected: `watch` is a wanted interactive
  feature, and `!Send` already fixes the bug without it — retiring it buys only a
  cosmetic merge the hang argument shows is unsafe anyway.
- **Keep `Value` out of exarch; fork the return type** (`Value`-free report for
  exarch, `Value`-rich outcome for the REPL, over one engine). Workable, preserves
  260615 — but two return types for one driver, for a purity the team chose not to
  buy here.
- **A core `TurnEvent` taxonomy** decoded by core. Rejected once `Value` may
  cross: it puts exarch's rail vocabulary in core. Raw `Value` to `on_surface`
  keeps core generic.
- **`Drive::Done` completion signal on exarch's channel** (worker sends a
  terminator; `drive` stops on it). Good independent defence-in-depth for `pump`,
  but leaves the seam open — exarch still builds the frame and injects the surface.
- **`Weak` surface.** Fixes the instance, keeps refcount-as-lifecycle, unnecessary
  once the surface is `!Send` — by
  [[decisions/260430_typed-state-flow-wrappers|typed-state-flow-wrappers]]'s
  restraint rule, don't add a wrapper that prevents no remaining mistake.
- **One-line `surface = None` in `spawn_thread`, leave the type `Send`.** The
  `!Send` recast subsumes this: same effect, but compile-enforced instead of by
  convention. Kept only as the fallback if `Shell: !Send` proves untenable.

## Scope and honesty about cost

This is a real refactor of one seam: a host-API surface in `host.rs`, `run_turn`
absorbing the frame/reaper/surface construction from *both* hosts, and a one-type
change (`SurfaceSink: Arc → Rc`) that turns the leak into a compile error. The
surprising part is how *small* the structural fix is — `Send` was the wrong bound
all along. Two honest costs: it spends
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]'s
`Value`-non-leak on this path (a deliberate trade for one common API), and it
makes `Shell` `!Send` (believed contained; verified per the implementation plan).
The `ral` batch path (`ral/src/main.rs`, still calling `eval_top_level` directly —
a gap 260616 also flagged) is a natural third `run_turn` client, left out of scope
here.

See also
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] (the unification
this completes),
[[decisions/260610_host-embedding-api|host-embedding-api]] (the seam this
extends),
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
(the data-boundary rule this narrows),
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(the "detachment holds no foreground capture state" invariant, now `Send`-enforced),
[[decisions/260617_watch-repl-builtin|watch-repl-builtin]] (the byte streaming
kept intact),
[[internals/output-capture-and-detachment|output-capture-and-detachment]] (the
data-pipe story this completes for the surface), [[map/exarch|map: exarch]],
[[map/repl/loop|repl/loop]].
