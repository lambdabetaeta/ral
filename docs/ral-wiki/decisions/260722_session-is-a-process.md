---
status: active
---

# A session is a process: the wire seam stays single-session

**One engine process per session, one connection per engine process — the
connection *is* the session, so frames carry no session address.** The
multi-session engine — a `Frame::Session(SessionId, …)` envelope, a
per-session worker table, session-scoped cancel cells, and engine-side
`Fork`/`Clear`/session-`Detach` — was built to completion, measured against
this question, and rejected before shipping; the wire speaks flat frames at
version 4. What that machinery reimplemented in software, the kernel
already supplies per process: isolation (a wedged or panicking
session cannot touch a sibling), scoped cancel (signal the process), worker
containment (a session's workers die with it), and teardown (kill it).

## The error this corrects

The multi-session engine was built for one reason: engine-side fork.
[[decisions/260706_enquiry-channel|enquiry-channel]] §5 rejected "one engine
process per agent, `SerialEnvSnapshot` shipped across" on two grounds, and
neither survives inspection:

- *"A shipped snapshot carries the scope's closures, which the first-order
  law forbids on every channel."* This conflates two boundaries. The
  first-order law governs the **host seam**, where peer and version are not
  yours to assume. A sub-agent fork is parent engine → child engine,
  **inside the guest, same binary** — the domain where `SerialEnvSnapshot`
  (`core/src/serial.rs`) already ships closures lawfully between the
  pipeline-helper processes of one kernel. A parent engine that spawns its
  fork as a child process and hands it the snapshot over an inherited fd
  never touches the host seam. The rejection was aimed at the
  *host-mediated* fork — snapshot round-tripped through the front-end — and
  at that shape it stands.
- *"It multiplies VM-side processes per sub-agent."* Not a defect: a ral
  pipeline already runs each helper as a re-exec'd process of the same
  binary. An engine spawning an engine is the same move one level up, and
  buys the kernel guarantees above for free.

With the fork argument gone, the session table is generality without a
caller — the exact trap
[[decisions/260721_synod-is-a-second-product|synod-is-a-second-product]]
names and rejects for the crate split: a factoring performed on a guess,
pinned into the protocol.

## The decision

- **The connection is the session.** `Attach` opens it, `Detach` (or EOF,
  or the liveness deadline) closes it; `Dispatch`, `Probe`, `Event`,
  `Control`, `Answer` ride it unaddressed. `SessionId`, `SessionFrame`, and
  `Event::Forked` do not exist on the seam.
- **Cancel is `Control` on your own connection.** One session per process
  makes the process-global foreground scope lawful again engine-side; the
  dispatch-precision guard (only the named, still-in-flight dispatch is
  cancelled) is kept.
- **`/clear` is host lifecycle, not a frame**: kill the engine process,
  boot a fresh one from the same recipe. Identical semantics to the
  identity seat's rebuild — replacing the shell drops the old one, whose
  teardown cancels its workers — with the kernel doing the dropping.
- **Sub-agent forks under the wire are guest-side spawn, built when they
  have a caller.** The shape is fixed now so it never grows back into the
  protocol: the `agent-start` handler asks the parent engine to spawn a
  child engine (same binary, `--engine`), the snapshot crosses guest-side
  over an inherited fd, the child dials the host on a fresh connection
  correlated by token, and the new host agent adopts it. Synod is that
  caller: it lifts its trunk's fuel off zero and drives the two-phase
  `agent-start` → `hatch` → `agent-hatched` sequence
  ([[map/synod|synod]], [[map/exarch/agent|agent]]) over the rendezvous this
  paragraph fixed, with its own exchange bracketed by
  [[decisions/260806_exchange-ends-at-fleet-quiescence|synod's exchange
  ends at fleet quiescence]] rather than exarch's chat-while-they-work
  model. Before that caller arrived, synod v1 refused `agent-start` (fuel 0)
  and needed one agent, one engine, one vsock — the daemon's fd-3 spawn
  contract and `Machine::take_control` already were that shape.

What the multi-session build got right ships without it, none of it
session-shaped: engine-owned durability (checkpoint and rollback at
`Shell::run_turn`), the bounded teardown settle, liveness armed by the
first ping
([[decisions/260721_liveness-armed-by-first-ping|liveness-armed-by-first-ping]]),
the testable `engine_session` split, boot recipes on `EngineInstaller`
(minus `arm`, which only re-armed forked sessions), `Break::Error`'s
`command_exit` fact, the pushed notices, and the enquiry and probe rails.

## Why not ship what was built

It was built and tested, and discarding working code is not free. But
every frame family is protocol surface under maintenance and version
obligation, the host-side half (the wire multiplexer routing per-session
streams to per-agent drain loops) was never built and would have been the
largest piece, and the only consumer with a date — synod v1 — uses none of
it. The session table was nine tests proving machinery no product drives.
Shipping it "because it's done" is the sunk-cost shape of the same
speculation.

## Consequences

- `core/src/engine.rs` is one worker, one desk, one busy flag — plus the
  kept improvements above.
- `PROTOCOL_VERSION` is 6 (`core/src/transport.rs`), having moved several
  times since — `Frame::Ping`/`Frame::Pong`
  ([[decisions/260721_liveness-armed-by-first-ping|liveness-armed-by-first-ping]])
  among the bumps — while the wire seam stays single-session throughout.
- The remaining synod increment shrinks to: a wire seat variant covering
  what `headless::run` exercises (dispatch, cancel, the drain loop's
  existing enquiry arm), `session.rs` adopting `take_control` into
  `WireTransport::adopt`, and `boot.img`.
- [[decisions/260706_enquiry-channel|enquiry-channel]] §5 is amended: its
  process-per-agent rejection is narrowed to the host-mediated fork.
- If a second product ever genuinely needs two sessions on one stream, the
  envelope can return behind a version bump — as a recorded decision with
  a caller, not as headroom.

## See also

[[decisions/260706_enquiry-channel|enquiry-channel]] (the seam this
corrects one section of),
[[decisions/260628_host-seam-transport-parametric|host-seam-transport-parametric]]
(the frame algebra this keeps at its original arity),
[[decisions/260721_liveness-armed-by-first-ping|liveness-armed-by-first-ping]]
(per-connection, unaffected),
[[decisions/260721_synod-is-a-second-product|synod-is-a-second-product]]
(the no-speculative-generality principle applied here to the protocol),
`dev/docs/VM/SYNOD-v1.md` (the increment this shrinks).
