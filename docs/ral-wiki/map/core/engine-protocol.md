---
generated_at_commit: 4dc94095
generated_at_date: 2026-09-24
covers_paths: [core/src/protocol.rs, core/src/protocol/, core/src/engine.rs, core/src/engine/, core/src/wire.rs, core/src/hatch.rs, core/src/engine_seed.rs, core/src/spawn_grant.rs]
---

# Map: core / engine protocol

**`core/src/engine.rs` is the one engine; `core/src/protocol.rs` is the frame
algebra and the two carriers that drive it — the identity transport in
process, the wire transport's front-end half — with `engine/wire.rs` the
wire's engine half; `protocol/reading.rs` types every probe class once;
`core/src/wire.rs` is the duplex byte channel; `core/src/hatch.rs` is a
wire-seat child's spawn machinery.** The why is
[[design/engine-protocol|engine-protocol]]; this page only points at symbols.

## `core/src/engine.rs`

- `EngineInstaller { tag, boot, narrow }` — one compiled-in boot recipe.
  `boot: fn(&Attach) -> Result<Booted, String>` reads the attach (its
  `config` above all) and refuses in its own words; `narrow: GrantNarrower`
  is the policy a seeded or adopted child is held to. Only `tag` crosses
  `Attach`, never the function.
- `Booted { shell, keep }` — what a recipe boots: the shell, and whatever
  must outlive it (a scratch directory, say), dropped after the shell.
- `Engine` — one session's engine: the `Shell`, its `Scopes`, the installer
  it was born from. `Engine::boot(installers, &attach, seed)` is the one
  boot both carriers call: `resolve_installer` (version check, then tag
  lookup), the recipe, `seed_cwd`, the attach's env, the
  guest jail under `RAL_GUEST`, and `EngineSeed::apply` for a hatch.
  `Engine::run` runs one dispatch under the scope `Scopes::open` minted;
  `Engine::probe` answers one reading.
- `Rails` — one dispatch's host-facing rails as its carrier lays them: the
  `Outlet` its events leave by, the deferred sink, the desk, the `Fork` arm.
  The carriers differ in these and nowhere else.
- `Scopes` — what stops an engine's runs, shared with whichever thread
  carries `Control`: the dispatch scope root, the durable root, and a slot
  holding the current dispatch and a foretold `Cancel`. `open` mints a
  dispatch's scope ahead of its frame (struck already if a `Cancel` foretold
  it); `apply` is `Control`'s one meaning — `Interrupt` strikes the current
  dispatch, `Cancel(id)` strikes it if it bears `id` and foretells it
  otherwise, `Terminate` cancels the durable root.

## `core/src/protocol.rs`

- `PROTOCOL_VERSION` (currently 8) — checked at `Attach`; a mismatch refuses.
- `check_media` — the same number compared at *build* time, against the
  `proto_version=` line `vm-image/build-boot.sh` records for the engine in the
  guest media; `synod/examples/check-media.rs`, tauri's `beforeBuildCommand`,
  refuses to package media that disagrees.
- `HATCH_ACK` — the guest's one-byte readiness signal, written from
  `hatch.rs`; lives here for platform neutrality, not because it is ever a
  `Frame`.
- `Frame` — the whole wire enum: `Attach`, `Detach`, `Dispatch`, `Probe`,
  `Event`, `Session`, `Control`, `Answer`, `Ping`, `Pong`; `Frame::kind`
  names a variant for the diagnostic about a frame out of place.
- `Attach` — what every engine is born from, under either carrier:
  `terminal`, `cwd`, `home`, `proto_version`, `installer`, `env` (per-session
  variables the host seeds), `config` (the installer's own settings, an
  `FOValue` its recipe decodes). `Attach::new` stamps this build's version;
  `with_config` sets the settings.
- `Run` / `Program` — one dispatch's payload: the policy fields plus
  `Program::Source`/`Program::Hook`.
