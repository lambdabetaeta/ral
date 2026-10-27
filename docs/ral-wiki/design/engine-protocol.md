---
generated_at_commit: 6281520c58de152afc0b1ed99b94ade98c21551e
---

# The engine protocol

**A front-end and a `ral` engine share exactly one duplex stream, sorted into
channels by direction, by whether a message is answered, and by the scope of
the handler that receives it; the stream is realised by exactly two bindings —
a direct call in one address space, or a codec across a socket, a re-exec'd
process, or a vsock into a guest.** Nothing exists in one binding and not the
other: *identity* is the trivial instantiation of the algebra *wire* encodes,
never a second design. A channel is a scope, never a purpose: every host
facility yet to be invented is a class inside an existing channel's payload,
decoded by one more arm.

## One engine, two carriers

The claim is true by construction, not by discipline: there is one engine,
and a binding is only the carriage around it.

- **One engine.** `Engine` is the engine side of one session — the `Shell`,
  the scopes that stop its runs, and the installer it was born from — and it
  answers a run and a probe. The identity carrier holds one behind its
  session lock and calls it on the dispatching thread; the wire carrier runs
  one on a worker thread behind a frame loop. They differ only in the
  `Rails` each lays for a dispatch: where its events go, who answers its
  enquiries, and how a session it forks is adopted.
- **One construction.** Every engine is born from an `EngineInstaller` and
  an `Attach`, and `Engine::boot` does it once for both carriers: resolve
  the installer (version first, then tag), derive its `Runtime` once from
  the attach's `EngineSpec` (builtins live on it and nowhere else), then
  either `Shell::root` and the root recipe over the `Origin::Root` (seating
  its cwd and env), then re-hold and install its `Root::grants` as permanent
  session layers, oldest first; or `Shell::child_of` over the `Bequest` a
  `Origin::Child` seal decodes to; face the process's signals, jail a guest.
  The identity carrier constructs its child from the engine's `Arc<Runtime>`
  and the parked bequest, so a fork too is the trivial instantiation of what
  the wire encodes. Construction is attach: `IdentityTransport::boot`
  returns the verdict the wire's `await_attached` does — `Severed::Refused`,
  in the refusing step's own words — so a transport in hand is one its
  engine accepted.
- **No front-end holds a `Shell`.** Nothing public on `IdentityTransport`
  yields a `Shell` or accepts one. A front-end reaches its engine only through
  `Transport` — dispatch, probe, control — so a facility it needs must exist
  in the protocol, and therefore exists under the wire as well. Tests reach
  engine state through `ral_core::test_access` (feature `test-util`) or a
  probe, never a public method: `test_access::workers` reads through
  `IdentityTransport::inspect`, itself `test-util`-only, and core's own tests
  boot their engines as every engine is booted, through `engine::testkit`.
- **The carrier adopts the fork.** A run that forks its session parks it or
  listens for it by the carrier's `Fork` arm, never by a `Host` method; both
  arms push the child's one grant layer through the same step (*Two
  bindings*, below).
- **One meaning per verb.** `Control`, reentrancy, and every probe class are
  defined once, and each carrier applies the same definition.

## Channels: direction, answer, scope

|                   | answered (call)                                                    | one-way (notice)                                              |
| ----------------- | ------------------------------------------------------------------- | --------------------------------------------------------------- |
| **host → engine** | `Dispatch → Report` — one whole run: clocked, walled, cancel-scoped  | `Control` — interrupt, cancel, terminate, abort, out-of-band     |
| **engine → host** | `Enquiry → Answer` — nested inside a run, no clock of its own       | `Surface` — a run's live values, ordered before its Report; a worker's batch, on the session |

Each direction owns one answered channel and one one-way channel, and the
engine's one-way traffic splits once more by scope (below). The remaining
asymmetries are semantic, not accidental, and are never smoothed into a fake
mirror pair:

