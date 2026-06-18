---
status: proposed
---

# A host drives a turn through a seam; its observer cannot reach a worker

**exarch must invoke ral the way every other embedder does — through the
host-embedding seam ([[decisions/260610_host-embedding-api|host-embedding-api]]),
passing a *borrowed, turn-scoped observer* and reading back a host-currency
report — never by building core's `TurnFrame`/`IoFrame`, injecting a `'static`
`SurfaceSink` that closes over its own transport, and destructuring core `Value`.**
The daemon-task hang is the symptom of that seam being open: exarch handed core a
`'static` surface capturing its event `Sender`, and core — correctly, for its own
reasons — clones the surface into detached `spawn` workers, so a worker that never
dies pins exarch's per-turn channel and the turn can never end. Close the seam and
the coupling is not merely fixed, it is unrepresentable: a worker needs `'static`,
a turn observer is a borrow, so the observer provably cannot escape into one.

## Context

A Terminal-Bench run (`exarch-bench/.../2026-06-18__13-40-33`) hangs on the two
daemon tasks, `kv-store-grpc` and `pypi-server`. Both score reward `1.0`, the
model finishes in ~80 s, then exarch idles ~13.5 min until Harbor SIGKILLs it at
the 900 s wall. Agent `result.json` is 0 bytes for both; `job.log` shows `failed to
read exarch result.json`; `exception.txt` shows Harbor blocked in
`process.communicate()` — **the exarch process itself never exited.** The session
log ends on the model's final no-tool-call message with **no `session_ended`**: the
work was done, exarch could not return.

