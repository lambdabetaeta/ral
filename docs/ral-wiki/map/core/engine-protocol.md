---
generated_at_commit: 50388d83
generated_at_date: 2026-08-29
covers_paths: [core/src/protocol.rs, core/src/engine.rs, core/src/wire.rs, core/src/hatch.rs]
---

# Map: core / engine protocol

**`core/src/protocol.rs` is the frame algebra and its two transports;
`core/src/engine.rs` is the `--engine` child that answers one of them;
`core/src/wire.rs` is the duplex byte channel both ride; `core/src/hatch.rs`
is a wire-seat child's spawn machinery.** The why is
[[design/engine-protocol|engine-protocol]]; this page only points at symbols.

## `core/src/protocol.rs`

- `PROTOCOL_VERSION` (currently 7) — checked at `Attach`; a mismatch refuses.
- `HATCH_ACK` — the guest's one-byte readiness signal, written from
  `hatch.rs`; lives here for platform neutrality, not because it is ever a
  `Frame`.
- `Frame` — the whole wire enum: `Attach`, `Detach`, `Dispatch`, `Probe`,
  `Event`, `Session`, `Control`, `Answer`, `Ping`, `Pong`.
- `Run` / `Program` — one dispatch's payload: the policy fields plus
  `Program::Source`/`Program::Hook`.
- `Event` — engine→front-end, inside a dispatch's window: `Surface`,
  `Enquiry`, `Report`.