- *Mood.* `Control` is imperative while `Surface` is indicative: this
  happened, this is now true. `Control` has four verbs, each meaning one
  thing under either carrier (`Scopes::apply`): `Interrupt(id)` unwinds that
  dispatch as a Ctrl-C would and `Cancel(id)` as a targeted teardown, either
  one even before the dispatch has arrived, and a stale id strikes nothing;
  `Terminate` cancels the durable root — every run and every worker,
  for good; `Abort` is `Terminate` as Ctrl-`\` asks it, cause `RootAbort`. No
  engine folds its process's signals: its front-end forwards them as these
  verbs (`ControlSender::forward_signals`), aiming an interrupt only at a
  dispatch it sent before the interrupt was raised, so a Ctrl-C struck between
  runs reaches none. A wire engine hears only its own process's shutdown. So a
  process hosting many engines — exarch, a test binary — never has one signal
  reach them all.
- *Ordering.* `Surface` is sequenced within its dispatch, strictly before
  that dispatch's `Report` — what makes the transcript truthful. `Control`
  is deliberately unsequenced: it must race past an in-flight dispatch, or
  cancel could not work. Naming its dispatch is what makes the race harmless:
  a verb that arrives late finds a stale id.
- *Clock.* `Dispatch` opens the turn's clock and holds the session's single
  worker rendezvous; `Enquiry` happens inside a dispatch and inherits all of
  it — no wall of its own, and a `Control::Cancel` must wake an engine
  parked on an unanswered enquiry.

A further frame, `Probe`, rides the `Dispatch → Report` rail rather than
opening a channel of its own (see *Probes*, below).

## Two scopes of `Surface`

**`Surface` is two channels, because a producer can outlive the dispatch that
started it.** A surfaced value is handled by the innermost host handler whose
lifetime contains its producer:

- *Dispatch scope* — `Event::Surface`: a running dispatch's values, rendered as
  they arrive and ordered before its `Report`.
- *Session scope* — `SessionEvent::DeferredSurface`: a detached worker's values,
  delivered to the host's `DeferredSink` as one batch when the worker settles,
  closed by its `` `done ``, or a `watch` line as it arrives; rendered at the
  host's next boundary. A worker holds no live sink and no desk, so it cannot
  speak on a dispatch-scoped channel at all
  ([[decisions/260622_surface-delivers-itself|surface-delivers-itself]]).

Both carry the same vocabulary and meet the same decoder; only the scope
differs.

## The channels as asynchronous effects

**Read through Ahman and Pretnar's asynchronous effects, engine and host are two
processes in parallel, exchanging operations rather than calls
([[related/asynchronous-effects|asynchronous-effects]]).**

- `Surface` is an outgoing *signal*: issued, never answered.
- `Control` is an incoming *interrupt*, arriving at any point of a run; its
  handlers are the runtime's (`Scopes::apply`), never the program's.
- A `spawn` handle is a *promise*, and `await` its eliminator.
- `Enquiry → Answer` is a synchronous operation — in Æff a signal, a promise of
  the reply, and an `await` — fused into one primitive, so that it stays inside
  its dispatch's window, ordered before the `Report`.
- The session scope has no Æff counterpart: an Æff signal propagates at once,
  while a worker's waits for its scope's boundary — as a kernel makes a pending
  signal visible only on a return to user mode.

## Payloads: first-order or it does not cross