[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
already cut two couplings between a turn and a detached worker — the **cancel
scope** (workers hang at the durable root, not the foreground) and the **data pipe**
(`spawn_child` gives the worker its own buffers, so there is no turn-owned pipe to
hold open, [[internals/output-capture-and-detachment|output-capture-and-detachment]]).
That same ADR states the governing invariant: *"Detachment may hold only root-owned
or handle-owned resources, **never a foreground frame's capture state.**"* This bug
is a **third** piece of foreground-frame capture state that leaked into detachment,
in violation of that invariant — and the leak runs straight across the host seam.

### The mechanism, exactly

`Session::apply` runs the whole round-trip loop internally (`exarch/src/session.rs:244`),
so there is one event channel per turn, owned by `pump`.

1. `pump` (`exarch/src/bus.rs:305`) opens `channel()`, runs `apply` on a
   `std::thread::scope` worker, and on the main thread calls `sink.drive(rx)`
   (`bus.rs:330`). The default `drive` is `while let Ok(ev) = rx.recv()`
   (`bus.rs:293`) — it returns only on channel **disconnect**, i.e. when the last
   `Sender<Event>` drops. Its doc says so: *"drains the channel until the worker
   drops its sender."*
2. `Emitter: Clone` clones its `Sender` (`bus.rs:246`). When the model runs a
   `spawn`, exarch's `run_shell` builds
   `surface = Arc::new({ let emit = emit.clone(); move |v: RalValue| … })`
   (`exarch/src/shell_eval.rs:80`) — a **`'static` `SurfaceSink`**
   (`core/src/types/shell/mod.rs:152`, `Arc<dyn Fn(Value)+Send+Sync>`) that captures
   a clone of the turn's `Emitter`, hence a clone of the `Sender`.
3. exarch stuffs that surface into a core `TurnFrame { io: IoFrame::Capture { …,
   surface: Some(surface) } }` (`shell_eval.rs:102`) and calls `eval_turn`.
4. Core's `Shell::spawn_thread` (`core/src/types/shell/inherit.rs:195`) clones
   `self.turn.surface` into each detached worker (`:205`, `:214`). Because the
   surface is `'static`, this compiles and the worker now holds exarch's `Sender`
   for as long as it lives.
5. A server never terminates → the `Sender` never drops → `drive` never returns →
   `pump` never returns → `run_turn` never returns. `result.json` is printed only
   *after* `run_turn` (`exarch/src/headless.rs:357`,`:363`), so nothing is written.

The natural experiment is in the same run: **torch** also `spawn`ed a worker (a
`pip install`) and did *not* hang — because `pip` finished, dropped its `Sender`,
and the channel disconnected (its `result.json` is intact, `stop_reason: step_cap`).

### Why the seam let this happen

Two seam violations conspire, both visible in `shell_eval.rs`:

- **exarch builds core's turn machinery by hand.** It imports and constructs
  `TurnFrame`, `IoFrame::Capture`, `Sink::Buffer`, `Source`, arms the reaper
  (`ral_core::process::arm_lifetime`, `:75`), sets `detached_ceiling`, and calls
  `eval_turn` directly (`shell_eval.rs:16-20`, `:75`, `:102-116`). The host owns
  the turn *frame*, so it is the host that gets to inject a surface — and nothing in
  the types bounds that surface to the turn.
- **exarch destructures core `Value`.** `value_to_kind` (`shell_eval.rs:248-280`)
  matches `RalValue::Variant`/`Map`/`String`/… to decode structured events. This is
  precisely what [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
  forbids — *"the host reads core's state through behavioural accessors, never by
  destructuring core's internal representation … only the host's own currency, a
  rendered `String` or a `bool`, ever crosses."* The surface is the leak's vehicle
  and `Value` is the leaked representation.

[[decisions/260610_host-embedding-api|host-embedding-api]] deliberately left
"output capture … in the hosts, interposed before and after the call." That was the
right call for *buffer* capture, but the **structured-event surface** was swept
along with it, and the surface — unlike a buffer — is a live handle onto the host's
transport with a `'static` type that invites exactly the worker-capture this bug is.

## Decision

Move the per-turn evaluation behind the host-embedding seam so exarch *uses* ral
rather than reaching into it. Three parts, in priority order.

1. **Core owns the turn; the host supplies a borrowed observer and reads a
   report.** Add a per-turn entry to `core/src/host.rs` (the home
   [[decisions/260610_host-embedding-api|host-embedding-api]] established):

   ```rust
   pub enum TurnEvent { Task{…}, Patch{…}, Write{…}, … }   // host currency, no Value
   pub trait TurnObserver { fn on_event(&self, ev: TurnEvent); }
   pub struct TurnRequest { pub wall: Duration, pub detached_ceiling: Option<Duration> }
   pub struct TurnReport  { pub stdout: Vec<u8>, pub stderr: Vec<u8>,
                            pub value: Option<String>, pub exit: i32,
                            pub timed_out: bool, pub diagnostics: Option<String> }

   impl Shell {
       pub fn run_command(
           &mut self, cmd: &str, caps: &Capabilities,
           req: TurnRequest, obs: &dyn TurnObserver,   // <-- borrowed, not 'static
       ) -> TurnReport { /* builds the frame, arms the wall + ceiling,
                            wires the surface to obs, decodes Value→TurnEvent,
                            renders the outcome to host currency */ }
   }
   ```

   Core now builds `TurnFrame`/`IoFrame`/the surface internally, arms the reaper,
   and decodes `Value` into `TurnEvent` on its own side of the seam — `value_to_kind`
   moves into core, where the `Value` shape lives. exarch implements `TurnObserver`
   over its `Emitter` (`TurnEvent → Kind → emit`) and stops naming `TurnFrame`,
   `IoFrame`, `SurfaceSink`, `Sink`, `Source`, `RalValue`, `arm_lifetime`,
   `eval_turn`, `TurnOutcome`, `Break`, `Escape` on this path.

2. **The observer is a borrow, so it cannot reach a detached worker — by the type
   system, not by discipline.** `obs: &dyn TurnObserver` is non-`'static`. A
   `spawn` worker is a `std::thread::spawn` closure requiring `'static`
   (`inherit.rs:211`). Therefore core *cannot* clone the foreground observer into a
   detached worker — the borrow checker rejects it. The leak that caused the hang
   becomes a compile error. This is the concurrency ADR's "detachment holds no
   foreground-frame capture state" invariant, finally enforced by lifetime instead
   of comment. Detached workers keep getting their own buffers and `None` surface,
   exactly as `spawn_child` already does for IO.

3. **REPL `watch` keeps its separate, explicitly durable sink.** A watched worker
   *does* stream live, but to the REPL's `'static` external-printer sink — a
   host-owned durable resource registered out-of-band, not the per-turn observer.
   ([[decisions/260617_watch-repl-builtin|watch-repl-builtin]] already makes `watch`
   a host-installed builtin that exarch does not register.) So the durable-streaming
   case has its own `'static` channel and the per-turn observer stays a borrow for
   everyone.

This *removes a path* rather than adding one, which is the point of the request:
there is no longer a host-built frame, a `'static` surface, or a `Value` decode in
exarch; there is one seam, and across it travel only the host's currency
(`Capabilities`, `Duration`, `&dyn TurnObserver`, `TurnReport`) plus the opaque
`Shell` embedding handle.

## Implementation plan

### Core — `core/src/host.rs`, `inherit.rs`, surface decode

1. Define `TurnEvent`, `TurnObserver`, `TurnRequest`, `TurnReport` in `host.rs`.
   `TurnEvent` mirrors today's structured `Kind` payloads (task / patch / write)
   in host currency — no `Value`, no `Break`, no `Status`.
2. Move the `Value → TurnEvent` decode (today exarch's `value_to_kind`,
   `shell_eval.rs:248-280`) into `host.rs`. It belongs where the `Value` shape is
   owned ([[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]).
3. Add `Shell::run_command` (or `host::run_command(&mut Shell, …)`): build the
   `IoFrame::Capture` with the buffers, install a surface that calls
   `obs.on_event(decode(v))`, arm the wall and `detached_ceiling` on
   `process::reaper`, call `eval_turn`, then render `TurnOutcome`/`Status`/buffers
   into a `TurnReport`. All the core types exarch currently destructures stay
   inside this function.
4. Change the foreground surface slot so it carries the borrowed observer for the
   eval's duration rather than a `'static` `SurfaceSink`. The minimal form: give
   `TurnState`'s surface field the observer's lifetime so `spawn_thread`'s `'static`
   clone (`inherit.rs:214`) no longer typechecks against it, forcing the worker to
   take `None`. If threading a lifetime through the ephemeral `TurnState` proves too
   invasive, fall back to the **one-line structural fix**: `spawn_thread` simply does
   not copy the foreground observer into the child (it already builds a fresh child
   and copies only selected fields). The borrowed form is preferred because it makes
   the invariant unrepresentable; the one-liner makes it merely true.

### exarch — `shell_eval.rs` collapses to an adapter

`run_shell` becomes: construct a `TurnRequest { wall: 30s, detached_ceiling: 1h }`,
pass `&caps` and a `TurnObserver` that forwards `TurnEvent → Kind` through the
`Emitter`, call `shell.run_command(...)`, and map the `TurnReport` to the existing
`Outcome`/`ToolResult`. The `DETACHED_WORKER_CEILING` constant and the wall move
into the `TurnRequest` exarch passes; the imports at `shell_eval.rs:16-20` shrink to
the host API plus `Capabilities`.

### What legitimately still crosses the seam

`Capabilities` (core-built from `--base`, carried opaquely by the host, judged only
by `capability::check_*` per [[decisions/260605_witness-collapse|witness-collapse]]);
`Duration`s; the `&mut Shell` embedding handle (method-only, no field reach);
`BakedPrelude`/`boot_shell`. Nothing core *destructures* in exarch remains on this
path.

## Test plan

- **Type-level (the class):** the existence of `spawn_thread` refusing to capture a
  borrowed observer is itself the guard — a test that tries to leak a turn observer
  into a `spawn` worker should fail to compile (a `// @compile-fail` fixture). If
  the one-line fallback is taken instead, an integration test stands in.
- **Integration (the instance):** a stub turn whose script is
  `spawn { <blocks forever> }` then a no-tool-call message; assert `run_command` /
  `run_turn` returns within a tight bound, `session_ended` is recorded, and a
  `TurnReport` is produced. Name in the spirit of the concurrency ADR's
  `await_unwinds_on_foreground_cancel_sparing_the_worker`, e.g.
  `live_spawn_worker_does_not_pin_the_turn_observer`.
- **Seam (the regression that hid it):** assert exarch no longer imports `TurnFrame`,
  `IoFrame`, `SurfaceSink`, `RalValue` on this path — a grep test in the spirit of
  [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]].
- **End-to-end:** re-run `kv-store-grpc` and `pypi-server`; assert no
  `AgentTimeoutError`, non-empty `result.json`, reward still `1.0` (the server must
  still be listening — detachment is preserved, only the *wait* is gone).

## Consequences

- The hang is impossible by construction: exarch's transport never reaches a core
  worker, because the observer it supplies cannot outlive the eval that borrows it.
- The per-turn eval path comes into compliance with
  [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]:
  `Value` no longer crosses into exarch; `shell_eval.rs` becomes a thin adapter.
- `run_turn` returns the moment the model is done, so `result.json` and
  `session_ended` are written — the report's separate "flush usage incrementally"
  item is now lower priority (it matters only if the model genuinely runs to the
  wall).
- The 1 h `detached_ceiling` is unchanged and correct as a backstop; it is no longer
  load-bearing for turn exit. Do **not** lower it to mask the bug.
- `internals/output-capture-and-detachment` needs a one-line correction: "no
  turn-owned pipe" was true, but the turn-owned **observer** was the foreground-frame
  capture state a detached worker could pin; it no longer can.

## Alternatives considered

- **Patch the driver: an owned `Drive::Done` completion signal on exarch's event
  channel** (the worker sends an explicit terminator; `drive` stops on it, never on
  refcount). This fixes exarch's *own* loop robustly and is a good independent
  invariant — but it leaves the seam open: exarch still builds the frame, injects a
  `'static` surface, and decodes `Value`. It treats a boundary leak as a transport
  bug. Keep it as optional defense-in-depth for `pump`, not the structural fix.
- **`Weak` surface** (the worker holds `Weak<Sender>`). Fixes the instance, keeps
  refcount-as-lifecycle, and is unnecessary once the observer is a borrow — by
  [[decisions/260430_typed-state-flow-wrappers|typed-state-flow-wrappers]]'s
  restraint rule, don't add a wrapper that prevents no remaining mistake.
- **Set `surface = None` on detached workers in `spawn_thread`, leave the seam as
  is.** This is the one-line fallback above; it fixes the bug by convention in one
  spot but keeps exarch reaching into core and `Value` crossing. Acceptable as an
  interim, not the target.
- **Lower `detached_ceiling` below the host wall.** Rejected: orthogonal, fights a
  prior decision, and still leaves the turn *waiting* on a worker. The turn must not
  wait at all.
- **A session-scoped structured-concurrency nursery** owning every `spawn` worker.
  Attractive for "too many termination paths," but it is a *session*-teardown
  concern and joining detached workers is exactly what a turn must not do. Future
  tidying, not this.

## Scope and honesty about cost

This is a real refactor of one seam, not a keystroke: a host-API surface in
`core/src/host.rs`, the `Value→TurnEvent` decode moving into core, and the turn
frame/reaper construction moving off exarch. It is bounded — the per-turn eval path
— and it pays for itself by deleting exarch's hand-rolled frame and bringing the
path into line with an already-active decision. Full purity (exarch naming *zero*
core types anywhere) is a longer journey other decisions track; this ADR closes the
seam the bug came through.

See also
[[decisions/260610_host-embedding-api|host-embedding-api]] (the seam this extends),
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]] (the
data-boundary rule this enforces on the eval path),
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(the "detachment holds no foreground capture state" invariant, now lifetime-enforced),
[[internals/output-capture-and-detachment|output-capture-and-detachment]] (the
data-pipe story this completes for the observer), [[map/exarch|map: exarch]].