- `SessionEvent` — engine→front-end with no dispatch to ride: `Attached` /
  `Refused(String)` (the attach verdict) and `DeferredSurface(Vec<FOValue>)`
  (a detached worker's batch).
- `EnquiryError` — message plus status, the wire shape of a refused enquiry;
  `no_desk()` is the fixed wording for a host with nothing to answer.
- `Control` / `Winsize` / `TerminalEndpoint` — the out-of-band control frame
  and the attach-time terminal conveyance (`TerminalEndpoint.lease` is
  `#[serde(skip)]`).
- `Report` / `Diagnostics` / `Ending` — the terminal frame: `Static {
  diagnostics }` or `Ran { ending, captured, trail }`; `Ending::Stopped`
  carries no platform `cfg` — the wire type is data, identical everywhere,
  only its producer (`render_ending`) is Unix.
- `render_ending` / `RunReport::into_report` — project the engine's own
  `run::Ending`/`RunReport` onto these wire shapes, rendering a caught error
  to a string against the `SourceDb`.
- `Severed` — why no further frame will cross: `Refused` / `Closed` /
  `Silent` / `Faulted`, `Display`ed as the sentence a front-end shows; the
  private `sever()` is first-cause-wins, and `WireTransport::sever` lets a
  front-end declare a `Faulted` peer dead itself.
- `ProbeError` — `Rejected` (a program error: an unknown class, a probe
  mid-run) or `Severed`.
- `Host` trait — one run's whole host-facing surface: `surface`, `enquire`,
  `fork`; `impl Host for ()` is the mute host every callerless dispatch
  installs.
- `Transport` trait — `dispatch`, `probe`, `control`, `events`, `severed`,
  `attach`, `detach`, `answer`, `set_deferred_sink`; `IdentityTransport` and
  `WireTransport` are its two instances.
- `dispatch_to_report` — mint a `DispatchId`, call `transport.dispatch`,
  drain `events()` to that dispatch's own `Report`, forwarding
  `Surface`/`Enquiry` to `host`; `Result<Report, Severed>`.
- `answer_probe` — the one probe-reading decoder both transports share
  (`worker-count`, `binding-count`, `leased-binding-count`, `env-var`, `cwd`,
  `grant-depth`, `largest-binding-bytes`, `workers`).
- `ControlSender` — out-of-band `Cancel`/`Suspend`/`Resume`/`Resize`; `new`
  trips the identity transport's own foreground scope, `new_wire` writes a
  `Control` frame through the severance cell.
- `EventReceiver` — the front-end's single-drainer event queue; its `stash`
  hands back an event a probe's or a desk's pre-drain read past, in arrival
  order, rather than dropping it.
- `IdentityTransport` / `EngineInner` / `SessionLock` — the in-process
  transport: one poison-recovering mutex around the `Shell`, and a
  `dispatch_thread` stamp `check_not_reentrant` asserts against before every
  session-lock door (`shell_mut`, `with_shell`, `dispatch`, `probe`, `attach`,
  `detach`, `set_deferred_sink`).
- `IdentityDesk` — the identity binding's `EnquiryDesk`: drains queued
  `Surface` events before calling `host.enquire`, so a handler can never
  outrun its own run's earlier output.
- `WireTransport` — the out-of-process transport. `new` is Unix-only: spawns
  a same-host `--engine` child on a socketpair, no heartbeat (a same-host
  death is a kernel-guaranteed EOF). `adopt` drives an existing stream — the
  guest-VM path — under a `Liveness` ticker. `severed()` reads the cause;
  `await_attached()` blocks on the reader's `Attached`/`Refused` verdict or
  its own `patience` deadline.
- `spawn_wire_reader` / `spawn_heartbeat` — the reader severs *before*
  dropping `event_tx`, on every exit path; the heartbeat pings on
  `Liveness::interval`, severs `Silent` past `Liveness::deadline`, and never
  takes the write lock on that path.
- `write_through` — the one write door `WireTransport::write` and
  `ControlSender`'s wire arm both share: on error, severs and shuts the
  channel down before the lock releases.

## `core/src/engine.rs`

- `EngineInstaller` — one compiled-in boot recipe (`tag`, `boot: fn() ->
  Shell`, `narrow: GrantNarrower`); only `tag` crosses `Attach`, never the
  function.
- `run_engine` — adopts fd 3 as the wire channel and calls `engine_session`.
- `engine_session` — the engine's whole protocol life: read `Attach`,
  `resolve_installer`, boot, apply a hatch seed if any, write
  `Attached`/`Refused`, then the reader loop; returns the process exit code.
- `resolve_installer` — the version check plus the installer-table lookup, a
  `Result` so the refusal path is testable without exiting.
- `WireDesk` — the wire engine's `EnquiryDesk`: writes `Event::Enquiry`,
  parks a `slots` map keyed by `EnquiryId` until `Frame::Answer` fills it or
  the run's own cancel scope fires.
- `Dispatch` — the engine's one-run-or-probe rendezvous; claiming it is the
  only way to mint one, so "engine busy" (written back to a second dispatch)
  can never be raised without a run genuinely in flight; its `Drop` lowers
  the busy flag on every exit, unwind included.
- `Patience` / `HOST_SILENCE_DEADLINE` — the engine's own read-silence and
  write-stall deadlines, armed once the first `Ping` arrives; production
  always runs `Patience::default`, a test gets a brisker one.
- The teardown settle — on any loop exit: cancel the in-flight run and the
  durable root, `hatch::teardown_hatched()`, then poll `busy` under
  `SETTLE_TIMEOUT`/`SETTLE_POLL` before exiting, so no run is abandoned
  mid-report.

## `core/src/wire.rs`

- `WireStream` — `UnixStream` on Unix, `TcpStream` on Windows: std's owner of
  a *connected stream socket*, never a statement about address family
  (`vm-manager` hands back `AF_VSOCK`/`AF_HYPERV` sockets through the same
  type).
- `WireChannel` — length-prefixed JSON framing (`subprocess_codec`) over one
  `WireStream`; `pair()` (a socketpair on Unix, a loopback accept on
  Windows), `from_stream`, `try_clone`.
- `poll_readable` — wait for a frame or a timeout without blocking inside
  `read_frame`; how `engine_session` notices a silent front-end with no
  dedicated thread.
- `set_write_deadline` — bounds every `write_frame` on every clone of the
  channel (`SO_SNDTIMEO` lives on the shared file description), turning a
  stalled write into the same fatal error a severed pipe already gives.

## `core/src/hatch.rs`

A wire-seat spawn is one exchange: the guest binds an ephemeral port for one
spawn and the host dials in — see [[design/engine-protocol|engine-protocol]]'s
hatch section for the why.

- `listen_for_hatch` — waits on a caller-bound listening descriptor for the
  one dial that hatches a child; checks the dialler's eight token bytes, and
  packs the parent's scrubbed `Shell` into an `EngineSeed` on the caller's
  own thread.
- `hatch_over` — re-execs this binary (`--engine` in production) with the
  dialled connection on fd 3 and a seed socketpair named by
  `RAL_ENGINE_SEED_FD`; writes the framed seed while the child drains it, and
  answers `HATCH_ACK` only once `spawn()` has returned and the seed has
  crossed.
- `HATCH_ACK` (defined in `protocol.rs`, written from here) — the byte that
  says the child exists and already holds its whole seed; neither it nor the
  token is a `Frame`, so a hatch never touches `PROTOCOL_VERSION`.
- `seed_from_env` — the child's own take: reads `RAL_ENGINE_SEED_FD`,
  striking the var as it takes the fd, before the engine waits for `Attach`.
- `apply_seed` — hydrates the taken seed's scope and context into the booted
  shell, then narrows its capabilities through the installer's
  `GrantNarrower`.
- `GrantNarrower` — `fn(&Capabilities, &str, &str) -> Result<Capabilities,
  String>`, a field of `EngineInstaller` rather than a registered hook: core
  has no base-tag lexicon of its own, so a seeded child's ceiling is always
  stated by the host that boots the engine.
- `HATCHED` / `teardown_hatched` / `sweep_hatched` — the process-global table
  of spawned-but-unreaped hatch children, swept by `waitpid` at the next
  hatch and again at engine teardown (a hatched child closes its seed channel
  on hydration, not on death, so only `waitpid` tells running from gone).

The seed a hatch carries is `EngineSeed` — [[map/core/transport|transport]]'s
`core/src/child_eval.rs` section.

## See also

[[design/engine-protocol|engine-protocol]] (the why — the channel table, the
laws of an enquiry, the two bindings, liveness and severance),
[[map/core/transport|transport]] (`subprocess_codec`'s framing, `EngineSeed`,
and the pipeline-helper IPC this protocol shares its codec with),
[[map/exarch/agent|exarch / agent]] (`RunHost`, the wire-seat spawn that dials
a hatch), [[map/synod|synod]] (`WireTransport::adopt` over a guest VM's
virtual socket).