Every *open* payload position — a surfaced value, an enquiry and its answer,
a probe's reading and its answer, a hook's arguments, a settled run's value
— carries `FOValue`: a serialisable *first-order* ral value — unit, bool,
number, string, bytes, and lists/maps/variants thereof, data all the way
down, its extension slot uninhabited by construction. The envelope's own
fields (`Run`, `Ending`, `Control`, `Attach`) are closed Rust types, and
nothing in either carries an fd, a handle, a closure, or a capability beyond
the resolved grants a `Root` or a sealed `Child` already carries. `Attach`
is `{ engine: EngineSpec, origin: Root | Child(Sealed) }` and carries only
what an engine reads. `EngineSpec` names the engine — protocol version,
installer tag, terminal state, home. A `Root` is the session a root starts
from — its cwd, the host's per-session env, `grants` installed after the
recipe, and `config`, the installer's own settings as an `FOValue` its
recipe decodes and core never reads. A `Child` is a sealed bequest the
engine alone reads, carrying its own cwd and env. `SerialValue` — the
closure-capable sibling that carries a forked session into a sealed child
engine (*The factory*, below) — is a different type for a different domain;
it never reaches the protocol, and the host never reads the bytes it travels
as. That is the **opacity law**: a payload the host relays but never reads
crosses as bytes; its content is a contract between the engines at its two
ends, and the spawner is what makes those ends one binary
([[decisions/261013_the-seed-rides-the-protocol-sealed|the-seed-rides-the-protocol-sealed]]).

