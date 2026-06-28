---
status: proposed
---

# The host seam is transport-parametric: the front-end owns the terminal, the engine operates a leased one

**The seam between a front-end and the `ral` engine — the framed source turn door
`Shell::run_source_turn(src, TurnRequest) -> TurnReport`
([[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]]) — is *parametric
in its transport*: the same dispatch / result / cancel / lease vocabulary runs as
an in-process call sharing one kernel, or across a process or a VM boundary, with
no construct existing in one mode and not the other.** A front-end *owns* a
controlling terminal; the engine *never owns* one — it *operates* a terminal it is
granted, a real kernel tty that is the login tty under the identity (in-process)
transport and a PTY under a remote transport. Three corollaries fall out and are
the substance of this decision: the [[decisions/260619_terminal-lease|terminal
lease]]'s authority is *conveyed across the seam by the terminal's owner*, never
self-derived from a local kernel probe; POSIX signals exist only *within* a
process and cross the seam as *typed messages*; and the cut falls only *around a
whole turn*, never inside an effect scope. The local, in-process deployment is the
identity instantiation of this one mechanism, not a privileged special case.

This is a *direction* ADR, filed from a design conversation. Nothing here is
built. It rests entirely on landed pieces — the
[[decisions/260610_evaluator-runtime-split|evaluator/runtime split]],
[[decisions/260610_host-embedding-api|host-embedding-api]],
[[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]],
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]],
and the [[decisions/260619_terminal-lease|terminal lease]] — and decides the
*shape* the seam must have so that moving the engine into a VM is a transport
substitution rather than a rearchitecture. The motivating roadmap: a front-end
(an interactive shell, or `exarch`) running on a host, driving a `ral` engine
inside a VM, so the agent's whole execution environment is contained.

## The setting: three co-residence assumptions

The host seam is already clean in shape — the source turn door takes *policy* and returns
one report, and core owns *resources*
([[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]]). But three facts
it rests on silently assume the front-end and the engine share an address space,
a kernel, and a controlling terminal:

1. **The seam is an in-process function call.** `run_source_turn` is a synchronous
   Rust call; the front-end (`ral`'s REPL loop, `exarch`'s TUI) and the engine are
   one process, embedded by `boot_shell`
   ([[decisions/260610_host-embedding-api|host-embedding-api]]). The local REPL
   also uses `run_value_turn` for prompt, plugin, and startup thunks, but that is
   an in-process embedding helper, not a transport construct.
2. **The lease is self-minted from the local kernel.**
   `TerminalState::probe_with_mode` computes `startup_foreground` from
   `tcgetpgrp(stdin) == getpgrp()`, and
   `TerminalLease::mint_at_startup(startup_foreground)` turns that local probe
   result into the session's terminal authority
   ([[decisions/260619_terminal-lease|terminal-lease]] §1).
3. **Cancellation is a shared signal domain.** A front-end's Ctrl-C reaches the
   engine because they share a process: today `exarch` chains its `SIGINT`
   disposition in front of ral's `relay_handler` by capturing a raw function
   pointer (commit `6abb905`). That only works in one address space.

A VM boundary breaks every one of these. The engine in the VM has no host
controlling terminal to probe; a raw function-pointer chain cannot cross a wire;
a synchronous call cannot be interrupted by a peer that is on the far side of a
blocked transport. This ADR decides each of the three so the in-process and
remote deployments are one design.

## The lesson we are not allowed to forget

ral once *had* full IPC, and tore it out. The since-removed `sandbox-ipc-cancel`
decision and its superseder
[[decisions/260617_sandbox-external-children|sandbox-external-children]] record
why: a `grant { … }` body was transported into a confined child interpreter
(`run_confined`, `WireMobile` serialising closures, a one-body child server). It
was removed because **the cut went through the middle of the evaluator's dynamic
context.** A grant is a *meet* — nested grants compose by intersection over
authority — and "that algebra belongs to the evaluator's dynamic context, not to
a hidden process boundary." Putting a process boundary inside a turn forced
closures, handler stacks, and live continuations to straddle the cut, which is
why it needed mobile closures and why a parent could deadlock blocked in
synchronous IPC mid-evaluation.

