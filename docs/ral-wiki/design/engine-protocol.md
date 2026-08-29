# The engine protocol

**A front-end and a `ral` engine share exactly one duplex stream, sorted into
four channels by direction and by whether a message is answered, and the
stream is realised by exactly two bindings — a direct call in one address
space, or a codec across a socket, a re-exec'd process, or a vsock into a
guest.** Nothing exists in one binding and not the other: *identity* is the
trivial instantiation of the algebra *wire* encodes, never a second design.
Growing the protocol never means a new channel: every host facility yet to be
invented is a class inside an existing channel's payload, decoded by one more
arm.

## Four channels, two directions

|                   | answered (call)                                                    | one-way (notice)                                              |
| ----------------- | ------------------------------------------------------------------- | --------------------------------------------------------------- |
| **host → engine** | `Dispatch → Report` — one whole run: clocked, walled, cancel-scoped  | `Control` — cancel / suspend / resume / resize, out-of-band      |
| **engine → host** | `Enquiry → Answer` — nested inside a run, no clock of its own       | `Surface` — live values ordered before the Report, or a detached worker's deferred batch |

Each direction owns one answered channel and one one-way channel. The
remaining asymmetries are semantic, not accidental, and are never smoothed
into a fake mirror pair:

- *Mood.* `Control` is imperative — cancel, resize — while `Surface` is
  indicative: this happened, this is now true.
- *Ordering.* `Surface` is sequenced within its dispatch, strictly before
  that dispatch's `Report` — what makes the transcript truthful. `Control`
  is deliberately unsequenced: it must race past an in-flight dispatch, or
  cancel could not work.
- *Clock.* `Dispatch` opens the turn's clock and holds the session's single
  worker rendezvous; `Enquiry` happens inside a dispatch and inherits all of
  it — no wall of its own, and a `Control::Cancel` must wake an engine
  parked on an unanswered enquiry.

A fifth frame, `Probe`, rides the `Dispatch → Report` rail rather than
opening a fifth channel (see *Probes*, below).

## Payloads: first-order or it does not cross

Every *open* payload position — a surfaced value, an enquiry and its answer,
a probe's reading and its answer, a hook's arguments, a settled run's value —
carries `FOValue`: a serialisable *first-order* ral value — unit, bool,
number, string, bytes, and lists/maps/variants thereof, data all the way
down, its extension slot uninhabited by construction. The envelope's own
fields (`Run`, `Ending`, `Control`, `Attach`) are closed Rust types, and
nothing in either carries an fd, a handle, a closure, or a capability beyond
what a `Run`'s ceiling already states. The terminal lease `Attach`
conveys is `#[serde(skip)]`: it reaches an engine only under identity, and an
engine on the far side of a wire attaches with no terminal of its own.
`SerialValue` — the closure-capable sibling
that ships lambdas between the pipeline-helper processes of one kernel — is
a different type for a different domain; it never reaches the protocol.

The **envelope** — `Frame`, `Event`, `SessionEvent`, correlation ids,
`Attach`'s handshake — stays a closed set of Rust types behind
`PROTOCOL_VERSION`, checked at `Attach` and refused loudly on mismatch. This
split is the design: the envelope must be exhaustive-matchable and
version-gated, because an envelope change *is* a protocol change; the
payload must be open, because operations keep arriving. **The extension
law**, stated once for every channel: a new facility is a new class — a
`FOValue::Variant` label — on an existing channel, plus a decoder arm at the
receiving end, never a new channel or frame family. The law binds a second
level too: a class that nests tags beneath it (`` agents `list ``,
`` schedules `add ``) draws the same loud error for an unrecognised *tag* as
for an unrecognised class — nesting must not open a silent hole beneath the
rule it was introduced under. An unrecognised enquiry class or tag answers
`Err` naming it; an unrecognised surface class is dropped with a note, never
silently.

**The desk's decode is a trust boundary, not a duplicate check.** Under the
wire the engine and its shell run inside a guest and the desk runs on the
host, so a guest can send whatever it likes regardless of what its own door
already validated. Both ends re-validate: the door refuses early for a fast,
legible diagnostic; the desk refuses again because a registry's own
admission check (naming, uniqueness) must hold independent of which door
reached it.

## The laws of an enquiry

- **Containment.** An enquiry lives inside its run: same wall, same
  foreground cancel scope, no clock of its own. A detached worker is built
  with no desk at all — a worker outlives its spawning run's `Report`, so one
  that could enquire would answer into a `Report` window already closed.
