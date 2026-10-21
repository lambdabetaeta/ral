---
generated_at_commit: 61c288f
generated_at_date: 2026-10-21
covers_paths: [core/src/protocol.rs, core/src/protocol/, core/src/carrier.rs, core/src/carrier/, core/src/engine.rs, core/src/engine/, core/src/run/report.rs, core/src/seed.rs, core/src/guard/grant.rs]
---

# Map: core / engine protocol

**`core/src/engine.rs` is the one engine; `core/src/protocol.rs` is the frame
algebra and nothing that carries it — `protocol/probe.rs` its probe
vocabulary, `protocol/channel.rs` the duplex byte channel and the one
liveness law; `core/src/carrier.rs` and `carrier/` are the two carriers that
interpret the algebra — the identity transport in process, the wire
transport's front-end half — with `carrier/wire/serve.rs` the wire's engine
half; `core/src/run/report.rs` projects a run onto the algebra; a wire-seat
child is made by the host's factory and born from an `Attach` carrying its
parent's seal, so core holds no spawn machinery for it.** The why is
[[design/engine-protocol|engine-protocol]]; this page only points at symbols.

## `core/src/engine.rs`

- `EngineInstaller { tag, runtime, root, narrow }` — one compiled-in
  installer, in two halves. `runtime: fn(&EngineSpec) -> Runtime`
  is the process half (Σ, builtins, docs; a root and a child engine each
  derive their own, an in-process child shares its parent's); `root:
  fn(&Root, &mut Shell) -> Result<(), String>` is the root-only recipe
  (libraries, hints, seeds, ledgers), which refuses in its own words; `narrow: GrantNarrower` is the policy a seeded or adopted
  child is held to. Only `tag` crosses `Attach`, never the function.
- `Runtime`: what the binary supplies, and the only place builtins are
  installed (`install_builtins`, `install_captured_builtins`); a `Shell` is
  built over one and cannot hand it back.
- `Engine` — one session's engine: the `Shell`, its `Arc<Runtime>`, its
  `Scopes`, the installer it was born from. `Engine::boot(installers, &attach)` is the one
  boot both carriers call: `resolve_installer` (version check, then tag
  lookup), the guest jail under `RAL_GUEST` (whose value is the jail's cgroup root), the installer's `runtime`, then
  by `attach.origin`: `Origin::Root(root)` is `Shell::root`, the root recipe,
  `seed_cwd` and the root's env; `Origin::Child(sealed)` is `Shell::child_of(runtime,
  sealed.unseal()?.decode(&runtime)?, narrow)`, and no recipe runs. There is no
  `Keep`: the attach is the birth, and the scratch is the host's, named in a
  root's env.
  `Engine::run` runs one dispatch under the view `Scopes::open` hands it;
  `Scopes` keeps the node, its one writer;
  `Engine::probe` (`engine/probe.rs`) answers one `Probe`.
- `Rails` — one dispatch's host-facing rails as its carrier lays them: the
  `Outlet` its events leave by, the deferred sink, the desk, the `Fork` arm.
  The carriers differ in these and nowhere else.
- `Scopes` — what stops an engine's runs, shared with whichever thread
  carries `Control`: the dispatch scope root, the durable root, and a slot
  holding the current dispatch and a foretold strike with its cause. `open`
  mints a dispatch's scope ahead of its frame (struck already if a strike
  foretold it); `apply` is `Control`'s one meaning — `Interrupt(id)` and
  `Cancel(id)` strike the current dispatch if it bears `id` and foretell it
  otherwise, as `Interrupted` and `Cancelled`, and `Terminate` cancels the
  durable root.

## `core/src/protocol.rs`

The frame algebra. Everything on it is a wire type; nothing here carries.

- `PROTOCOL_VERSION` (currently 27) — checked at `Attach`; a mismatch refuses.
- `check_media` — the same number compared at *build* time, against the
  `proto_version=` line `vm-image/build-boot.sh` records for the engine in the
  guest media; `synod/examples/check-media.rs`, tauri's `beforeBuildCommand`,
  refuses to package media that disagrees.
- `Frame` — the whole wire enum: `Attach`, `Detach`, `Dispatch`, `Probe`,
  `Event`, `Session`, `Control`, `Answer`, `Ping`, `Pong`; `Frame::kind`
  names a variant for the diagnostic about a frame out of place.
- `Attach { engine: EngineSpec, origin: Origin }` — what every engine is born
  from, under either carrier. `EngineSpec` names the engine: `proto_version`,
  `installer`, `terminal`, `home`. `Origin` is `Root(Root { cwd, env, config })`
  (`env`: per-session variables the host seeds; `config`: the installer's own
  settings, an `FOValue` its recipe decodes) or `Child(Sealed)` (a child's
  parent, sealed, carrying its own cwd and env). `home` and `cwd` are
  `path::EnginePath`s (`path/namespace.rs`): a path on the engine's machine,
  named in the namespace the front-end can name it in — `Host(PathBuf)` for an
  engine sharing the front-end's computer (in process, or a child process),
  `Guest(GuestPath)` for one inside a Posix guest, a `/`-separated string that
  a Windows front-end has no `PathBuf` for. The engine reads either as its own
  through `EnginePath::native`, crate-private, since only the engine's side
  has the OS a guest path is native to. `Attach::new` is a plain root's, at
  this build's version; `Attach::root` pairs an `EngineSpec` with a `Root`, whose
  `with_env` and `with_config` edit it; `Attach::child` pairs an `EngineSpec`
  with a seal; `with_terminal` edits either.
- `Run` / `Program` — one dispatch's payload: the policy fields plus
  `Program::Source`/`Program::Hook`. `Run`'s field types are the run regime,
  held by session state, and live below the wire: `RunIo` and `RunStdin`
  (`io/regime.rs`), `Captured` (`io/sink.rs`) and `RequestedTerminalAccess`
  (`process/lease.rs`); `ral_core` re-exports them at the root.
- `Event` — engine→front-end, inside a dispatch's window: `Surface`,
  `Enquiry`, `Reading`, `Report`. Every surfaced value is a tagged variant; an
  audit observation rides as `` `observed <record> `` (`Observation::to_surface`).
  `Reading(Result<FOValue, String>)` answers a `Frame::Probe` under its
  `DispatchId`: the probe's own event, not a run that ended.
- `SessionEvent` — engine→front-end with no dispatch to ride: `Attached` /
  `Refused(String)` (the attach verdict) and `DeferredSurface(Vec<FOValue>)`
  (a worker's batch, and a watched worker's lines).
- `EnquiryError` — message plus status, the wire shape of a refused enquiry;
  `no_desk()` is the fixed wording for a host with nothing to answer, and
  `From<EnquiryError> for Error` the one way a refusal becomes the error
  both carriers' desks raise.
- `Control` — `Interrupt(DispatchId)`, `Cancel(DispatchId)`, `Terminate`, `Abort` (a
  `Terminate` whose cause is `RootAbort`); applied by `Scopes::apply` under
  both carriers.
- `Report` / `Ending` / `FailureStatus` — the terminal frame:
  `Static { rendered, status }` (a refusal: the run never reached evaluation, so
  no command ran and nothing was bound), `Ran { ending, captured, trail }`, or
  `Faulted { rendered, status }` (the engine could not report the run: a panic
  in the run or in the report plumbing, or a report the fuse would not pass; what
  the run did is unknown, so a host reads it as neither a refusal nor an ending);
  `Ending` has no process-stop arm — ral does not suspend — so every ending is
  `Settled`/`Raised`/`Stopped`/`Unreturnable`/`Exited`. `Stopped` is a
  cancellation stop that reached the top, a top-level effect like `Exited`,
  carrying its cause and its rendered diagnostic (empty for the closed-stdout
  stop, which prints nothing). `Unreturnable` is a
  run that settled on a value the wire cannot carry (a handle, a block, a
  function), rendered with a hint and reported as status 1. `Raised` and
  `Unreturnable` carry their error's `record` — what `try`
  hands its handler, and what `--audit` builds its `` `err `` outcome from —
  and the two failing arms a `FailureStatus`, never a bare `i32`: it clamps
  into `1..=255` on the way in and on the way back off the wire, so no ending
  that failed can be reported with the code that means success.

## `core/src/run/report.rs`

The projection is the run's. `RunReport::into_report` and `render_ending`
project the engine's own `run::Ending`/`RunReport` onto the wire shapes above:
every diagnostic renders here, runtime errors against the `SourceDb` and static
ones against the `Source` the diagnostic carries
([[decisions/260902_static-diagnostics-render-at-the-seam|static-diagnostics-render-at-the-seam]]).
`not_data` / `unreturnable` word a value the wire cannot carry; `record_of`
is the record `try` hands its handler. A panicked run is a
`RunReport::Faulted`, projected to a `Report::Faulted` on its error's status.
`Report::refused` is the engine's own refusal before the run reached
evaluation (a busy engine, a dispatch that could not be sent) shaped as a
`Static`, so a host never has to tell it from a run's own failure;
`Report::faulted` is a fault outside the run's report (a panicked worker, a
report the fuse would not pass) shaped as a `Faulted`. Both render the
spanless one-liner through the one rule `StaticDiagnostics::Host` uses. The
two seam test modules live here.

## `core/src/protocol/probe.rs`

The probe vocabulary, typed once for both ends.

- `Probe` — the payload of `Frame::Probe`: `BindingCount`,
  `LeasedBindingCount`, `LargestBindingBytes`, `EnvVar(String)`, `Cwd`,
  `Home`, `BuiltinNames`, `PathBytes(PathBuf)`, `Workers`, `SessionEnded`,
  `CompletionNames`, `Bindings`, `PathEntries(PathBuf)`, `Grants`;
  `WorkerCount` exists only under `test-util`. Each variant states what it
  takes, so a payload its probe does not take is not representable.
- The rows the answers come in, rendered engine-side, each a `record!`:
  `WorkerRow`, `BindingRow`/`HandleRow`, `CompletionNames`, `PathEntry`.
  `Grants` answers the session's `GrantStack` (no run's ceiling) as opaque
  bytes in the JSON codec `Run.caps` already rides, as a sealed seed does.

## `core/src/protocol/channel.rs`

- `WireStream` — `UnixStream` on Unix, `TcpStream` on Windows: std's owner of
  a *connected stream socket*, never a statement about address family
  (`vm-manager` hands back `AF_VSOCK`/`AF_HYPERV` sockets through the same
  type).
- `WireChannel` — length-prefixed JSON framing (`frame.rs`) over one
  `WireStream`; `pair()` (test-only: a socketpair on Unix, a loopback accept
  on Windows), `from_stream`, `try_clone`.
- `poll_readable` (Unix) — wait for a frame or a timeout, by
  `rustix::event::poll` against a `Timespec` deadline, without blocking inside
  `read_frame`; how `engine_session` notices a silent front-end with no
  dedicated thread. Windows has no engine half, so no twin.
- `set_write_deadline` — bounds every `put` on every clone of the
  channel (`SO_SNDTIMEO` lives on the shared file description), turning a
  stalled write into the same fatal error a severed pipe already gives.
- `write_or_sever` / `WriteError` — the severance law itself, enforced once
  for both doors (`carrier::write_through`, `engine_write`, which differ only
  in what they record): the frame is encoded before the lock is taken, and a
  frame the fuse refuses is `Unencodable`, the wire untouched, for its writer
  to answer where it stands; a failed write is `Severed`, recorded *and* the
  channel shut down before the lock is released, so nothing appends a frame
  after a truncated one and no window leaves the record calling an
  already-shut socket healthy. Only failed I/O severs.
- `Liveness` / `ticks` — how briskly a front-end pings and how much silence it
  tolerates; the deadline is judged as a count of unanswered pings
  (`Liveness::probes`), never as elapsed clock. `HOST_SILENCE_DEADLINE`, the
  engine's armed read silence, is six default ping intervals, written beside
  `Liveness::default`.

## `core/src/carrier.rs`

The two interpretations of the algebra, and what they share.

- `Severed` — why no further frame will cross: `Refused` / `Closed` /
  `Silent` / `Faulted`, `Display`ed as the sentence a front-end shows, and
  `code()` the stable name it is quoted by; the private `sever()` is
  first-cause-wins.
- `ProbeError` — `Rejected` (a probe mid-run, a panicked reading, or one the
  fuse refused before it was sent) or `Severed`.
- `Host` trait — one run's host-facing surface: `surface`, `enquire`;
  `impl Host for ()` is the mute host.
- `Ends` — the front-end state both carriers hold in one value: the
  `ControlSender`, the `EventReceiver`, the severance cell and the session
  sink. A carrier supplies `Transport::ends` and the verbs that differ in
  carriage (`dispatch`, `probe`, `detach`, `answer`); `control`, `events`,
  `severed`, `sever` and `set_deferred_sink` are provided. The wire carrier
  overrides `sever` only to shut the socket too.
- `Transport` trait — the front-end side of the protocol. Construction is
  attach, so the trait has no attach of its own; `sever` lets a front-end
  record a fault it observed. It also provides the host's typed probe doors,
  one per `Probe`: `cwd`, `home`, `env_var`, `builtin_names`, `path_bytes`,
  `binding_count`, `leased_binding_count`, `largest_binding_bytes`,
  `workers`, `session_ended`, `completion_names`, `bindings`,
  `path_entries`, each decoding through `Datum` and severing the transport
  `Faulted` on an answer outside its shape (`read`).
- `forbid_reentry` / `Dispatching` — the carrier-uniform reentrancy law: a
  thread-local registry of the transports this thread is dispatching on,
  entered by `dispatch_to_report` and checked by every typed reading and the
  identity carrier's `probe`, panicking with a didactic sentence.
- `dispatch_to_report` — mint a `DispatchId`, aim the control door at it,
  call `transport.dispatch`, drain `events()` to that dispatch's own `Report`,
  forwarding `Surface`/`Enquiry` to `host`; `Result<Report, Severed>`.
- `ControlSender` — `interrupt()`, `cancel(id)`, `terminate()` over one of two
  doors: `Door::Identity` applies the verb straight onto the engine's
  `Scopes`, `Door::Wire` writes a `Control` frame through the severance cell.
  `aim(id)` records the dispatch in flight with the `interrupt_epoch` it was
  sent at, until its `Aimed` guard drops; `interrupt()` names that dispatch,
  and `hear(ambient)` names it only if the epoch has moved since, sending
  `Control::ending` for a root cause; `forward_signals()` holds a process
  listener forwarding through that door.
- `EventReceiver` — the front-end's single-drainer event queue; its `stash`
  hands back an event a probe's or a desk's pre-drain read past, in arrival
  order, rather than dropping it.
- `write_through` — the front-end door `WireTransport::write` and
  `ControlSender`'s wire arm both share: `write_or_sever` recording a
  `Severed` cause.

### `carrier/identity.rs`

- `IdentityTransport` / `SessionLock` — the in-process carrier: an `Engine`
  behind a poison-recovering session lock, its `Scopes` and its `Nursery`
  held outside the lock so a `Control` and an adoption land mid-dispatch.
  Its public surface is `boot(installers, &attach)` — the same verdict as
  the wire's attach — and `adopt_parked(id)`, which takes the `Bequest` a
  run parked in this transport's nursery, builds `Shell::child_of` the
  engine's `Arc<Runtime>`, under this transport's installer's
  `narrow`, and answers a new transport over it. `over` takes the
  engine's `Arc<Runtime>` before the session lock and keeps it. Nothing on it yields or accepts a `Shell`. Its
  `dispatch` lays `Rails` whose outlet is an in-process channel, whose desk
  is `IdentityDesk`, and whose fork arm is `Fork::Park` over its own nursery.
- `IdentityDesk` — the identity binding's `EnquiryDesk`: drains queued
  `Surface` events before calling `host.enquire`, so a handler can never
  outrun its own run's earlier output.

### `carrier/wire/front.rs`

- `WireTransport` — the wire carrier's front-end half. `adopt(stream,
  Liveness)` drives an existing duplex stream; `attach(Attach)` writes the
  only legal first frame and only then starts the heartbeat;
  `await_attached()` blocks on the reader's `Attached`/`Refused` verdict or
  its own patience, counted in waits observed. A probe writes `Frame::Probe`
  and drains the event queue to the `Event::Reading` under its id, stashing
  what it reads past. A frame the fuse refuses is answered where its writer
  stands: a `Dispatch` by a `Report::refused` stashed on its own event
  stream, a `Probe` by `ProbeError::Rejected`, an `Answer` by an `Err` answer;
  an `Attach`, with no session yet to refuse into, severs `Refused`.
- `spawn_wire_reader` / `spawn_heartbeat` — the reader severs *before*
  dropping `event_tx`, on every exit path, and severs `Faulted` on a frame
  only a front-end sends; the heartbeat pings on `Liveness::interval`,
  severs `Silent` once `Liveness::probes` pings go unanswered, and never
  takes the write lock on that path.

### `carrier/wire/serve.rs`

The wire carrier's engine half: a connection-lived engine process.

- `run_engine(installers)` — adopts fd 3 as the wire channel and calls
  `engine_session`.
- `engine_session` — reads `Attach` (the only legal first frame), `restore_process_dirs` (the
  process-level half of an attach, which only a carrier owning its process
  performs: a root's cwd, every engine's `HOME`, each read `EnginePath::native`; a child restores no cwd), `Engine::boot`, writes `Attached`/`Refused`, moves the engine
  onto a worker thread, then runs the reader loop; returns the process exit
  code. The loop breaks on a `SessionEnd` — `Requested` (a `Detach` or the
  front-end's EOF) or `Corrupt` (a read error, a dead worker, silence past the
  deadline, or a frame only an engine sends) — so corruption cannot be
  reported as an end the front-end asked for; a wire fault demotes even a
  `Requested` end to `1`.
- `engine_write` / `engine_send` — every engine-side write. A `Report` or
  `Reading` the fuse refuses is replaced by a `fault`; any other refusal
  goes back to its writer, `engine_send` diagnosing and dropping a frame with
  no writer to refuse into (a session frame, a `Pong`).
- The worker's `Rails` — an outlet writing `Frame::Event` through
  `engine_write` (an `Outlet` answers `Result<(), WriteError>`), a
  `ChannelDeferredSink` writing `Frame::Session`, a `WireDesk`, and
  `Fork::Seal`.
- `WorkItem` / `refusal` / `fault` — what the worker is handed, a run or a probe, and
  the refusal or fault in the shape each waits on: a busy engine's run ends in
  a `Report::refused`, a panicked worker's or fuse-refused one in a
  `Report::faulted`, a probe in an `Event::Reading(Err(..))` either way.
- `WireDesk` / `Parks` — the wire engine's `EnquiryDesk`: writes
  `Event::Enquiry` (a refused or severed write is its own `Err`), then parks on a oneshot registered under its `EnquiryId`
  until `Frame::Answer` fills it or the run's own cancel scope fires. A park
  that gave up has deregistered its sender, so a late answer is dropped.
- `Dispatch` / `Writing` — the one-run-or-probe rendezvous and the guard
  spanning an item's report write; claiming is the only way to mint a
  `Dispatch`, so "engine busy" is never raised without work in flight.
- `Patience` — the engine's own read-silence and write-stall deadlines, armed
  once the first `Ping` arrives (both `HOST_SILENCE_DEADLINE` in production,
  a test gets a brisker pair).
- The teardown settle — on any loop exit: `Scopes::end`, then poll the busy and writing flags under
  `SETTLE_TIMEOUT`/`SETTLE_POLL` before exiting, so no run is abandoned
  mid-report.

### `carrier/testkit.rs`

Core's own test engines, booted as every engine is (`BARE`, `boot`, `eval`).

## `core/src/engine/probe.rs`

The engine's answers: `Engine::probe` reads one `Probe` off the shell
(`answer`), rendering rows engine-side — never a live handle.
`probe/fs.rs` — `entries` (and `path::tree::bytes`, behind `Probe::PathBytes`): read-only
metadata walks of the engine's own filesystem, under its cwd.

## `core/src/guard/grant.rs`

- `SpawnGrant` — `Inherit`, `Base(name)`, or `Restrict(record)` carried
  unfrozen. `layer` resolves it against a `FreezeCtx`; `narrow_onto` pushes
  that layer onto a shell as a session frame, frozen against the shell's own
  cwd — the one layering step, which `Shell::child_of` takes for an adopted
  identity fork (`IdentityTransport::adopt_parked`) and a sealed child alike, so neither seat holds a narrowing decision of its own
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

The seal a child's `Attach` carries is an `EngineSeed` in bytes —
[[map/core/transport|transport]]'s `core/src/seed.rs` section.

## See also

[[design/engine-protocol|engine-protocol]] (the why — one engine and two
carriers, the channel table, the laws of an enquiry, liveness and severance),
[[map/core/transport|transport]] (`frame.rs`'s framing, `Datum`,
`seal`/`Sealed`), [[map/exarch/agent|exarch / agent]] (the seat, the wire-seat
spawn that dials the factory), [[map/repl/loop|repl / loop]] (the REPL as an
identity front-end), [[map/synod|synod]] (`WireTransport::adopt` over a
guest VM's virtual socket).