The lesson is precise and load-bearing here: **the boundary may fall only around a
whole turn, never inside an effect scope.** The framed turn doors already satisfy
this — they evaluate a complete top-level turn, with the grant meet, the handler
stack, shell state, and the cancellation scope living entirely engine-side, and
return a serialisable `TurnReport`. They are exactly the "evaluate a complete turn
in a confined interpreter child and return only a serialisable result" boundary
that [[decisions/260617_sandbox-external-children|sandbox-external-children]]
explicitly preserved as legitimate while killing grant-body IPC. This ADR commits
that the turn door *is* that boundary and forbids any future seam that reaches
inside a turn.

## The decision

### 1 — Ownership is not operation

The front-end owns its process's controlling terminal — it is the terminal's user
and the source of turns. The engine only ever *operates a terminal it is leased*.
These are different rights held by different parties, in every deployment. The
[[decisions/260619_terminal-lease|terminal lease]] already encodes "operate, by
grant"; this ADR fixes that ownership and operation never coincide in the engine,
which is what lets the engine move to a VM without its job-control code changing.

The dilemma this dissolves — *does the engine keep the tty (cheap) or does
job-control move up into the front-end (symmetric)?* — is a false one. It assumes
ownership and operation are the same act. They are not: ownership stays in the
front-end, operation stays in the engine, and the engine's existing
`tcsetpgrp` / termios / pgid machinery (`signal/unix.rs`) is untouched because it
operates a real kernel tty in both deployments (§3).

### 2 — The lease is granted across the seam, not self-minted

Replace the startup self-mint with a grant conveyed by the terminal's owner:

```
was:  startup_foreground = tcgetpgrp(stdin) == getpgrp()
      TerminalLease::mint_at_startup(startup_foreground) -> Option<Self>
now:  the front-end conveys a terminal endpoint at attach;
      the engine's session holds Option<TerminalLease> bound to it.
```

The session's controlling terminal — today always the local login tty probed at
startup and passed to `mint_at_startup` — becomes a **leased endpoint conveyed by
the front-end when it attaches to the engine**:

- **identity transport** — the front-end *is* the terminal's owner in the same
  kernel; the grant conveys the real fds, and the `tcgetpgrp(stdin) == getpgrp()`
  probe is exactly the front-end asserting "I own this terminal, here it is."
  Nothing observable changes from today.
- **remote transport** — the front-end conveys "I own a terminal; here is a
  PTY-bridge endpoint." The VM-side engine allocates a PTY as its session's
  controlling terminal, backed by that bridge.

This *completes* the witness discipline of
[[decisions/260601_reduced-authority-witness|reduced-authority-witness]] that
[[decisions/260619_terminal-lease|terminal-lease]] began. That ADR already
apologised for `startup_foreground` surviving as "compatibility data" and for the
engine self-deriving its own authority from a process-global probe. Making the
lease a *grant that crosses the seam* removes the last ambient self-derivation:
the engine can no longer mint terminal authority from its own kernel state; it can
only be *handed* a terminal by the party that owns one. The unforgeability is
strictly stronger, not weaker.

Crucially, the **carrier discipline of
[[decisions/260619_terminal-lease|terminal-lease]] is unchanged.** The session
owns `Option<TerminalLease>` (the terminal *exists*); `TurnState` carries
`TerminalAccess` (may this turn foreground *now*), restored by `TurnGuard` like
`cancel` and `surface` ([[decisions/260617_turn-local-state|turn-local-state]]).
What changes is only the *provenance* of the session lease (granted, not probed)
and the *referent* of its terminal (a kernel tty that may be a PTY). The PTY is a
session-scoped resource allocated at attach, exactly as the login tty is today;
per-turn `RequestedTerminalAccess` / `TerminalAccess` / `ExplicitLoan` are
untouched.

### 3 — A PTY is a tty: the engine's job-control code does not branch

The remote transport works only because **a PTY is indistinguishable from a
controlling terminal to the process operating it.** The VM-side engine's leased
terminal is a real PTY in the VM's own kernel, so an interactive foreground child
(`vim`, `top`, `less`) sees a genuine tty, and the engine's `tcsetpgrp`, termios,
SIGTTOU-masked restore, and pipeline-group machinery run *identically* to the
local case. The engine cannot tell whether its terminal is the login tty or a PTY,
and must not be able to. That indistinguishability is what makes "write the
job-control machinery once" a fact rather than an aspiration. The front-end holds
the far end of the bridge to the PTY master and relays bytes, window size, and
termios mode to the human's real terminal for the duration of a foreground lease.