- **Cancel wakes a park.** `Control::Cancel` must reach an engine parked on
  an unanswered enquiry; the wire engine's park polls the run's cancel cause
  at its condvar's own wait timeout, so cancel and answer race safely.
- **Correlation from day one.** `EnquiryId`, fresh per enquiry, rides beside
  `DispatchId` on every frame that carries one — dispatches and probes share
  one id mint precisely so a probe's `Report` can never be mistaken for a
  dispatch's.
- **At-most-once, no protocol-level retries.** A dispatch, an enquiry, an answer
  each cross once. A broken transport fails the run — under identity,
  impossible; under wire, `Severed` — and is never replayed.
- **Reentrancy, enforced rather than documented.** A handler must never take
  the session lock: `dispatch`, `attach`, `detach`, `set_deferred_sink`, and
  every other lock-taking door on `IdentityTransport` check a reentrancy
  stamp naming the dispatching thread first, and panic with a didactic
  message if entered from it — a wedge becomes a loud, named failure at the
  exact call that would have hung, rather than a silent deadlock.
- **Duration discipline**, with a litmus for what belongs on this channel:
  *promote a verb only when the caller can observe and act on the host's
  answer — value or refusal — within the turn.* A start receipt, a ledger
  read, a confirmation belong here; a result that arrives later belongs to
  the inbox ([[design/agents|agents]]).
- **Authority is enforced at the desk**, never by a visibility filter over
  which verbs a builtin index advertises — a filter is not an authority
  check once the engine may be a different machine. The desk refuses, in
  the same words the builtin's own door would have used.

## Two bindings: a call and a codec

A run's whole host-facing surface — where its surfaced values go, who
answers its enquiries, how a session it forks reaches its own desk — is one
object, `Host`, installed once per dispatch, so the rails a run speaks on
can never be bound to two different hosts by accident. A host with nothing
to offer installs the mute `Host` — renders nothing, refuses every
enquiry with the honest absence error, adopts no fork — the bare REPL's
whole story.

- **Identity binding — a direct call.** `dispatch` runs the whole turn on
  the calling thread, and the front-end drains events only after `dispatch`
  returns; routing an enquiry through that event channel would park with
  nobody draining. So the desk is a direct call into the installed `Host`,
  adapted by a **drain-then-handle** law — it drains whatever `Surface`
  events the run already queued before invoking the handler, so a handler's
  own output can never outrun its run's earlier values. The handler runs on
  the dispatching thread, inside the host's own call stack: captured state,
  never `&mut Shell`.
- **Wire binding — the desk is the codec.** The engine writes
  `Event::Enquiry`, parks a rendezvous keyed by `EnquiryId`, and the
  front-end's own drain loop (`dispatch_to_report`) answers by calling the
  same `Host` and writing `Frame::Answer` back. Only who calls the `Host`,
  and when, differs between the two bindings.
- **`Fork` names how a forked session is adopted.** `Park` is the identity
  arm's door — the reentrancy law bars a handler from holding `&mut Shell`,
  so a spawning builtin parks the fork in a nursery and the handler adopts
  it by id; `Listen` is the wire arm (see *The hatch*).

## Probes: boundary-time reads

A **probe** is a pure, boundary-time reading of session state — no wall, no
sinks, no clock, absent by type — so it is a `Frame`, not a `Run`. It shares
the engine's single worker rendezvous with dispatches: a probe sent mid-run
gets the same "engine busy" a second dispatch would, since probes are legal
only at a run boundary. One decoder, shared by both transports, answers
every reading class by name (worker counts, `cwd`, the worker table) and
names an unrecognised one loudly. What a probe answers is *data*, never a
handle: the worker table decodes into the front-end's own row type rather
than exposing a live handle across the protocol.

A failed probe distinguishes two unrelated causes: a class the engine would
not read at all — an unknown reading, a malformed payload, a probe sent
mid-run — is a program error on the caller's side, since a probe is legal
only at a boundary; a probe for which no answer will ever come, because the
transport itself is gone, is the far side's death. The two are never
conflated into one string a caller might pattern-match.

## A session is a process is a connection