- `Event` — engine→front-end, inside a dispatch's window: `Surface`,
  `Enquiry`, `Report`. Every surfaced value is a tagged variant; an audit
  observation rides as `` `observed <record> `` (`Observation::to_surface`).
- `SessionEvent` — engine→front-end with no dispatch to ride: `Attached` /
  `Refused(String)` (the attach verdict) and `DeferredSurface(Vec<FOValue>)`
  (a detached worker's batch, and a watched worker's lines).
- `EnquiryError` — message plus status, the wire shape of a refused enquiry;
  `no_desk()` is the fixed wording for a host with nothing to answer.
- `Control` — `Interrupt`, `Cancel(DispatchId)`, `Terminate`, `Abort` (a
  `Terminate` whose cause is `RootAbort`); applied by `Scopes::apply` under
  both carriers.
- `Report` / `Ending` / `FailureStatus` — the terminal frame:
  `Static { rendered, status }` or `Ran { ending, captured, trail }`; `Ending`
  has no stop arm — ral does not suspend, so every ending is
  `Settled`/`Raised`/`Walled`/`Unreturnable`/`Exited`. `Unreturnable` is a
  run that settled on a value the wire cannot carry (a handle, a block, a
  function), rendered with a hint and reported as status 1. `Raised`,
  `Walled` and `Unreturnable` carry their error's `record` — what `try`
  hands its handler, and what `--audit` builds its `` `err `` outcome from —
  and the two failing arms a `FailureStatus`, never a bare `i32`: it clamps
  into `1..=255` on the way in and on the way back off the wire, so no ending
  that failed can be reported with the code that means success.
  `Report::host_fault` is the engine's own refusal (a panicked worker, a busy
  engine) shaped as a `Static`, so a host never has to tell it from a run's
  own failure.
- `render_ending` / `RunReport::into_report` — project the engine's own
  `run::Ending`/`RunReport` onto these wire shapes. Every diagnostic renders
  here, runtime errors against the `SourceDb` and static ones against the
  `Source` the diagnostic carries
  ([[decisions/260902_static-diagnostics-render-at-the-seam|static-diagnostics-render-at-the-seam]]).
- `Severed` — why no further frame will cross: `Refused` / `Closed` /
  `Silent` / `Faulted`, `Display`ed as the sentence a front-end shows, and
  `code()` the stable name it is quoted by; the private `sever()` is
  first-cause-wins.
- `ProbeError` — `Rejected` (a program error: an unknown class, a probe
  mid-run) or `Severed`.
- `Host` trait — one run's host-facing surface: `surface`, `enquire`;
  `impl Host for ()` is the mute host.
- `Transport` trait — `dispatch`, `probe`, `control`, `events`, `severed`,
  `sever`, `detach`, `answer`, `set_deferred_sink`. Construction is attach, so
  the trait has no attach of its own; `sever` lets a front-end record a fault
  it observed, and on the wire also shuts the socket.
- `forbid_reentry` / `Dispatching` — the carrier-uniform reentrancy law: a
  thread-local registry of the transports this thread is dispatching on,
  entered by `dispatch_to_report` and checked by every typed reading and the
  identity carrier's `probe`, panicking with a didactic sentence.
- `dispatch_to_report` — mint a `DispatchId`, call `transport.dispatch`,
  drain `events()` to that dispatch's own `Report`, forwarding
  `Surface`/`Enquiry` to `host`; `Result<Report, Severed>`.
- `ControlSender` — `interrupt()`, `cancel(id)`, `terminate()` over one of two
  doors: `Door::Identity` applies the verb straight onto the engine's
  `Scopes`, `Door::Wire` writes a `Control` frame through the severance cell.
- `EventReceiver` — the front-end's single-drainer event queue; its `stash`
  hands back an event a probe's or a desk's pre-drain read past, in arrival
  order, rather than dropping it.
- `IdentityTransport` / `SessionLock` — the in-process carrier: an `Engine`
  behind a poison-recovering session lock, its `Scopes` and its `Nursery`
  held outside the lock so a `Control` and an adoption land mid-dispatch.
  Its public surface is `boot(installers, &attach)` — the same verdict as
  the wire's attach — and `adopt_parked(id, &grant)`, which takes a fork a
  run parked in this transport's nursery, narrows it through
  `SpawnGrant::narrow_onto` under this transport's installer, and answers a
  new transport over it. Nothing on it yields or accepts a `Shell`. Its
  `dispatch` lays `Rails` whose outlet is an in-process channel, whose desk
  is `IdentityDesk`, and whose fork arm is `Fork::Park` over its own nursery.
- `IdentityDesk` — the identity binding's `EnquiryDesk`: drains queued
  `Surface` events before calling `host.enquire`, so a handler can never
  outrun its own run's earlier output.