The **envelope** — `Frame`, `Event`, `SessionEvent`, correlation ids,
`Attach`'s handshake — stays a closed set of Rust types behind
`PROTOCOL_VERSION`, checked at `Attach` and refused loudly on mismatch. This
split is the design: the envelope must be exhaustive-matchable and
version-gated, because an envelope change *is* a protocol change; the
payload must be open, because operations keep arriving. **The extension
law**, stated once for every channel: a new facility is a new class — a
`FOValue::Variant` label — on an existing channel, plus a decoder arm at the
receiving end, never a new channel or frame family. The law binds a second
level too: a class that nests tags beneath it (`` exarch-agents `list ``,
`` exarch-schedules `add ``) draws the same loud error for an unrecognised *tag* as
for an unrecognised class — nesting must not open a silent hole beneath the
rule it was introduced under. An unrecognised enquiry class or tag answers
`Err` naming it; an unrecognised surface class is dropped with a note, never
silently. Every surface class is a label, an audit observation included
(`` `observed ``), so a host dispatches on the tag alone and never on a
value's shape.

**A payload is typed once, for both ends.** It is an `FOValue` on the wire
and a Rust type on either side of it: `Datum` (`ral_core::data::datum`) is
the one first-order codec — `encode`, and a strict `decode` naming whatever
arrived ill-shaped — and `record!` derives it for a record whose keys must be
exactly its own, answering an unknown key with the one it most likely meant
(`variant!` and `label!` are its siblings for payload enums and enums of bare tags).
Probe classes, exarch's enquiries, the REPL's enquiries and an installer's
`Root.config` all cross through it, so a builtin and the desk behind it decode
with one function and refuse in one wording.

**The frame codec has no depth cap.** Frames are length-prefixed JSON, and
`serde_stacker` grows both encode and decode onto the heap, so a frame nests
as deep as the data it carries and only the frame-length fuse bounds it;
`FOValue` is externally tagged. A frame that decodes but is out of protocol —
one only the other side ever sends — is a breach, not noise: the front-end
severs `Faulted`, and the wire engine ends the session `Corrupt`.

**The desk's decode is a trust boundary, not a duplicate check.** Under the
wire the engine and its shell run inside a guest and the desk runs on the
host, so a guest can send whatever it likes regardless of what its own builtin
already validated. Both ends re-validate: the builtin refuses early for a fast,
legible diagnostic; the desk refuses again because a registry's own
admission check (naming, uniqueness) must hold independent of which path
reached it.

**The guest's end ships in media built earlier.** The warrant between ral and
its confined child is one pinned binary at both ends, so a changed encoding
changes its version string and no old form is decoded. The engine is different:
the guest end lives in the VM image, built before the host. So any change to a
payload's shape (an `ExecGrant`'s allowance and exclusion sets, verb
sets as sequences) bumps `PROTOCOL_VERSION` and rebuilds the guest media, whose
manifest the host compares before packaging. Without the bump stale media passes
the packaging check and the attach handshake and fails decoding the first
dispatch. **`PROTOCOL_VERSION` is 33.** `Capabilities` carries an authored
`ExecGrant` as `every`, `allow` and `deny`: the two sets encode as sequences,
omitted sets default to empty, and unknown fields are refused. A key may occur
in both sets. Rebuild guest media with `just guest-boot`. A `Fragment` never
crosses: it lives only between loading and `extend`.

## The laws of an enquiry

- **Containment.** An enquiry lives inside its run: same wall, same
  foreground cancel scope, no clock of its own. A worker is built
  with no desk at all — a worker outlives its spawning run's `Report`, so one
  that could enquire would answer into a `Report` window already closed.
- **Cancel wakes a park.** `Control::Cancel` must reach an engine parked on
  an unanswered enquiry; the wire engine's park polls the run's cancel cause
  between bounded `mpsc` `recv_timeout` waits on its answer channel, so
  cancel and answer race safely.
- **Correlation from day one.** `EnquiryId`, fresh per enquiry, rides
  inside `Frame::Event(DispatchId, Event::Enquiry(EnquiryId, _))` on the way
  out, and alone on `Frame::Answer(EnquiryId, _)` on the way back — the
  dispatch is implicit in which enquiry is outstanding. Dispatches and probes
  share one id mint precisely so a probe's `Report` can never be mistaken
  for a dispatch's.
- **At-most-once, no protocol-level retries.** A dispatch, an enquiry, an answer
  each cross once. A broken transport fails the run — `Severed` — and is never
  replayed.
- **Reentrancy, enforced rather than documented, on both carriers.** A `Host`
  handler runs on the dispatching thread, inside the dispatch that called it,
  so it must not dispatch, probe, or reach the session of that same
  transport: the identity carrier would deadlock on its own session lock, and
  the wire's drain loop would swallow the outer run's events. One
  thread-local registry, entered by `dispatch_to_report` and consulted by
  every typed reading, names the transports this thread is
  dispatching on, and a reentrant call panics with a didactic message — a
  wedge becomes a loud, named failure at the exact call that would have hung,
  under either carrier.
- **Duration discipline**, with a litmus for what belongs on this channel:
  *promote a verb only when the caller can observe and act on the host's
  answer — value or refusal — within the turn.* A start receipt, a ledger
  read, a confirmation belong here; a result that arrives later belongs to
  the inbox ([[design/agents|agents]]).
- **Authority is enforced at the desk**, never by a visibility filter over
  which verbs a builtin index advertises — a filter is not an authority
  check once the engine may be a different machine. The desk refuses, in
  the same words the builtin's own check would have used.

## Two bindings: a call and a codec

A run's whole host-facing surface — where its surfaced values go, and who
answers its enquiries — is one object, `Host`, handed to each dispatch, so
the rails a run speaks on can never be bound to two different hosts by
accident. A host with nothing to offer hands over the mute `Host` `()` —
renders nothing, refuses every enquiry with the honest absence error — which
is batch's whole story. How a forked session is adopted is not the host's to
say: it is carriage, and the carrier decides it.

- **Identity binding — a direct call.** `dispatch` runs the whole turn on
  the calling thread, and the front-end drains events only after `dispatch`
  returns; routing an enquiry through that event channel would park with
  nobody draining. So the desk is a direct call into the installed `Host`,
  adapted by a **drain-then-handle** law — it drains whatever `Surface`
  events the run already queued before invoking the handler, so a handler's
  own output can never outrun its run's earlier values. The handler runs on
  the dispatching thread, inside the host's own call stack, answering from
  state it captured.
- **Wire binding — the desk is the codec.** The engine writes
  `Event::Enquiry`, parks a rendezvous keyed by `EnquiryId`, and the
  front-end's own drain loop (`dispatch_to_report`) answers by calling the
  same `Host` and writing `Frame::Answer` back. Only who calls the `Host`,
  and when, differs between the two bindings.
- **`Fork` is the carrier's arm.** The identity carrier lays `Fork::Park`
  over a nursery it owns: a spawning builtin parks the fork there, and the
  handler adopts it through the parent's own transport —
  `IdentityTransport::adopt_parked(id)`, a new transport over the
  child engine, so the host never holds a forked `Shell`. The wire carrier
  lays `Fork::Seal` (see *The factory*): the builtin seals its bequest and
  names the bytes in its enquiry, and the host makes the engine. Either way the child's one grant
  layer is pushed by `Shell::child_of`, from the grant the `Bequest` carries,
  under the parent installer's `narrow` and against the child's own cwd — the
  one layering step an adopted fork and a sealed child share.

## Probes: boundary-time reads

A **probe** is a pure, boundary-time reading of session state — no wall, no
sinks, no clock, absent by type — so it is a `Frame`, not a `Run`. It shares
the engine's single worker rendezvous with dispatches: a probe sent mid-run
gets the same "engine busy" a second dispatch would, since probes are legal
only at a run boundary.

**A reading is typed once.** Core's `Probe` enum (`protocol/probe.rs`) is
the frame's payload: each variant states what it takes, so a payload its probe
does not take is not representable and the engine has nothing to refuse. The
engine's answer (`engine/probe.rs`) and the host's typed reading —
`t.cwd() -> Option<PathBuf>`, `t.workers() -> Vec<WorkerRow>`, one per variant,
provided methods of `Transport` — match on the one enum, so the two ends of a
probe cannot disagree about what a probe means. A working directory that cannot
travel as text answers `None`, as no cwd at all. The answer is its own event,
`Event::Reading`, not a forged run report. The probes span session state
(`cwd`, `home`, `env-var`, `builtin-names`, `session-ended`),
the scope (`bindings`, `completion-names`, the binding counts) and the
engine's filesystem (`path-bytes`, `path-entries`). What a probe answers is *data*,
never a handle. An answer outside its class's shape is the engine breaking
the protocol: the typed reading severs the transport `Faulted` itself, which is
why `Transport::sever` is on the trait, identity included.

A failed probe distinguishes two unrelated causes: a probe the engine would
not read at all — one sent mid-run, or whose reading panicked — is a program
error on the caller's side, since a probe is legal only at a boundary; a probe for which no answer will ever come, because the
transport itself is gone, is the far side's death. The two are never
conflated into one string a caller might pattern-match.

## A session is a process is a connection

**One engine process per session, one connection per engine process — the
connection *is* the session**, so no frame carries a session address.
`Attach` opens it; the front-end's EOF or the liveness deadline closes it;
`Dispatch`, `Probe`, `Event`, `Control`, `Answer` all ride it unaddressed.

**One owner per connection.** A wire transport alone owns its `Line`:
the write channel, a never-locked duplicate to shut the socket with, and
the severance cell. A control sender and the heartbeat hold the line
weakly, so holding a sender, as an exarch `Agent` does in its `reach`,
holds no connection, and a verb aimed at a gone line is a no-op: its
engine ended every run at EOF. A session ends when its front-end drops
its transport. The wire transport's drop shuts the socket, which acts on
the connection rather than on one descriptor, and the engine reads EOF;
the identity transport's drop takes its engine, and every worker, with
it. There is no explicit session-ending verb.

**The process is what makes a session's end final.** Most of what a session
needs from its boundary is already software: `Control` cancels a dispatch,
the session's `DurableRoot` and `Roster` contain its workers, and the worker's
`catch_unwind` contains a panic. None of that ends a session that will not
cooperate. Cancellation is polling, not revocation
([[internals/cancellation|cancellation]]): an effect already past its last
check runs to completion. An atomic write's commit, for one, fsyncs and then
renames; a thread blocked in that fsync renames over its target after its
session has been abandoned. Only the death of a process ends every thread at
once, and with it every effect the session still had in flight. The process
is therefore the unit of *forced* termination, and so of a session's
effects. The isolation, scoped cancel and teardown a process also gives
come along with it.

- **No session is adopted into another's process.** In-process adoption of a
  wire helper would delete the seal (*The factory*) but would lose that bound.
  Recovering it means one of two things. The first is ending the guest
  whenever a helper fails to settle, which turns one wedged helper into the
  loss of the whole conversation. The second is a contract that no effect
  outlives its session, and cooperative cancellation cannot give one. Either
  way the adoption needs more machinery than the seal costs: a session
  supervisor, a process cancel root, an audit of every process-global
  mutation, and a new handshake. The host's identity seats share exarch's
  process, and accept the weaker bound there.

- **`/clear` is host lifecycle, not a frame**: drop the engine and boot a
  fresh one from the same `Attach`, onto the same interrupt target. A seat
  that cannot be reborn here — a wire seat, whose engine was not booted by
  this process, or an adopted fork, whose authority a fresh boot would not
  carry — refuses `/clear` with a sentence.
- **A sub-agent fork under the wire is a child engine spawned by the guest
  daemon**, on a connection the host dialled, born from an `Attach` that
  carries the parent's seal. The parent's `Bequest` crosses sealed: bytes the
  host relays unread, interpreted only by two instances of one binary, both
  spawned by the daemon from one path. The first-order law governs the
  positions the host reads; this one it does not.

## The factory: the host dials the daemon

A wire spawn is one exchange. **No engine spawns an engine**: the parent's
fork builtin does no I/O, only `bequeath`, `seal`, and names the bytes in its
enquiry as `` `sealed <bytes> ``.

- The host, *while still answering that enquiry*, asks its `Factory` for a
  connection: the desk dials `guest_abi::ports::FACTORY`, the one guest port, on which the
  guest daemon listens for the life of the VM.
- The daemon admits only a peer whose CID is `VMADDR_CID_HOST` and drops
  anything else; a guest process cannot present the host's CID, since the
  guest kernel's transport writes the source. Each admitted connection is one
  engine, spawned exactly as the root is, on fd 3.
- The desk builds the child's `Attach` from the parent's own plus the seal,
  writes it, waits for `Attached`, and only then answers the enquiry. **The
  child exists and is attached before the roster names it.** A refused attach
  reaches the parent as the desk's refusal, in the engine's words.

The root keeps dialling out: its dial is the boot's readiness signal. Children
are dialled in because the host already holds the spawn the connection is for,
so no correlation is needed. The seal is a `Bequest` in wire form, carrying the
parent's sources, terms and grant as well as its scope, and no recipe runs in a
sealed child: it derives its runtime and constructs over the unsealed bequest.
A child ends when the host drops its connection (EOF on fd 3).

## Liveness and severance

**Any frame that arrives is proof of life**; heartbeats exist only to
manufacture traffic where the failure mode is silence rather than EOF.
**Silence is counted in unanswered probes, never in elapsed clock**
([[decisions/260920_silence-is-counted-in-probes|silence-is-counted-in-probes]]):
a deadline is evidence only if someone was running to observe it, and a
suspended host is not. A
same-host child's death is a kernel-guaranteed EOF the moment its process
exits — nothing to manufacture. A guest across a virtual socket can die into
silence with no EOF at all, so the front-end that crosses one must ping, and
by pinging enters a contract it cannot silently leave: the **first `Ping`
the engine receives arms its read deadline**; a front-end that never pings
leaves the engine's patience infinite, correctly, since its death needs no
deadline to be noticed. A write that stalls past the same patience reads as
death too — a front-end that stopped reading is, within that bound,
indistinguishable from one that no longer exists. Only failed I/O severs: a
frame the fuse refuses is refused before the wire is touched, is the writer's
fault, and is answered where its writer stands (a `Report` or a reading by the
engine's refusal, a `Dispatch` by a host-fault `Report`, a `Probe` by a
rejection, an `Answer` by `Err`, an enquiry by the enquirer's own error, an
`Attach` by a refused attach); a frame with nowhere to refuse into is
diagnosed and dropped. The reader still severs on an oversize frame, a peer's
protocol violation.

Every terminal cause collapses into one type, **`Severed`**: the engine
refused `Attach` in its own words (a version mismatch, an unknown installer,
a seal it could not unseal); the stream closed or a frame failed to cross;
nothing arrived before the liveness deadline; or the engine answered outside
the protocol — a reading outside its class's shape, a frame only a front-end
sends — a fault the front-end observed for itself and records through
`Transport::sever`. `Severed` is terminal — a front-end that observes one
ends its session, never retries into it: exarch's headless and synod
front-ends end the conversation on it, showing its sentence once — and first
cause wins where more than one could apply. The reader thread that
watches a wire connection always severs *before* it closes the event
stream it drives, so a closed stream is proof a cause already exists to
read back.

The **attach verdict** is the handshake's answer: the engine replies to
every `Attach` with `Attached` or `Refused(reason)` as a session event, and
the wire front-end's `await_attached` blocks on exactly that verdict before
its first dispatch — a refusal is learnt at construction, in the engine's own
words, rather than inferred from an EOF three frames later.
`IdentityTransport::boot` returns the same verdict from the same
`Engine::boot`, so neither carrier hands out a transport its engine refused.

## Platform neutrality

The frame algebra is data, identical on every platform that can serialise
it; only its *producers* are gated. A wire type carries no platform `cfg` of
its own even when only a Unix engine can ever construct one particular
value of it, so a Windows front-end decoding a Linux guest's report at equal
`PROTOCOL_VERSION` succeeds exactly as any other value would; only the code
that *produces* or *acts on* the Unix-only value locally is gated. The
wire's own stream abstraction is std's generic owner of a connected stream
socket — one end of a socketpair, a vsock or Hyper-V socket into a guest —
never a name for one address family, so a new transport is a constructor,
not a rewrite.

## Why not gRPC, JSON-RPC, or Cap'n Proto

| | this protocol | gRPC | LSP / JSON-RPC | Cap'n Proto |
| --- | --- | --- | --- | --- |
| both directions request | native (`Dispatch`↓, `Enquiry`↑) | client-streaming hacks, hand-rolled correlation | yes | yes (capabilities both ways) |
| in-band ordering (events before result) | law, on one stream | not across RPCs | not guaranteed | pipelined, per-capability |
| cancellation | out-of-band by law, wakes a parked enquiry | per-RPC deadline, tied to the call | advisory | promise drop |
| payload fit for ral data | native (`FOValue`: variants, bytes, NaN by bits) | protobuf: no bare variants, no NaN discipline, codegen | JSON: loses bytes/NaN/variants | schema + codegen |
| schema evolution | a class on a stable envelope; unknown class = one clean error | a `.proto` rev and regen on both ends per operation | method strings, ad hoc | schema evolution rules, codegen |
| fit with [[invariants/single-binary|single-binary]] | perfect: both ends are this repository, re-exec'd | poor: a second IDL, codegen artefacts | good | poor |

The shape that *does* match is the LSP/JSON-RPC one — bidirectional requests
plus notifications, with a typed envelope over method strings. gRPC is built
for polyglot service meshes with independent teams and a schema registry as
the contract; this protocol is two halves of one program that must also
survive a machine boundary, exactly where the heavier options are weakest.

## See also

[[map/core/engine-protocol|engine-protocol]] (the files and symbols that realise this),
[[design/agents|agents]] (the inbox as the "I want it eventually" channel
the enquiry litmus routes around),
[[design/grant|grant]] (the capability meet a fork's authority narrows
through, never the protocol),
[[invariants/single-binary|single-binary]] (every wire peer is a re-exec of
the same binary, which is what makes a bare tag a safe boot recipe).
