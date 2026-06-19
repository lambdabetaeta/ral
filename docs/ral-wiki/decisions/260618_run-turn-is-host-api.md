---
status: active
---

# The turn entry is the host API; frame and outcome mirrors collapse

**`Shell::run_turn(src, TurnRequest) -> TurnReport` is the only host-facing
evaluation seam. `TurnFrame`, `IoFrame`, `TurnOutcome`, and public `eval_turn`
disappear as host-visible names; core materialises a request directly into the
existing `TurnState`/`Io` machinery and returns one report.** The split that
remains is semantic: hosts describe *policy* (`TurnRequest`, `TurnIo`,
`SurfaceSink`, lifecycle hooks), while core owns *resources* (`Sink`, `Source`,
`TurnState`, guards, buffers, signal slots). A second vocabulary is allowed only
when it enforces a different invariant. A mirror type is deleted.

## Context

[[decisions/260618_run-turn-host-loop|run-turn-host-loop]] fixes the daemon-task
hang by making turn completion explicit: the turn call returns, or the host's
worker sends a done value. That decision introduces the right host seam, but it
also raises a naming question: should there be public request/report types and
internal frame/outcome types with nearly the same shape?

The current source exposes the intermediate layer. `ral_core` publicly re-exports
`IoFrame`, `TurnFrame`, `TurnOutcome`, and `eval_turn`; the REPL and exarch import
those names and assemble frames themselves. That was the useful bridge for
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]], but it is not
the final seam. If it remains public after `run_turn` lands, the repository has
two host APIs: the clean one and the old one.

The cutover rule is simple: **policy crosses the host boundary; materialisation
stays in core.**

## Decision

### 1. The public turn API is one layer

Hosts may name these turn types:

```rust
impl Shell {
    pub fn run_turn(&mut self, src: &str, req: TurnRequest<'_>) -> TurnReport;
}

pub struct TurnRequest<'a> {
    pub script_name: &'a str,
    pub caps: Capabilities,
    pub turn_limit: Option<Duration>,
    pub detached_limit: Option<Duration>,
    pub io: TurnIo,
    pub surface: Option<SurfaceSink>,
    pub lifecycle: Box<dyn TurnLifecycle + 'a>,
}

pub enum TurnIo { Inherit, Capture }

pub enum TurnReport {
    Static { diagnostics: StaticDiagnostics },
    Ran {
        result: Settled<Value>,
        status: i32,
        single_command: bool,
        captured: Option<Captured>,
        timed_out: bool,
    },
}
```

`SurfaceSink`, `EventSink`, `TurnLifecycle`, `Captured`, and
`StaticDiagnostics` are part of the same public seam because hosts supply or
render them. `Sink`, `Source`, `Io`, `TurnState`, signal slots, guard types,
`arm_lifetime`, and evaluation helpers are not.

### 2. `TurnFrame` and `IoFrame` collapse

`TurnRequest` replaces `TurnFrame` as the host policy object. Core consumes the
request and builds the installed turn directly:

- `TurnIo::Inherit` clones the ambient `shell.turn.io` streams.
- `TurnIo::Capture` mints private stdout/stderr buffers and remembers them in a
  private `CaptureBuffers` holder for the final `TurnReport`.
- `surface` is installed on the new `TurnState` for same-thread children; the
  detached path receives bounded deferred storage instead.
- foreground scope, detached ceiling, source cursor, and lifecycle hooks are
  installed by `run_turn`, not by the host.

No public `IoFrame` remains. No long-lived internal `IoFrame` is needed either:
`TurnIo` is intent, `Io` is the materialised resource. A small private helper
struct is permitted only if it names resources (`MaterializedIo`,
`CaptureBuffers`, `TurnInstall`), not if it mirrors the host request.

### 3. `TurnOutcome` collapses into `TurnReport`

`TurnOutcome` exists today because `eval_turn` stops before capture and timeout
classification. After `run_turn` owns frame construction, capture buffers, and
limit arming, that distinction stops carrying its own invariant.

`run_turn` returns `TurnReport` directly:

- parse/type failure becomes `TurnReport::Static`;
- runtime completion becomes `TurnReport::Ran`;
- `status`, `single_command`, `captured`, and `timed_out` are computed once, in
  the same function that owns the relevant state.

Internal helpers may return ordinary `Result`/tuples while factoring the code,
but there is no named core `TurnOutcome` type. The exarch `session::TurnOutcome`
for provider-message outcomes is a different layer and remains outside this
cutover.

### 4. `eval_turn` stops being a host function

`eval_turn` is inlined into `run_turn` or made `pub(crate)` under a name that
does not read as a host API, such as `eval_compiled_turn`. Tests that need the
real host seam call `run_turn`; tests that need guard mechanics live in core and
use private helpers.

`core/src/lib.rs` no longer re-exports `TurnFrame`, `IoFrame`, `TurnOutcome`, or
`eval_turn`.

## Implementation plan

1. Add the public seam in `core/src/host.rs`: `TurnRequest`, `TurnIo`,
   `TurnReport`, `Captured`, `EventSink`, `SurfaceSink`, and `Shell::run_turn`.
2. Move compile/typecheck, frame install, lifecycle, eval, status classification,
   timeout classification, and capture draining behind `run_turn`.
3. Delete `IoFrame` and `TurnFrame`; replace their construction sites with
   private materialisation helpers that return `Io`, `CaptureBuffers`, and the
   installed `TurnState` guard.
4. Delete core `TurnOutcome`; return `TurnReport` from `run_turn` and adapt tests
   to match it.
5. Stop exporting `eval_turn`; make any remaining evaluator helper `pub(crate)`.
6. Migrate `ral/src/repl/exec.rs`, `ral/src/main.rs`, and
   `exarch/src/shell_eval.rs` to build `TurnRequest` and render `TurnReport`.
7. Add grep guards: hosts must not name `TurnFrame`, `IoFrame`, `eval_turn`,
   `arm_lifetime`, or core `TurnOutcome`; `ral_core` must not name `tokio`,
   `spawn_blocking`, `mpsc`, or `select`.

## Consequences

- There is one host API, not a clean API beside a legacy frame API.
- Capture and timeout classification live beside the state that proves them.
- `shell_eval.rs`, `repl/exec.rs`, and `main.rs` become request builders and
  report renderers.
- `TurnState` remains the only installed dynamic frame; `Io` remains the only
  materialised byte-resource bundle.
- `TurnIo` is small because it is intent. `Io` is rich because it owns resources.
- The name `TurnOutcome` is freed from `ral_core`, reducing confusion with
  exarch's provider-level `session::TurnOutcome`.

## Alternatives considered

- **Keep `TurnFrame`/`IoFrame` as private internal mirrors.** Rejected as the
  default: private is better than public, but a mirror still invites drift. Use
  private resource holders only when they name a real resource or guard.
- **Keep `TurnOutcome` internal.** Rejected unless a future helper proves it owns
  an invariant different from `TurnReport`. Today it is just the report before
  the fields `run_turn` can now fill.
- **Expose materialised IO to hosts.** Rejected: it leaks `Sink`/`Source` and
  reopens the old frame-construction API under another name.

See also [[decisions/260618_run-turn-host-loop|run-turn-host-loop]],
[[decisions/260618_after-turn-api-simplifications|after-turn-api-simplifications]]
(the follow-on cleanup draft), [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]],
[[decisions/260610_host-embedding-api|host-embedding-api]], and
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]].