- `WireTransport` — the wire carrier's front-end half. `adopt(stream,
  Liveness)` drives an existing duplex stream; `attach(Attach)` writes the
  only legal first frame and only then starts the heartbeat;
  `await_attached()` blocks on the reader's `Attached`/`Refused` verdict or
  its own patience, counted in waits observed. `severed()` reads the cause.
- `spawn_wire_reader` / `spawn_heartbeat` — the reader severs *before*
  dropping `event_tx`, on every exit path, and severs `Faulted` on a frame
  only a front-end sends; the heartbeat pings on `Liveness::interval`,
  severs `Silent` once `Liveness::probes` pings go unanswered, and never
  takes the write lock on that path.
- `write_through` — the front-end door `WireTransport::write` and
  `ControlSender`'s wire arm both share: `wire::write_or_sever` recording a
  `Severed` cause.

## `core/src/protocol/reading.rs`

Every probe class, typed once for both ends.

- `Class` / `CLASSES` — the label table: `binding-count`,
  `leased-binding-count`, `largest-binding-bytes`, `env-var`, `cwd`, `home`,
  `builtin-names`, `path-bytes`, `workers`, `session-ended`,
  `completion-names`, `bindings`, `spine`, `bind-effects`, `last-chpwd`,
  `path-entries`; `worker-count` and `grant-depth` exist only under
  `test-util`. `reads_string` is the payload rule: the classes that read a
  name, a path, or a source text take a string, the rest take none.
- `answer` — the engine's answer against a `Shell`, refusing a non-variant,
  an unknown class, or a payload its class does not take, each by name.
- `report` / `unreport` — a probe's answer as the wire engine reports it,
  and back.
- `read` and one typed door per class — `cwd`, `home`, `env_var`,
  `builtin_names`, `path_bytes`, `binding_count`, `leased_binding_count`,
  `largest_binding_bytes`, `workers`, `session_ended`, `completion_names`,
  `bindings`, `spine`, `bind_effects`, `last_chpwd`, `path_entries` — each
  decoding through `Datum` and severing the transport `Faulted` on an answer
  outside its shape.
- `reading/rows.rs` — the rows the answers come in, rendered engine-side:
  `WorkerRow`, `BindingRow`/`HandleRow`, `CompletionNames`,
  `Spine`/`SpineStage`/`SpineError`, `BindEffect`, `PathEntry`.
- `reading/source.rs` — `spine` and `bind_effects`: a source text compiled
  against the live session, never run.
- `reading/fs.rs` — `tree_bytes` and `entries`: read-only metadata walks of
  the engine's own filesystem, under its cwd.

## `core/src/engine/wire.rs`

The wire carrier's engine half: a connection-lived engine process.

- `run_engine(installers)` — adopts fd 3 as the wire channel and calls
  `engine_session`.
- `engine_session` — takes a hatch seed first (`hatch::seed_from_env`),
  reads `Attach` (the only legal first frame), `restore_process_dirs` (the
  process-level half of an attach, which only a carrier owning its process
  performs), `Engine::boot`, writes `Attached`/`Refused`, moves the engine
  onto a worker thread, then runs the reader loop; returns the process exit
  code. The loop breaks on a `SessionEnd` — `Requested` (a `Detach` or the
  front-end's EOF) or `Corrupt` (a read error, a dead worker, silence past the
  deadline, or a frame only an engine sends) — so corruption cannot be
  reported as an end the front-end asked for; a wire fault demotes even a
  `Requested` end to `1`.
- The worker's `Rails` — an outlet writing `Frame::Event`, a
  `ChannelDeferredSink` writing `Frame::Session`, a `WireDesk`, and
  `Fork::Listen`.
- `WireDesk` / `Parks` — the wire engine's `EnquiryDesk`: writes
  `Event::Enquiry`, then parks on a oneshot registered under its `EnquiryId`
  until `Frame::Answer` fills it or the run's own cancel scope fires. A park
  that gave up has deregistered its sender, so a late answer is dropped.
- `Dispatch` / `Writing` — the one-run-or-probe rendezvous and the guard
  spanning an item's report write; claiming is the only way to mint a
  `Dispatch`, so "engine busy" is never raised without work in flight.
- `Patience` / `HOST_SILENCE_DEADLINE` — the engine's own read-silence and
  write-stall deadlines, armed once the first `Ping` arrives; production
  always runs `Patience::default`, a test gets a brisker one.
- The teardown settle — on any loop exit: `Scopes::end`,
  `hatch::teardown_hatched()`, then poll the busy and writing flags under
  `SETTLE_TIMEOUT`/`SETTLE_POLL` before exiting, so no run is abandoned
  mid-report.

## `core/src/wire.rs`

- `WireStream` — `UnixStream` on Unix, `TcpStream` on Windows: std's owner of
  a *connected stream socket*, never a statement about address family
  (`vm-manager` hands back `AF_VSOCK`/`AF_HYPERV` sockets through the same
  type).
- `WireChannel` — length-prefixed JSON framing (`subprocess_codec`) over one
  `WireStream`; `pair()` (test-only: a socketpair on Unix, a loopback accept
  on Windows), `from_stream`, `try_clone`.
- `poll_readable` — wait for a frame or a timeout without blocking inside
  `read_frame`; how `engine_session` notices a silent front-end with no
  dedicated thread.
- `set_write_deadline` — bounds every `write_frame` on every clone of the
  channel (`SO_SNDTIMEO` lives on the shared file description), turning a
  stalled write into the same fatal error a severed pipe already gives.
- `write_or_sever` — the severance law itself, enforced once for both doors
  (`protocol::write_through`, `engine_write`, which differ only in what they
  record): a failed write is recorded *and* the channel shut down before the
  lock is released, so nothing appends a frame after a truncated one and no
  window leaves the record calling an already-shut socket healthy.

## `core/src/hatch.rs`

A wire-seat spawn is one exchange: the guest binds an ephemeral port for one
spawn and the host dials in — see [[design/engine-protocol|engine-protocol]]'s
hatch section for the why.

- `listen_for_hatch` — waits on a caller-bound listening descriptor for the
  one dial that hatches a child, checking the dialler's eight token bytes. It
  scrubs the shell it is handed (`Shell::fork_scrubbed`) and packs the fork
  into an `EngineSeed` on the caller's own thread, so every path onto the
  seed wire is a scrubbed fork ([[map/core/transport|transport]]).
- `hatch_over` — re-execs this binary (`--engine` in production) with the
  dialled connection on fd 3 and a seed socketpair named by
  `RAL_ENGINE_SEED_FD`; writes the framed seed while the child drains it, and
  answers `HATCH_ACK` only once `spawn()` has returned and the seed has
  crossed.
- `HATCH_ACK` (defined in `protocol.rs`, written from here) — the byte that
  says the child exists and already holds its whole seed; neither it nor the
  token is a `Frame`, so a hatch never touches `PROTOCOL_VERSION`.
- `seed_from_env` — the child's own take: reads `RAL_ENGINE_SEED_FD`,
  striking the var as it takes the fd, before the engine waits for `Attach`;
  `Engine::boot` applies what it took (`EngineSeed::apply`).
- `HATCHED` / `teardown_hatched` / `sweep_hatched` — the process-global table
  of spawned-but-unreaped hatch children, swept by `waitpid` at the next
  hatch and again at engine teardown (a hatched child closes its seed channel
  on hydration, not on death, so only `waitpid` tells running from gone).

## `core/src/spawn_grant.rs`

- `SpawnGrant` — `Inherit`, `Base(name)`, or `Restrict(record)` carried
  unfrozen. `layer` resolves it against a cwd and home; `narrow_onto` pushes
  that layer onto a shell as a session frame, frozen against the shell's own
  cwd — the one layering step an adopted identity fork
  (`IdentityTransport::adopt_parked`) and a hatched seed (`EngineSeed::apply`)
  share, so neither seat holds a narrowing decision of its own
  ([[decisions/260922_a-spawn-is-one-layer|a-spawn-is-one-layer]]). A
  `` `restrict `` record crosses undecoded and is decoded there, which keeps
  `Capabilities`' resolved-by-construction invariant true of every value that
  ever exists.
- `GrantNarrower` — `fn(&str, &Path) -> Result<Capabilities, String>`, a field
  of `EngineInstaller` rather than a registered hook: core has no base-tag
  lexicon of its own, so a `` `base `` arm is always resolved by the host
  that boots the engine (`exarch::policy::base_layer`), and the policy is
  demanded of every host that boots an engine instead of left in a slot one
  could forget to fill.

The seed a hatch carries is `EngineSeed` — [[map/core/transport|transport]]'s
`core/src/engine_seed.rs` section.

## See also

[[design/engine-protocol|engine-protocol]] (the why — one engine and two
carriers, the channel table, the laws of an enquiry, liveness and severance),
[[map/core/transport|transport]] (`subprocess_codec`'s framing, `Datum`,
`EngineSeed`), [[map/exarch/agent|exarch / agent]] (the seat, the wire-seat
spawn that dials a hatch), [[map/repl/loop|repl / loop]] (the REPL as an
identity front-end), [[map/synod|synod]] (`WireTransport::adopt` over a
guest VM's virtual socket).