### 4 — Signals exist within a process; they cross the seam as messages

POSIX signals are a within-kernel mechanism. They survive *inside* each process,
for its own terminal's process groups — unchanged. Across the seam they become
typed protocol messages:

| within a process | across the seam |
| --- | --- |
| Ctrl-C → `SIGINT` → foreground `CancelScope` | a `cancel` message |
| Ctrl-Z → `SIGTSTP` | a `suspend` message |
| `SIGWINCH` (resize) | a `resize` message |

This **deletes, at the seam, the signal-chaining apparatus of commit `6abb905`.**
Capturing ral's `relay_handler` / `term_handler` function pointers and chaining
dispositions exists *only* because `exarch` and the engine share one process and
one signal domain. Under a real transport there is nothing to chain: a front-end's
Ctrl-C is a `cancel` message, and the engine applies it to the current turn's
foreground scope exactly as a local `SIGINT` does today. The non-escalating,
turn-scoped semantics already chosen
([[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]],
[[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]]) are precisely the
semantics a `cancel` message carries: cancel the turn, never force-exit the
engine.

### 5 — Cancellation is out-of-band, by law

The since-removed `sandbox-ipc-cancel` decision was written against a real
incident: a `cargo test` under the sandbox blew its 30s limit, and the TUI's Esc
could not return control because the parent was blocked in a synchronous IPC round
trip. Its hard rule is reinstated verbatim and promoted to a law of this seam:

> Any host that waits on a turn must hold a `cancel` channel **independent of the
> result stream**. Cooperative polling across a blocked transport is not a
> cancellation boundary.

Therefore the transport must be **full-duplex**: a `cancel` (or `suspend`, or
`resize`) message must be deliverable while a `dispatch` is outstanding. A
strictly synchronous request/response transport is rejected on exactly the failure
the incident recorded. Under the identity transport this is the existing
foreground `CancelScope` observed off-thread; under a remote transport it is a
control message on the duplex channel.

### 6 — The transport is the only parameter

Everything above is defined independent of the transport. The two deployments
differ *only* in the transport binding:

- **identity transport** — in-process, one kernel. The lease conveys real fds; the
  bridge is identity (no relay process); `cancel` is the local cancel scope. There
  is nothing to copy.
- **PTY-bridge transport** — serialised protocol across a process or VM boundary.
  The lease conveys a PTY endpoint; the bridge relays terminal bytes / resize /
  termios; `cancel` is a control message.

There is no `if local { … } else { … }` in the *design*. An implementation may
detect the identity transport and elide copies — that is an optimisation the
semantics already license, not a second code path with its own rules. The wire
form is not invented from scratch: the serde mirror (`SerialValue`,
`SerialBinding`) and envelope (`WireMobile`) that already carry a shell across the
pipeline-helper re-exec ([[map/core/transport|transport]]) are the starting point
for the result/value edge of the protocol.

### 7 — Dispatch carries source, not host-held values

The transport dispatch is `(src, TurnRequest)`. A live core `Value` is never the
program of a wire turn.

`run_value_turn` remains a local REPL embedding helper: prompt bodies, plugin
hooks, plugin factories, and startup blocks are already-held thunks in the same
process as the engine. That shape does not survive a process or VM boundary
without either leaking core's internal `Value` representation or inventing a
second program-reference protocol. This ADR rejects both for the host seam.

If a remote shell front-end later wants programmable prompts, plugins, or startup
hooks inside the VM, it must install them as source in the engine and invoke them
by an engine-owned name or ordinary source-level mechanism. The transport still
carries source turns and serialisable protocol values, never executable `Value`s.

### 8 — `surface` crosses as two frame kinds, not two channels

The result/event side of the protocol is a typed frame stream. There is still one
language→host vocabulary, `surface`, with the two delivery regimes decided by
[[decisions/260622_surface-delivers-itself|surface-delivers-itself]]:

- **live surface frame** — emitted while the dispatched turn is alive; ordered
  before that turn's `TurnReport`; allowed to depend on the turn's foreground
  lease and live UI context.
- **boundary surface batch** — emitted only at a turn boundary; carries the
  deferred batch a detached worker flushed through its `Boundary` sink; decoded by
  the same surface decoder as live frames.