**One engine process per session, one connection per engine process — the
connection *is* the session**, so no frame carries a session address.
`Attach` opens it; `Detach`, an EOF, or the liveness deadline closes it;
`Dispatch`, `Probe`, `Event`, `Control`, `Answer` all ride it unaddressed.
What a multi-session envelope would reimplement in software, the kernel
already supplies per process: isolation (a wedged or panicking session
cannot touch a sibling), scoped cancel (signal the process), worker
containment (a session's workers die with it), teardown (kill it).

- **`/clear` is host lifecycle, not a frame**: kill the engine process, boot
  a fresh one from the same recipe — the identity seat's own rebuild,
  generalised, with the kernel doing the dropping.
- **A sub-agent fork under the wire is a child engine spawned inside the
  guest**, same binary, re-exec'd as `--engine`. The parent's scope crosses
  as an `EngineSeed` over an inherited fd — same-binary, kernel-to-kernel,
  the domain the closure-capable serial form already serves lawfully — and
  never touches the engine protocol at all. The first-order law governs the
  protocol between two parties who cannot assume each other's version; a
  parent spawning its own child, same binary, is not that protocol.

## The hatch: the host dials in

A wire spawn is one exchange, not a standing listener answering an unbounded
stream of dials: **the guest binds an ephemeral port for the duration of
exactly one spawn**, and names it — port and an eight-byte token — in its
own enquiry payload.

- The host, *while still answering that enquiry*, dials the named port and
  writes the token.
- The guest's listener thread checks the token, spawns `current_exe
  --engine` with the dialled connection handed to it on the protocol fd and
  the seed on an inherited one, and only *then* writes the single
  acknowledgement byte, `HATCH_ACK` — the whole of the guest's readiness
  signal, since the wire's only legal first frame is `Attach` and nothing
  else remains to say it.
- The host reads the ack, adopts the stream as the child's seat, and only
  then enrols the child in the roster. **The child exists before the roster
  names it**, literally: the ack is written after `spawn()` has already
  returned.

Neither the token nor the ack is a frame; both live before the handshake,
beside the algebra rather than inside it. The token secures *adoption*, not
authority — a plain capability meet already stops a child exceeding its
parent — by stopping an unrelated guest process from getting the host to
build an agent around its socket. The seed itself is written only after the
child is spawned and read before it waits for `Attach`, so neither side
blocks the other on a buffer a large scope could outgrow; a partial seed
kills the child on the spot rather than waiting on a death that never comes.

## Liveness and severance

**Any frame that arrives is proof of life**; heartbeats exist only to
manufacture traffic where the failure mode is silence rather than EOF. A
same-host child's death is a kernel-guaranteed EOF the moment its process
exits — nothing to manufacture. A guest across a virtual socket can die into
silence with no EOF at all, so the front-end that crosses one must ping, and
by pinging enters a contract it cannot silently leave: the **first `Ping`
the engine receives arms its read deadline**; a front-end that never pings
leaves the engine's patience infinite, correctly, since its death needs no
deadline to be noticed. A write that stalls past the same patience reads as
death too — a front-end that stopped reading is, within that bound,
indistinguishable from one that no longer exists.

Every terminal cause collapses into one type, **`Severed`**: the engine
refused `Attach` in its own words (a version mismatch, an unknown installer,
a seed it could not apply); the stream closed or a frame failed to cross;
nothing arrived before the liveness deadline; or the engine answered outside
the protocol — a refused boundary-time probe, say — a fault the front-end
observed for itself. `Severed` is terminal — a front-end that observes one
ends its session, never retries into it — and first cause wins where more
than one could apply. The reader thread that
watches a wire connection always severs *before* it closes the event
stream it drives, so a closed stream is proof a cause already exists to
read back.

The **attach verdict** is the handshake's answer: the engine replies to
every `Attach` with `Attached` or `Refused(reason)` as a session event, and
the wire front-end's `await_attached` blocks on exactly that verdict before
its first dispatch — a refusal is learnt at construction, in the engine's own
words, rather than inferred from an EOF three frames later.

## Platform neutrality

The frame algebra is data, identical on every platform that can serialise
it; only its *producers* are gated. `Ending::Stopped` — a job-control
outcome only a Unix engine can ever produce — carries no platform gate on
its own type or decode path, so a Windows front-end decoding a Linux guest's
`Stopped` report at equal `PROTOCOL_VERSION` succeeds exactly as any other
arm would; only the code that *renders* it locally, where stopping a local
job means something, is gated. The wire's own stream abstraction is std's
generic owner of a connected stream socket — one end of a socketpair, a
vsock or Hyper-V socket into a guest — never a name for one address family,
so a new transport is a constructor, not a rewrite.

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