There is no `notify` protocol channel. Full duplex is still required, but for
control frames (`cancel`, `suspend`, `resize`) and for engine→front-end events
that may arrive while a dispatch is outstanding. `TurnReport` closes the
foreground dispatch; a boundary batch is a separate boundary event, not a second
notion of turn completion.

### 9 — Terminal access is a sandbox projection axis

The terminal lease has three layers, and the remote seam makes the third explicit:

- **session lease** — does this engine session have a terminal endpoint at all?
- **turn access** — may this turn borrow it (`Denied`, `Leased`,
  `ExplicitLoan`)?
- **sandbox projection** — may this child open or ioctl the terminal endpoint?

The projection therefore needs a terminal axis. `Denied` projects to *no terminal*
(`/dev/tty` opens and terminal ioctls denied, modulo platform limits);
`Leased`/`ExplicitLoan` may project the exact leased endpoint and the ioctls
needed for foreground terminal operation. The loan is not "the bridge exists";
the loan is the engine's typed permission to pass that endpoint to this child.
That makes the macOS tty-ioctl hole recorded in
[[map/core/capabilities|capabilities]] an implementation gap with a chosen shape,
not an open design question.

### 10 — Reconnect is host lifecycle, not seam law

The seam requires a **session-lived** engine, not a daemon. The baseline remote
deployment may be an attached pair: the front-end starts an engine, drives it for
one session, and both die together. A supervised deployment may keep the engine
alive after the front-end drops and later attach a new front-end with a fresh
terminal endpoint, but that requires a host-level naming and authentication
scheme outside this ADR. Reconnect is an optional lifecycle policy layered over
the same attach/dispatch/cancel/lease protocol, not a construct the engine
semantics depends on.

## The front-end is a role with two inhabitants

The engine/front-end split is not new — it is the line
[[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]],
[[decisions/260610_host-embedding-api|host-embedding-api]],
[[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]], and
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
have been drawing. This ADR only recognises the *front-end* as the unit that owns
the terminal and may run remote, and observes that the role already has two
inhabitants:

- **the shell front-end** (`ral`'s REPL host) — turns sourced from a human's
  keystrokes; "ral as my interactive shell" is this front-end plus an engine.
- **the agent front-end** (`exarch`, [[design/exarch-architecture|exarch-architecture]])
  — turns sourced from the model. It stays *only an agent*; it does not become the
  general shell.

They are peers, distinguished only by where turns come from, and both drive an
engine over the one transport-parametric seam. "ral as my shell with a TUI" is the
shell front-end over the identity transport; "exarch driving a contained agent" is
the agent front-end over a PTY-bridge transport into a VM. The seam is the same.

## The lease state machine

Terminal access over a turn, building on
[[decisions/260619_terminal-lease|terminal-lease]]'s `TerminalAccess`. The session
holds the terminal (an `Option<TerminalLease>` bound to a leased endpoint); the
turn holds an access level; a foreground child holds the terminal transiently.

```
session attach:
    front-end conveys terminal endpoint ──▶ session: Some(TerminalLease)
                                            (none conveyed ──▶ None, e.g. a
                                             backgrounded or tty-less front-end)

per turn (TerminalAccess on TurnState, restored by TurnGuard):

    Denied ───────────────── no foreground handoff this turn (exarch tool turn)
    Leased ───────────────── may foreground; holds the borrow at the door
       │
       ├─ acquire foreground ─▶ child owns the tty (tcsetpgrp); engine is
       │                        background until the child releases
       ├─ release ◀───────────  child exits / returns the terminal
       ├─ suspend (Ctrl-Z / `suspend` msg) ─▶ parked (REPL front-end only;
       │                                       a non-interactive turn reaps)
       └─ resume (`fg`) ◀────── re-acquire

    ExplicitLoan ─────────── within-turn elevation of an already-Leased turn
                             (the _ed-tui case: a captured-stdout body like
                              `fzf` that draws on /dev/tty). begin/end, not RAII,
                              per terminal-lease's recorded deviation. Can only
                              *raise* a Leased turn — never mint from Denied.

turn end: TurnGuard restores the prior TerminalAccess.
```

The state machine is identical across transports — it is engine-local — except
that "child owns the tty" means the login tty (identity) or the PTY (remote), and
"suspend"/"resume" arrive as `SIGTSTP`/`fg` locally or as `suspend`/`resume`
messages remotely.

## The one corner the theory must pin: disconnect mid-lease

A transport can drop while a lease is held (the VM pauses, the bridge breaks, the
front-end dies). This is the corner the design must define precisely, because it
is the remote analogue of a hangup:

- **Terminal loss is cleanup, not authority.** A disconnect while a foreground
  child holds the PTY appears as HUP/EOF on the PTY master's bridge. The engine
  treats it as terminal loss: it reaps the foreground subtree (the analogue of
  `SIGHUP` to the foreground group), restores its own pgid + termios, and settles
  the in-flight turn as **cancelled** — the same outcome the `cancel` message
  produces, reached through a different trigger.
- **The required lifetime is session-lived, not daemon-lived.** The engine must
  live across the turns of one shell/agent session, because it owns *all* dynamic
  context — the grant stack, the handler stack, shell state, defined functions.
  It does **not** have to outlive its front-end. A front-end-attached engine may
  die with the front-end after the cleanup above; a supervised engine may instead
  remain alive and let a new front-end attach with a fresh terminal endpoint. Both
  are the same seam. What is forbidden is only a fresh engine *per turn*, because
  that would make session continuity a serialisation problem: turns and leases
  cross the wire, while the dynamic context stays engine-local.

This is why **process-per-turn is rejected** (see Alternatives): a fresh engine
process per turn would have to rehydrate the dynamic context every turn,
re-creating precisely the transport-of-dynamic-context mistake that grant-body IPC
died for.

## Why this shape

- **It is the boundary the history blessed, not the one it killed.** The cut is
  around a whole turn; the grant meet and handler stack stay engine-side. This is
  the legitimate whole-interpreter boundary
  [[decisions/260617_sandbox-external-children|sandbox-external-children]] kept,
  not the grant-body IPC it removed.
- **It removes authority before it adds transport.** §2 deletes the engine's last
  self-minted authority (the local `tcgetpgrp` probe) rather than wrapping it. The
  transport is added under a seam that is *already* a capability boundary, so the
  wire does not become a new way to forge a terminal.
- **The engine does not learn it is remote.** §1 and §3 keep ownership in the
  front-end and operation in the engine over an indistinguishable kernel tty, so
  the engine's job-control code is transport-blind. The only transport-aware code
  is the front-end's bridge and the protocol layer.
- **It forces the representation boundary that
  [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
  asks for.** A wire cannot carry core's internal types; the behavioural-accessor
  discipline stops being hygiene and becomes a mechanical requirement of
  serialisation. The protocol *is* the published seam.
- **The local case pays nothing.** The identity transport is the trivial
  instantiation; there is no proxy, no relay, no copy when the front-end and engine
  share a kernel. Symmetry is achieved by parametricity, not by making the common
  case pay the rare case's cost.

## What changes, what stays

- **New:** a transport-parametric protocol over the existing framed source turn
  seam, carrying `dispatch(source-turn)`, the `TurnReport`, live `surface`
  events and deferred boundary surface batches, the out-of-band `cancel` /
  `suspend` / `resize` control messages, and the terminal-endpoint grant +
  revoke; a PTY-bridge transport implementation; a front-end-side terminal bridge
  (bytes / resize / termios relay); and a terminal axis in the sandbox
  projection.
- **Changed:** the lease's provenance — `startup_foreground` followed by
  `mint_at_startup` (a local probe-derived bool) becomes a grant conveyed by the
  front-end at attach; the session's controlling terminal becomes a leased
  endpoint (login tty under identity transport, PTY under remote).
- **Deleted, at the seam:** the cross-process signal-chaining of commit `6abb905`
  (`exarch`'s `RAL_SIGINT_HANDLER` / `RAL_TERM_HANDLER` chaining). It is replaced
  by `cancel` / `suspend` messages. Within-process signal handling is unchanged.
- **Unchanged:** the [[decisions/260619_terminal-lease|terminal-lease]] carrier
  discipline (session-owned `Option<TerminalLease>`, per-turn `TerminalAccess`
  restored by `TurnGuard`); `ForegroundGuard`'s pgid/termios save-restore and
  SIGTTOU mask; the pipeline group/anchor/relay machinery; the engine's
  job-control code; the grant *meet* and `capability::check_*` chokepoint, which
  stay engine-side; the single `surface` vocabulary and its deferred boundary
  destination
  ([[decisions/260622_surface-delivers-itself|surface-delivers-itself]]), which
  become the result/event stream of the protocol.

## Alternatives considered

- **Keep host↔engine in-process forever; sandbox only external children.** The
  status quo, and correct *as the identity transport*. Rejected only as the
  *direction*: it forecloses ral-in-a-VM and keeps growing the shared-signal-domain
  machinery (commit `6abb905`) that a real transport deletes. This ADR retains it
  as the identity instantiation, not as a separate design.
- **A separate helper process or session per *tool turn*** (the literal "run every
  turn in its own session" proposal). Rejected: a fresh engine per turn must
  rehydrate the dynamic context — shell state, handles, the grant stack — every
  turn, which is session-continuity-as-serialisation and an echo of the grant-body
  IPC mistake. The engine must be *session-lived* across turns; only turns and
  leases cross the wire.
- **Cut inside the turn (grant-body IPC, redux).** Rejected by the explicit lesson
  of [[decisions/260617_sandbox-external-children|sandbox-external-children]]: a
  process boundary inside an effect scope forces mobile closures and a parent
  blocked mid-evaluation. The cut is around a whole turn or it is wrong.
- **Re-derive the lease from a local kernel probe even when remote.** Rejected:
  there is no local controlling terminal in the VM, and self-minting from a kernel
  probe is the ambient-authority residue
  [[decisions/260619_terminal-lease|terminal-lease]] already flagged. The lease
  must be granted across the seam.
- **A synchronous request/response transport.** Rejected on the `sandbox-ipc-cancel`
  incident: a wedged synchronous round trip is uncancellable. The transport is
  full-duplex so `cancel` is always deliverable (§5).
- **Make the front-end own job-control even in the co-resident case (full
  symmetry, PTY everywhere).** Rejected: it pays the PTY-proxy cost when a real tty
  is an inch away, for no gain. §1 already delivers symmetry — ownership in the
  front-end, operation in the engine — without forcing a PTY locally.

## Consequences

- Moving the engine into a VM is a transport substitution: the engine's
  evaluation, job-control, and capability code are transport-blind, so the change
  is confined to the protocol layer and the front-end's bridge.
- `exarch` stays only an agent; the shell front-end is its peer; both drive the
  same engine over the same seam. "ral as my shell with a TUI" and "exarch driving
  a VM-contained agent" are two transport instantiations of one architecture.
- A disconnect cancels the in-flight turn and reaps its foreground child. After
  that, host lifecycle policy decides whether the engine exits with its front-end
  or remains available for re-attach; the seam requires only that the engine is
  session-lived across turns, not process-per-turn.
- The protocol has no `notify` channel and no executable core-`Value` wire form.
  Surface is a single vocabulary with live frames and deferred boundary batches;
  the in-process value-turn helper is removed before transport work begins.
- Terminal loaning becomes part of the sandbox projection: a Denied turn projects
  no terminal endpoint, while Leased/ExplicitLoan may project the exact leased
  tty/PTY endpoint.
- The two-enforcers story ([[design/two-enforcers|two-enforcers]]) gains an outer
  ring: with the engine in a VM, the VM is itself a sandbox boundary, and the
  per-command Seatbelt/bwrap confinement of
  [[decisions/260617_sandbox-external-children|sandbox-external-children]] becomes
  defence-in-depth *inside* the VM rather than the only wall. This ADR does not
  design that ring; it only notes the engine's relocation makes it available.
- The protocol is the published host seam, so
  [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]'s
  accessor discipline becomes mechanically enforced by serialisation rather than
  by review.

## Implementation plan

### Phase 0 — Source-only host turns

**Make every computation scheduled by a front-end look like a source turn before
any transport exists.** Lambdas and blocks remain first-class inside the
evaluator, and `apply` remains the language's eliminator for them. What
disappears is narrower: no host API accepts a live `Value::Block` or
`Value::Lambda` as the program of a fresh turn.

The target representation is a host program reference, not a runtime closure:

```
HostProgramRef {
    namespace,      // session or plugin-private
    name,           // engine-minted or parser-validated
    origin,         // path plus source range for diagnostics
    default_policy, // e.g. Denied, capture stdout, turn budget
}
```

The engine may cache parsed, typed, or elaborated forms behind that reference.
That cache is engine-local state. It is not the host seam, and it never becomes a
wire form.

0. **Keep language values in the language.** Ordinary aliases, lexical bindings,
   lambdas, and blocks may still be stored in the engine session and invoked from
   an already framed turn. A callable is only forbidden when the *front-end* wants
   to schedule it as the root program of a new turn.
1. **Add seeded source turns.** `TurnRequest` grows a small turn-local seed map.
   The engine installs those seeds as the innermost turn scope before
   typechecking and evaluation, with schemes derived from the seeded data. Seeds
   admit only transportable data values: unit, bool, number, string, bytes, list,
   map, and variant. They do not admit blocks, lambdas, handles, captured
   environments, or builtin bodies. Prompt bindings (`USER`, `CWD`, `STATUS`),
   plugin options, hook arguments, editor context identifiers, and preserved
   status all enter through this seed map, not by interpolating values into
   generated source text.
2. **Register host programs from source-aware loaders.** Any subsystem that
   accepts code to be run later must still be holding source when it accepts it.
   A literal block/lambda may be registered from its source span; a config or
   manifest may also name a program declared in the same session or plugin
   namespace. A computed callable with no recoverable source is not schedulable as
   a fresh host turn. The diagnostic should say what shape is accepted, e.g.
   "prompt expects a source block or named prompt program; did you mean
   `prompt: { ... }`?"
3. **Lower rc startup and prompt to program references.** `startup: { ... }`
   registers a session program whose wrapper forces the block under
   `RequestedTerminalAccess::Denied`; the source-level block boundary keeps its
   local `let`s local. `prompt: { ... }` registers a prompt program; each render
   dispatches a source wrapper with `USER`, `CWD`, and `STATUS` in the seed map,
   captures stdout, and preserves the user's previous status. A non-callable
   prompt remains plain data.
4. **Lower plugins to private namespaces.** Loading a plugin evaluates source
   that declares plugin-local programs and returns manifest data. The manifest
   stores hook names, keybinding names, alias names, and policy metadata, not
   executable handler values. A plugin factory receives its options through the
   seed map and returns manifest data through a source turn. Hook, keybinding,
   buffer-change, and prompt events dispatch source wrappers that enter the
   plugin namespace, seed their arguments, and call the named program under their
   requested policy. Lifecycle hooks already inside a command frame call the named
   program inside that frame; they do not create a fresh value turn.
5. **Use wrappers, not stringly values.** The wrapper source is generated only
   from engine-owned names and fixed templates. User data enters as seeded values.
   User text is never spliced into a wrapper as syntax.
6. **Delete the host-facing value door.** After startup, prompt, plugin factory,
   and framed plugin hooks/keybindings all dispatch through `run_source_turn`,
   remove `Shell::run_value_turn`. `run_built` may remain as a private framed
   scaffold inside core; the public host evaluation surface is only
   `run_source_turn` plus `TurnRequest` / `TurnReport`.

### Phase 1 — Protocol over the identity transport

**Introduce the protocol while front-end and engine still share one process.**
The first transport is identity: no PTY bridge, no VM, no serialisation cost on
the hot path. Its job is to make the seam's vocabulary real before moving it
across a boundary.

- **Define the frame algebra.** The protocol has four frame families:
  `Attach` / `Detach` for the session terminal endpoint, `Dispatch` for one
  source turn, `Event` for engine→front-end observations, and `Control` for
  front-end→engine interruption. `Dispatch` carries source plus the serialisable
  shape of `TurnRequest`, including the turn-local seed map; `Control` carries
  `cancel`, `suspend`, `resume`, and `resize`.
- **Mirror host-facing data deliberately.** Do not expose core structs as the
  protocol. Define protocol mirrors for `TurnRequest`, seeded data,
  `TurnReport`, captured byte output, diagnostics, status, and surfaced values.
  The identity transport may borrow or elide copies internally, but every field
  is one that a remote transport can encode.
- **Route `surface` through event frames.** Live `surface` emissions become
  ordered `Event::Surface` frames for the active dispatch; deferred boundary
  batches become `Event::BoundarySurface` frames. Both carry the same surfaced
  value vocabulary and use the same host decoder.
- **Make cancellation out-of-band immediately.** A dispatch wait must hold an
  independent control sender, even in process. Ctrl-C / Esc in the front-end sends
  `Control::Cancel`; resize sends `Control::Resize`; job suspension sends
  `Control::Suspend`. The identity implementation may translate those directly
  into the existing `CancelScope`, foreground-child interrupt, and terminal-size
  updates, but the call shape is already duplex.
- **Convey the terminal lease through attach.** The identity attach grants the
  existing login tty endpoint to the engine session. It may be represented by the
  current fds and `TerminalLease` internally, but the protocol operation is still
  "front-end conveys terminal endpoint"; there is no engine-side startup mint in
  the semantic interface.
- **Keep completion a report, not stream closure.** `TurnReport` is the dispatch's
  terminal frame. Dropping an event sender, draining a surface stream, or losing a
  bridge is never how a turn completes.

After Phase 1, the in-process shell and exarch hosts already drive the engine
through the same attach/dispatch/event/control vocabulary a remote transport will
use. Phase 2 can move that vocabulary across a process boundary without changing
what a turn is.

### Phase 2 — Process transport on one host

**Move the engine into a session-lived child process on the same host.** This is
the first non-identity transport: a real process boundary, one kernel, no VM, and
no new semantics.

- **Re-exec the same binary as an engine.** Honour [[invariants/single-binary|single-binary]]:
  the front-end starts the engine by re-execing `ral`/`exarch` in an internal
  engine mode, not by shipping a sibling daemon. The child boots one `Shell` and
  keeps it for the session.
- **Use one full-duplex framed channel.** The parent and engine speak the Phase 1
  frame algebra over a local socket or pipe pair with length-delimited frames.
  `Dispatch` waits for a `TurnReport`, but the control half remains writable while
  that wait is outstanding. A wedged dispatch must still be cancellable.
- **Serialise only the protocol mirrors.** The wire carries protocol
  `TurnRequest`, seeded data, `TurnReport`, diagnostics, captured bytes, status,
  and surfaced values. It does not carry `Shell`, `Mobile`, handler stacks,
  grants, closures, or core `Value` programs. Session state lives in the engine
  process.
- **Pass the terminal endpoint at attach.** On Unix, the front-end conveys the
  terminal endpoint with fd passing over the local channel; the engine turns that
  endpoint into its session `TerminalLease`. On platforms without fd passing or
  foreground job control, attach either conveys no lease or uses the platform's
  existing no-foreground semantics. There is still no engine-side terminal probe
  as authority.
- **Translate signals only at the front-end.** The front-end owns OS signal
  handlers for its terminal. Ctrl-C / Esc / resize / suspend become protocol
  `Control` frames; the engine applies them to its current turn and foreground
  child. No raw signal-handler pointer crosses the process boundary.
- **Define attached-pair death.** In the baseline deployment, if the front-end
  dies, the channel closes; the engine treats that as `Detach`, cancels any
  in-flight turn, reaps foreground children, restores terminal state, and exits.
  A supervised engine that survives detach is a later lifecycle layer, not part of
  this phase.

After Phase 2, the design has crossed a real process boundary while preserving the
whole-turn cut. Phase 3 can replace the same-host terminal endpoint with a
PTY-backed endpoint and put the engine in a VM.

## See also

[[decisions/260619_terminal-lease|terminal-lease]] (the lease whose provenance and
referent this generalises; carrier discipline unchanged),
[[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]] (the seam this makes
transport-parametric),
[[decisions/260610_host-embedding-api|host-embedding-api]] (embedding the engine;
the front-end role's two inhabitants),
[[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]] (the
calculus/plumbing line this completes outward),
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
(the representation boundary serialisation forces),
[[decisions/260617_sandbox-external-children|sandbox-external-children]] (the
whole-turn boundary blessed, grant-body IPC killed — the lesson),
[[decisions/260601_reduced-authority-witness|reduced-authority-witness]] (the
witness discipline §2 completes),
[[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]] and
[[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]] (the cancel
message's semantics),
[[decisions/260617_turn-local-state|turn-local-state]] (terminal access rides
`TurnState`),
[[decisions/260622_surface-delivers-itself|surface-delivers-itself]] (the
surface vocabulary and boundary delivery regime that cross the seam),
[[design/exarch-architecture|exarch-architecture]],
[[design/two-enforcers|two-enforcers]], [[design/grant|grant]],
[[map/core/transport|transport]] (the existing wire form to build on),
[[map/core/io-process|io-process]].
