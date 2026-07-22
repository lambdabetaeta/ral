---
status: active
---

# The seam completes: an answered Enquiry channel beside Surface, first-order payloads on every rail

**The host seam gains its fourth channel — an engine→host request-response
rail, `Enquiry → Answer`, the answered dual of `Dispatch → Report` — and every
channel's payload is unified on one type, `FOValue`: a serialisable
*first-order* ral value, data all the way down.** `surface` stays one-way;
nothing about its two delivery regimes changes — but its vocabulary is
opened: the render document becomes one *class* among several (facts, state,
presentation, and classes not yet invented), retiring the claim of
[[decisions/260619_surface-carries-documents|surface-carries-documents]] that
documents are what the channel carries. The channel set is then closed by
symmetry: each direction has one answered channel and one one-way channel,
and every future host facility is a new *class* on an existing channel, never
a new channel. This is the seam shape that made it *possible* for the
provider-side tools this ADR found stateful (`amnemon`/`mnemon`, `agents`,
`message`, `agent_cancel`, the `schedule` family, `commit`/`verify_commitment`,
and — per the corrected litmus below — `reply`) to migrate into shell
builtins; that migration has since landed in full
([[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]]),
and it is designed against the real target: the front-end (REPL or exarch) on
a host machine, the engine and its shell in a VM, with two-way communication
robust across that boundary.

Builds directly on
[[decisions/260628_host-seam-transport-parametric|host-seam-transport-parametric]]
(the frame algebra and its two transports, both landed in `core/src/transport.rs`
and `core/src/engine.rs`) and
[[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]]
(the migration this makes possible — landed, riding this rail).

**Phases A and B are implemented.** Phase A and its folded simplifications
landed as `4cb867f`–`4751ae8`; Phase B landed as `a5db7a8` (the wire
enquiry rail), `70ad17f` (engine shell parity), `187dc8e` (probes),
`2b3b1e1` (pushed notices, `Kind::Done`), and `da110a8` (integration
cleanup).
Two vocabulary notes for readers coming from the code: an intervening
refactor (`4751ae8`) retired the seam's "mirror" naming and made the
transport's `Turn` the canonical turn vocabulary — so `AnswerMirror`/
`ErrMirror` below landed as `Result<FOValue, EnquiryError>`, the boundary
regime landed as `Event::DeferredSurface`/`DeferredSink`, and the probe
landed as `Frame::Probe`, not `Turn::Probe` (§5 records why). Phase C, the
coherence gate (Phase B item 6 as originally planned), and §4.3's
register re-typing remain open; §"What remains, and in what order" carries
their dependency structure. Riding the rail is a separate track from
building it: the tool migration this rail exists for has landed in full
under the identity transport — `exarch/src/tools.rs` shrinks to `ral` alone
([[map/exarch/tools|tools]]) and the desk (`exarch/src/desk.rs`) answers
every harness class — independently of whether Phase C or the coherence
gate ever land; the wire dependency is named in that ADR's "Wire mode"
section.

## The setting: the seam as built, and its empty cell

`core/src/transport.rs` sorts every frame that crosses the seam by
*(direction × blocking)*:

|                   | answered (call)                       | one-way (notice)                          |
| ----------------- | ------------------------------------- | ----------------------------------------- |
| **host → engine** | `Dispatch(id, Turn)` → `Event::Report` | `Control` (cancel / suspend / resume / resize) |
| **engine → host** | *(empty)*                             | `Event::Surface` / `Event::BoundarySurface` |

Both transports exist: `IdentityTransport` runs the turn synchronously on the
dispatching thread under a poison-free session lock; `WireTransport` re-execs
the same binary as an `--engine` child on a socketpair (fd 3), with a
front-end reader thread live during dispatch and an engine-side reader/worker
pair (`core/src/engine.rs`) that keeps `Control` deliverable mid-turn. The
protocol version is checked at `Attach`.

The migration of tool calls into shell primitives
([[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]])
is what demands the empty cell. The stateless tools went cleanly
(`exarch/src/agent_builtins.rs`: `view-text`, `grep-files`, `edit`, `fff`)
because they never need the host: they act on the filesystem and *announce*
through `shell.surface(...)`. Everything left in `exarch/src/tools/` takes
`&mut Agent`, the `AgentRegistry`, the parent `Mailbox`, the provider handle,
the policy narrower — front-end state the engine cannot and must not hold —
and every one of them hands a value back (a start receipt, a listing, a
confirmation, a verdict). A builtin that consumes an answer needs a rail on
which an answer can arrive. That rail does not exist. Building the rail is
this ADR's business; riding it is not — the tools stayed provider-side until
the migration ADR's own bench-gated schedule moved them (landed 2026-07-16;
[[map/exarch/tools|tools]] and [[map/exarch/builtins|builtins]] carry the
current split).

The dual shape — mutating `surface` into a request-response channel instead —
is rejected outright, for two load-bearing reasons visible in the code.
First, emit must never block the evaluator: surface is fired mid-turn from
deep inside evaluation (`Shell::emit_io` fires on every redirect read, every
exec settlement), and a sink that could carry a response would make every
slow front-end a stalled script and every emitter a potential deadlock
against the drain loop. Second, half of surface's deliveries have no producer
left to resume: the boundary regime (`Event::BoundarySurface`) delivers a
detached worker's batch *after* the worker settled — there is structurally
nobody to hand an answer to. The one-way-ness is also an attenuation: a
detached worker holding the boundary sink cannot interrogate or block the
host, by construction. So the answered channel is a sibling, not a mutation.

## Three findings from the working tree

The design below is argued from the code as it stands; three findings do
most of the arguing.

1. **The frame algebra is one cell short — and the missing traffic already
   flows, just not through the seam.** Beside the turn traffic there is the
   host's accessor traffic through `IdentityTransport::shell_mut()`:
   `exarch/src/agent.rs` reaches through the transport ~20 times per session
   loop — `mobile_snapshot`, `take_worker_reap_notices`,
   `prune_idle_bindings`, `take_large_binding_notices`, `workers`,
   `worker_count`, `binding_count`, `leased_binding_count`,
   `advance_worker_epoch`, `cancel_workers`, `fork_session`, `env_var`,
   `cwd`, `grant_depth`. None of this crosses a wire. The evidence that it
   matters: under `RAL_WIRE`, `Agent::run_shell` dispatches the turn to the
   wire engine but every accessor — including the durable snapshot taken
   *for panic recovery of that very turn* — still reads the co-resident
   identity shell, which never ran anything. The wire mode is incoherent
   today, and no design that ignores the accessor flow can fix it. §5 gives
   this traffic its lawful shapes (pushed notices, probe turns, lifecycle
   frames — and one relocation: durability goes engine-side, off the seam).

2. **The payload discipline exists only as a runtime check.** The seam's
   data-only rule is real (`SerialValue::from_ground` rejects non-ground
   values loudly), but the *type* does not enforce it: `SerialValue` carries
   `Lambda`/`Block` variants because `child_eval` legitimately ships closures
   between pipeline helpers within one kernel. The seam borrows that type and
   re-checks first-orderness at every edge — under three names for one
   concept (`is_ground`, `from_ground`, `into_ground`), every fallible use a
   silent-drop arm. §2 splits the two uses and collapses the vocabulary:
   `FOValue` — first-order *by construction*, its extension slot uninhabited
   — becomes the seam payload everywhere; `SerialValue` remains the
   closure-capable mirror for the in-kernel helper IPC only, as the alias
   `FOValue<Closure>`.

3. **`done` loses its structure at the seam's edge, and one register stores
   presentation as state.** `decode_surface` maps a worker's `` `done ``
   completion to a bare `Kind::Card` — the forensic record keeps only ink,
   unlike `Kind::Io` which records the fact beside its rendering. And the
   commitment register (`PinDigest`, `Agent::commitment_card`) stores a
   rendered `Card` that the verifier orchestration later *re-reads as data* —
   state persisted as presentation. §4 re-types both.

## The decision

### 1 — Four channels, symmetric names

|                   | answered (call)          | one-way (notice)            |
| ----------------- | ------------------------ | --------------------------- |
| **host → engine** | **Dispatch → Report** — the turn: clocked, walled, cancel-scoped | **Control** — imperative, out-of-band, unordered |
| **engine → host** | **Enquiry → Answer** — nested inside a turn, inheriting its clock | **Surface** — indicative, ordered before the Report; live or boundary-deferred |

The names are chosen as one register — the correspondence of a dispatch rider
and the office that answers enquiries — and the new pair is deliberately the
dual of the old: *Dispatch/Enquiry* are the questions, *Report/Answer* the
returns. (`Enquiry`, with the British spelling, is the term of art for a
question put to an institution that is obliged to answer; that is exactly
this channel's contract.)

The symmetry that holds: **each direction owns one answered channel and one
one-way channel**. The asymmetries that remain are semantic and deliberate,
not accidental:

- *Mood*: the downward notice channel is imperative (`Control` commands:
  cancel, resize), the upward one indicative (`Surface` informs: this
  happened, this is now true). They are not renamed into a fake mirror pair
  because they do not mirror.
- *Ordering*: `Surface` is sequenced within its dispatch, before the Report —
  that is what makes the transcript truthful. `Control` is deliberately
  unsequenced — it must race past an in-flight dispatch or cancel could not
  work. Wanting an ordered control frame or an out-of-band surface event is
  the sign a thing belongs in the other cell.
- *Clock*: `Dispatch` defines the turn clock and holds the session lock;
  `Enquiry` happens inside a dispatch and inherits everything — it gets no
  wall of its own, and a `Control::Cancel` must wake an engine parked on an
  unanswered enquiry.

Code names, mirroring the surface plumbing exactly:

```rust
// core/src/types/shell/mod.rs — beside EventSink / SurfaceSink / BoundarySink
pub trait EnquiryDesk: Send + Sync {
    /// Answer one enquiry. Blocking, short by contract (§3).
    fn enquire(&self, req: FOValue) -> Result<FOValue, Error>;
}
pub type Desk = Arc<dyn EnquiryDesk>;

// core/src/transport.rs — what an answer preserves across the seam.
// Deliberately not folded into types::Error, which carries a `loc` core
// has no business shipping across the seam; the location is stamped
// engine-side, at the enquiring builtin.
pub struct EnquiryError { pub message: String, pub status: i32 }
// every answer position carries Result<FOValue, EnquiryError>
```

`TurnRequest` gains `desk: Option<Desk>`, turn-local like `surface`;
`Shell::enquire` is the public door beside `Shell::surface`; the absent-desk
error is the bare REPL's honest answer: **"this host answers no enquiries."**
On the wire the channel is `Event::Enquiry(EnquiryId, FOValue)` upward and
`Frame::Answer(DispatchId, EnquiryId, Result<FOValue, EnquiryError>)`
downward — the first answered frame in the front-end→engine direction, which
is precisely the quadrant filling in. The answer's error arm keeps message
*and* status, so a refused enquiry raises the same error under both
transports; its location is stamped engine-side, at the enquiring builtin.

Two auxiliary host→engine shapes complete the accessor story in §5:
**probes** (pure, boundary-time reads answered through the existing
`Dispatch → Report` rail's `Event::Report` — a class family sharing that
rail's correlation, not a new channel; landed) and session lifecycle frames
(`Fork` / `Clear`, beside `Attach`/`Detach`; Phase C). The table above is
the whole turn-time algebra.

### 2 — One payload type: `FOValue`; one word; the envelope stays typed

Every payload position on every channel carries the same type — and the type
is not a wrapper over `SerialValue` but the structure both mirrors share,
factored once:

```rust
// core/src/serial.rs
/// A serialisable *first-order* ral value: unit, bool, int, float (by bits),
/// string, bytes, and lists/maps/variants thereof — data all the way down.
/// `X` is the extension slot, uninhabited by default: a bare `FOValue` is
/// first-order by construction, not by a checked invariant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]  // today's SerialValue shape, kept
pub enum FOValue<X = NoExt> {
    Unit, Bool { value: bool }, Int { value: i64 },
    Float { value: f64 /* by bits */ },
    String { value: String }, Bytes { value: Vec<u8> },
    List { items: Vec<FOValue<X>> },
    Map { entries: Vec<(String, FOValue<X>)> },
    Variant { label: String, payload: Option<Box<FOValue<X>>> },
    Ext(X),
}
pub enum NoExt {}                          // uninhabited: no Ext arm exists

/// What the in-kernel helper IPC adds: closures with interned scopes.
pub enum Closure { Lambda(SerialLambda), Block(SerialThunk) }
pub type SerialValue = FOValue<Closure>;

impl TryFrom<&Value> for FOValue { /* the ONE first-order check */ }
impl From<FOValue> for Value      { /* total: first-order ⊂ Value */ }
impl FOValue {
    /// The embedding, total. Inherent, not `From`: a `From<FOValue> for
    /// FOValue<X>` impl instantiates at `X = NoExt` to the reflexive case
    /// and collides with core's `impl<T> From<T> for T` (E0119).
    pub fn embed<X>(self) -> FOValue<X> { /* first-order ⊂ every extension */ }
}
```

The vocabulary collapse is as much the point as the type. Today the seam
speaks three names for one concept — `Value::is_ground` (the predicate),
`SerialValue::from_ground` (check + encode), `SerialValue::into_ground`
(decode + re-check) — and a naïve `FOValue` would have added a fourth pair
(`from_value`/`into_value`). All of them go, with one correction to the
call-site sweep: `run_hook`'s ground loop is not merely a seam position —
it guards five *direct* callers that never cross a transport (the REPL's
startup and prompt hooks, the plugin runtime's two dispatch sites, the
binding ledger's hook), and deleting the loop alone would unguard them. So
`run_hook`'s parameter itself becomes `Vec<FOValue>`: the loop is
unwritable rather than deleted, the seam's decode is the identity, and each
direct caller does the one fallible `TryFrom` at its own edge — where a bad
argument gets a diagnostic naming the caller, not the hook. The predicate
and both method pairs are **deleted, not wrapped**. What remains is one word —
*first-order* — and zero new method names: the check lives only inside the
`TryFrom` impl (the recursion *is* the predicate), and decoding is
`Value::from(fo)`, total. Totality is not cosmetic: every current decode
site is a silent-drop arm (`if let Ok(v) = val.into_ground()` — a value that
fails to decode simply vanishes from the surface stream, or worse, from a
hook's *positional* argument list); with a total conversion those arms
cannot be written. The partial direction survives only at the doors, never
in a sink: `Shell::surface` converts once as a value enters the rail (a
non-first-order emission is dropped there with a `SystemNote` — the same
treatment an unrecognised class gets), the result mirror already refuses a
non-transportable turn result loudly, and `EventSink::emit` /
`BoundarySink::deliver` take `FOValue`, so the four sink-side drop arms
delete with the decode arms and a sink that loses a value can no longer be
written.

`SerialValue` survives as the alias `FOValue<Closure>`, keeping its
closure-capable form for what actually needs it — the `child_eval` /
pipeline-helper IPC that ships lambdas with interned scopes *within one
kernel* (`from_runtime`/`into_runtime` and the `InternCtx` machinery are
untouched). The serde repr keeps today's internally-tagged, struct-variant
shape (`tag = "kind"` — tuple variants like `Bool(bool)` cannot carry an
internal tag), so the nine data variants are byte-identical whether read as
`FOValue` or as `SerialValue`; only `Lambda`/`Block` change layout as they
move under the `Ext` arm, which is free — the helper IPC lives and dies
inside one process pair.
The seam stops borrowing it: `Event::Surface`, `Event::BoundarySurface`,
`Event::Enquiry`, `Frame::Answer`, `Turn::Hook` args, and `ResultMirror::Ok`
all carry bare `FOValue`. The minimal alternative — a second flat enum
duplicating the nine data variants — is rejected: ~40 duplicated lines and a
subset relation maintained by review instead of by the type parameter.

The **envelope** — the frame enum, correlation ids, `Attach`'s protocol
handshake — stays a closed set of Rust serde types. This split is the design:

|                          | first-order ral values (`FOValue`) | bespoke Rust types per operation      | JSON (`serde_json::Value`)          |
| ------------------------ | ---------------------------------- | ------------------------------------- | ----------------------------------- |
| new operation costs      | a new variant class + decoder arm  | a frame-enum change on **both** ends  | a new object shape + decoder arm    |
| cross-version skew       | unknown class → clean per-call error | decode failure → channel poisoned    | unknown shape → clean error         |
| static checking          | at the builtin's type rule + decoder | full, but only within one binary     | none                                |
| carries ral data         | natively (variants, bytes, NaN by bits) | natively                          | lossily (no variants; `null` for NaN; bytes as arrays) |
| model/script legibility  | the language's own literals        | invisible to scripts                  | a second vocabulary beside ral's    |
| blast radius of a change | one class                          | the protocol                          | one shape, but conventions drift    |

The middle column is right for the *channel structure* — which cell a frame
is, what correlates with what, what closes a dispatch — because that
structure must be exhaustive-matchable and version-gated: an envelope change
**is** a protocol change and must be refused loudly at `Attach`
(`PROTOCOL_VERSION`, already checked in `core/src/engine.rs`). The left
column is right for the *payloads*, because operations will keep arriving
(the whole migration is a stream of them) and each must cost a decoder arm,
not a protocol rev. JSON is rejected as a **vocabulary** — ral values already
have a canonical JSON *encoding* through `serde` (the wire codec is
length-prefixed JSON today, `subprocess_codec`, and stays so; swap to CBOR
later if profiling asks — an encoding choice, invisible to this design).

The extension law, stated once for every channel: **a new host facility is a
new class — a variant label on `FOValue` — on an existing channel, plus a
decoder arm at the receiving end. It is never a new channel, never a new
frame family, never an envelope change.** An unrecognised enquiry class
answers `Err` naming the class (the engine's builtin raises it as an ordinary
catchable error); an unrecognised surface class is dropped with a
`SystemNote` (today it is dropped silently — the note is new). That is the
forward-compatibility story across version skew between machines, and it is
the same story LSP and MCP chose (unknown method → error response; unknown
notification → ignore).

### 3 — Enquiry semantics: the laws

**Containment.** An enquiry is inside the turn: same wall, same foreground
`CancelScope`, no clock of its own. `Control::Cancel` must wake an engine
parked on an unanswered enquiry; the park fails with a cancellation `Error`
that the builtin raises like any other failure. Containment also fixes the
spawn direction: the desk flows into same-thread bodies with the rest of
the turn frame (`TurnState::inherit_from`) and is left `None` on a detached
worker, exactly as `terminal_access` is left `Denied` — a worker outlives
its turn's Report, so a worker that could enquire would break the ordering
law (§6.5), the boundary attenuation (§"The setting"), and, under the wire,
itself: parked on a rendezvous whose drain loop stopped listening at the
Report, until the lease reaper kills it. `spawn { agent-start … }` gets the
honest absence error.

**Identity binding — a direct call, never frames.** Under
`IdentityTransport`, `dispatch` runs the whole turn on the calling thread and
the front-end drains events only after dispatch returns
(`exarch/src/shell_eval.rs::run_shell`); an enquiry routed through the event
channel in-process would park with nobody draining — instant deadlock. So
in-process, the desk is a direct `Arc<dyn EnquiryDesk>` the host installs on
the turn, exactly as `PerDispatchSink` is a direct `EventSink` — wrapped in
the drain-then-handle adapter §6.5's companion law requires, so the handler
never runs ahead of its turn's own surface output. The handler
runs on the dispatching thread, which sits *inside* the host's own
`Agent::run_shell` stack frame — so the desk captures shared handles, never
`&mut Agent` (the migration ADR names that capture set `HostServices`).

**Wire binding — the desk impl is the codec.** The engine-side desk encodes
`Event::Enquiry(eid, req)` onto the wire and parks on a rendezvous keyed by
`eid`; the front-end's existing drain loop (already live during a wire
dispatch — the engine's reader thread likewise) grows one arm: on `Enquiry`,
run the same handler the identity desk wraps, write `Frame::Answer`. The
engine's reader loop grows the dual arm: on `Answer`, wake the parked
rendezvous. The park selects against the turn's foreground scope at the same
cadence the evaluator's other blocking points poll (`CancelScope::is_cancelled`),
so cancel and answer race safely; a late `Answer` for a dead `EnquiryId` is
dropped by id. This direct-object/frames symmetry is exactly how the surface
sink already differs between `PerDispatchSink` and `ChannelSurfaceSink` — the
desk is transport-parametric by the same construction.

**Correlation from day one.** `EnquiryId(u64)`, fresh per enquiry, carried on
both frames beside the `DispatchId`. A sequential script issues one enquiry
at a time, but ral has pipelines and workers; the moment two stages can each
enquire, outstanding enquiries overlap. The id costs nothing now and spares
the LSP-style retrofit.

**Reentrancy law.** A desk handler must never take the session lock — not
`dispatch`, not `shell_mut`/`with_shell`, not a probe: identity would
deadlock on the lock its own stack holds; the wire engine would answer
"engine busy". The law is enforced, not documented: `IdentityTransport`
stamps the dispatching thread for the duration of `dispatch`, and every
lock-taking door panics with a didactic message when entered from that
thread — the wedge becomes a loud, named failure at the exact call that
would have hung. The law binds `on_surface` too: §6.5's companion runs it
inside dispatch under identity (the drain-then-handle adapter), so a
surface renderer that reaches for the session lock is the same wedge —
caught by the same thread-stamp panic. Sub-agent sessions are forked
engine-side and adopted by id (the migration amendment's nursery), so the
`agent-start` handler never needs the parent session at all.

**Duration discipline.** The desk is for short exchanges: a start receipt, a
ledger read, a confirmation, a verdict. It is not the mechanism for
long-running results — a sub-agent's reply keeps arriving through the inbox
at the turn boundary exactly as `spawn_async` delivers it today; the handler
for `agent-start` adopts the engine-forked child session onto its worker
thread and answers with the receipt immediately, the same shape
`dispatch_spawn` already has. The litmus:
*surface* says "the host may look at this"; an *enquiry* says "this script
cannot take its next step without the host's answer"; "I want it eventually"
is the inbox.

> **Corrected 2026-07-16.** The enquiry half of this litmus was first glossed
> as "the caller genuinely consumes the answer" — a special case of the real
> law, not the law itself, and too narrow to justify promoting `reply`. The
> fixed wording: **promote a verb only when the caller can observe and act on
> the host's answer — value or refusal — within the turn.** `reply`
> ([[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]])
> is the case that exposed the gap: nothing downstream computes with a
> reply's *value*, so the narrower gloss would have kept it a tool — but a
> non-returning agent must observe the *refusal* to proceed correctly, and a
> non-first-order payload must fail the call, not the episode, so the
> refusal arm alone satisfies the corrected law.

**At-most-once, no seam-level retries.** A dispatch, an enquiry, an answer
each cross once. Idempotency is not assumed anywhere; a broken transport
fails the turn (identity: impossible; wire: death flag), never replays it.

**The class discipline, with one worked example.** This ADR fixes only the
*form* of an enquiry class — an `FOValue` variant validated on both ends (the
builtin's type rule engine-side, the desk host-side; never trust one end) —
and pins it with one example: `` `agent-start [session, kind: `amnemon|`mnemon,
prompt, title, permissions] `` answered by `` `started [id, title, log-dir] ``
— `session` is the nursery id of the engine-forked child (the fork precedes
the enquiry; see the amendment) — the child's eventual reply still arriving
through the inbox at a turn boundary, exactly as `spawn_async` delivers it
today. The full class
vocabulary — the spawn, message, schedule, and commitment families, the
handler refactorings, and `reply` (folded in, not carved out, once the
litmus above was corrected) — was the migration's to own, and has landed:
[[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]]'s
amendment.

**Authority is enforced at the desk.** Whichever classes eventually arrive,
the wall is host code holding the real state (the spawn-fuel gate, the
`--allow-schedule` grant, the protected keyspace): a visibility filter —
the `tools_for` gating this replaced — is not an authority check once the
engine is a different machine. The desk refuses, with the tools' own
didactic texts.

### 4 — Surface carries classes: facts, state, presentation — and more

This section retires the titular claim of
[[decisions/260619_surface-carries-documents|surface-carries-documents]].
Surface does not carry documents; **surface carries classes of first-order
values, and the render document is one class**. The document's internal
discipline — the open card set over a closed, Bertin-disciplined mark set —
survives untouched *inside* the presentation class; what retires is the idea
that presentation is the channel's vocabulary rather than one entry in it.
The set of classes is open by the extension law of §2: a channel that today
carries facts, state, and presentation can tomorrow carry progress classes,
approval requests' notices, resource pressure — a decoder arm each, never a
new channel.

The inventory as built, from `decode_surface` / `card.rs` / the host-side
composers, with each item's kind named honestly:

| today's class | kind | producer | defect |
| --- | --- | --- | --- |
| `{io: read\|write\|exec\|grep}` | **fact** | core (`emit_io`: redirects, exec doors, uutils) | none — records the fact beside its card (`Kind::Io{event, card}`) |
| `` `card [marks…] `` | **presentation** | ral kits, exarch builtins (`edit`'s diff) | none — a card is a deliberate act of ink |
| `` `done [outcome…] `` | **fact** | core, at a worker's boundary flush | decoded to bare `Kind::Card` — the record keeps only ink |
| `` `pin ``/`` `unpin `` | **state** | kits, model | the register slot stores a rendered `Card`; the commitment register is then *read back as data* by the verifier |
| reap / prune / large-binding notices | **fact** | host **polls** shell accessors, composes cards itself | never crosses the seam at all; four polled accessors + host-side card composition |
| services ledger, commitment verdicts | **state** | host-authored protected pins | state persisted as presentation (`PinDigest.card`) |

The re-typing principle: **the seam carries facts and state; presentation is
derived at the edge that owns the renderer.** Concretely:

1. **`done` gets its structure back**: `decode_surface` gains
   `Kind::Done { outcome: DoneOutcome, card: Card }` parallel to `Kind::Io`,
   so `transcript.jsonl` records how the worker settled, not just the ink.
   (`DoneOutcome` already exists and already decodes; only the `Kind` flattens
   it away.)
2. **Notices become a pushed surface class** *(landed, with one residue)*.
   The engine runs its own ready-boundary housekeeping:
   `Shell::emit_ready_boundary_notices`, called by the turn door
   (`run_framed`) after `post_exec` and before the turn's frame tears down,
   pushes `` `notice [kind: `reap|`large-binding, …] `` through the settling
   turn's own surface sink — ordered before that turn's Report, exactly the
   §6.5 discipline. `Kind::Notice { notice, card }` records the fact; the
   card is composed exarch-side (`reap_card`, `bindings_pruned_card`,
   `large_binding_card` became private renderers of the decoded notice
   rather than of a polled accessor's return). `take_worker_reap_notices`
   and `take_large_binding_notices` went crate-private to core — under the
   wire there is nothing left to poll for those two. The notice-half of
   `prune_idle_bindings` did **not** move: a prune is a mutation with a
   durability obligation, not a fact report (§"What remains" carries why),
   so `Notice::Prune` is still host-composed from the polled return and
   deliberately has no decode arm. One cadence change is inherent to
   push-not-poll: a worker the background lease chain reaps *between* turns
   has no live sink until the next turn installs one, so its notice queues
   in the ledger and surfaces at the next dispatched turn rather than on
   every idle drive-loop pass.
3. **Registers store `FOValue`, render on read.** A pin's body becomes the
   structured state value; the card is derived by the kit or the host
   renderer at display time. The protected registers become *typed* state:
   the commitment register stores the criteria record the writer produced
   (what `verify_commitment`'s orchestration actually wants — today it parses
   a `Card`'s field rows back into meaning), and the services ledger stores
   the `[id, description, born]` rows it is reconciled from. `Kind::Pin`
   carries `{key, state: FOValue, card}`; the mirror the nudge layer reads
   (`PinDigests`) keeps the state beside the card and stops being
   presentation-only.

This is the answer to "too many things are cards": a card is never again the
*only* representation of something another program will later read — and the
channel it rides is free to grow classes that were never documents at all.

### 5 — Probes and lifecycle: the residual host→engine traffic

After §4 moves the notices to pushes, the accessor flow that remains splits
in two:

- **Pure reads** *(landed)* — `workers()` (the `/resources` fold, the
  service-pin reconciliation), `worker_count`, `binding_count`,
  `leased_binding_count`, `env_var` (`EXARCH_SCRATCH`), `cwd`,
  `grant_depth`. These became **probes**: `Frame::Probe(DispatchId,
  FOValue)`, a sibling of `Dispatch` answered through the existing
  `Event::Report` rail (`Report::Ran`'s `Ok` carries the reading),
  correlated by `DispatchId` like any dispatch — dispatches and probes
  share one id mint, precisely so a probe's Report can never be mistaken
  for a dispatch's. (This ADR originally specced `Turn::Probe`; the mirror
  retirement made `Turn` a struct carrying the wall, sinks, and clock, and
  a probe must have none of those *by type* — so the variant moved one
  level up, to the frame. The extension law of §2 is untouched: a new
  reading is still a class inside the `FOValue` — the landed vocabulary is
  `` `worker-count ``, `` `binding-count ``, `` `leased-binding-count ``,
  `` `env-var [name] ``, `` `cwd ``, `` `grant-depth ``, `` `workers `` —
  decoded by one `answer_probe(shell, req)` both transports share, an
  unknown class answering an error naming it.) One law: **probes are
  boundary-time only** — they serialise with dispatches on the engine's
  single worker rendezvous (a probe while a turn runs answers "engine
  busy", literally the same arm a second dispatch hits). Under the identity
  transport `Transport::probe` takes the session lock (under the
  reentrancy stamp) and calls `answer_probe` directly — zero new
  behaviour. On the wire, the front-end's reply drain obeys §6.5: a frame
  it reads past while waiting for its own Report (a deferred worker's
  batch settling mid-probe) is stashed on the `EventReceiver` for the
  ordinary drain, never dropped. What a probe answers is *data*, not
  handles: exarch decodes `` `workers `` rows into its own `ProbedWorker`
  (id, cmd, `LeaseClass`, running, ages) rather than ever seeing a live
  `WorkerEntry`.
- **Lifecycle operations** — `fork_session` (a spawn's child shell),
  `cancel_workers` + shell replacement (`/clear`), `mobile_snapshot`
  (durability). These are session lifecycle, not turn traffic. The
  direction is decided here, not deferred: **the engine owns session state,
  and a snapshot never crosses the seam** — fork and clear join
  `Attach`/`Detach` as session frames in Phase C; durability relocates
  instead of crossing. The engine becomes multi-session —
  frames gain a `SessionId`, `Frame::Fork(parent) → Event::Forked(SessionId)`
  forks engine-side (the shell state lives there; a fork that round-trips a
  `MobileSnapshot` through the host would ship the whole scope twice for
  nothing), and each exarch `Agent` drives its own session over the shared
  channel — the migration's engine-side fork already builds this table in
  embryo (its nursery). Durability moves with the state it guards: today's
  snapshot is a host-held in-memory clone only because the `catch_unwind`
  is host-side — the rollback-to-last-clean-boundary it implements is the
  state owner's job, so the engine snapshots around its own dispatch and a
  panicked turn reports as a failed turn with the shell already rolled
  back; `mobile_snapshot` leaves the seam's obligations entirely. The
  alternative — one engine process per agent, `SerialEnvSnapshot` shipped
  across — is **rejected**, not kept as fallback: it multiplies VM-side
  processes per sub-agent, and a shipped snapshot carries the scope's
  closures, which law §6.6 forbids on every channel. Lifecycle frames carry
  ids and nothing else.

  > **Corrected 2026-07-22.** The rejection above holds only for the
  > *host-mediated* shape — a snapshot round-tripped through the front-end,
  > which law §6.6 rightly forbids. A parent engine spawning its fork as a
  > child engine process *inside the guest* ships the snapshot same-binary,
  > kernel-to-kernel — the domain `SerialValue` already serves — and never
  > touches the seam; and a process per sub-agent is the same move a ral
  > pipeline already makes per helper, buying kernel-enforced isolation,
  > cancel, and teardown. The multi-session engine this paragraph
  > motivated was built and rejected before shipping; the engine is
  > single-session, the connection is the session, and fork and clear
  > never became session frames:
  > [[decisions/260722_session-is-a-process|session-is-a-process]].

### 6 — Two-machine robustness, as laws

The design criterion is the REPL/exarch on a host, the engine in a VM. The
laws, each with its mechanism:

1. **Version handshake before anything else.** `Attach` carries
   `proto_version`; mismatch refuses the session loudly (already enforced in
   `engine.rs`). The envelope may only change behind a version bump; payloads
   evolve by class (§2), which skew tolerates per-call.
2. **Everything answered is correlated.** `DispatchId` and `EnquiryId` on
   every exchange (a probe is a dispatch, correlated as one); stale answers
   dropped by id, never misdelivered. (Today's `TransportBoundarySink` stamps a boundary batch
   with dispatch id 0 between turns — the correlation discipline already
   half-exists; it becomes law.)
3. **Cancel is always deliverable** — reinstating
   [[decisions/260628_host-seam-transport-parametric|host-seam §5]] verbatim:
   the control channel is independent of the result stream, and now also of
   the enquiry rendezvous: a parked enquiry wakes on cancel (§3).
4. **Liveness is detected, not assumed.** The wire's EPIPE death flag
   generalises: a transport constructor over TCP/vsock arms a read deadline
   or heartbeat frame; a dead peer fails the in-flight turn as cancelled and
   marks the session detached — the same outcome as
   [[decisions/260628_host-seam-transport-parametric|host-seam §"disconnect
   mid-lease"]], reached from either side.
5. **Ordering is per-dispatch FIFO on one duplex stream.** All of a
   dispatch's `Surface` events precede its `Report`; an `Enquiry` precedes
   its turn's `Report`; nothing about a dispatch arrives after its `Report`
   except a boundary batch, which is its own correlated event. One stream
   gives this for free; the law forbids a future second socket from breaking
   it silently. The companion law on the answering side: **a desk handler
   runs only after the host has handled every surface event its turn
   emitted before the enquiry** — the approval classes §4 anticipates
   ("approve the diff I just surfaced") are blind without it. The wire has
   this by stream order; identity does not by default — dispatch runs on
   the drain thread, so the turn's events sit queued until dispatch
   returns, and a handler invoked mid-turn would run ahead of them. The
   identity binding restores it: the desk adapter drains `events()` through
   the extracted loop's `on_surface` before invoking the handler (§"Folded
   simplifications"). The drain is exact, not racy: the emitter is the
   dispatching thread itself, so everything emitted before the `enquire`
   call is already in the queue. (A boundary batch from a concurrently
   settling worker may drain early there; boundary batches are
   independently correlated by this law already.)
6. **First-order or it does not cross.** No fds, no handles, no closures, no
   `Card` structs, no `Capabilities` beyond the `ReqMirror` — the payload
   type enforces it (§2), and the law binds every frame family, lifecycle
   included: session state never crosses (a scope snapshot carries the
   user's defined closures), which is why fork and durability are
   engine-side by §5 — no closure-bearing frame exists to except. The one
   deliberate exception is `Attach`'s terminal endpoint, which is SCM_RIGHTS
   on a same-kernel socketpair and a PTY bridge across a VM (host-seam
   Phase 3, unchanged here).
7. **Reconnect is lifecycle, not seam law** — host-seam §10 stands. A
   dropped front-end cancels in-flight work; a supervised engine may accept a
   re-attach; nothing in the channel design depends on either.

### 7 — Prior art: why this and not gRPC

The comparison the design must survive, since "a host and a subordinate
engine that each tell and ask the other" is a solved genre:

| | this seam | gRPC | LSP / JSON-RPC | MCP | Cap'n Proto (ocap) |
| --- | --- | --- | --- | --- | --- |
| both directions can *request* | yes (Dispatch↓, Enquiry↑) | not natively — client-streaming hacks or a second channel; correlation hand-rolled | yes (requests both ways) | yes (`sampling/createMessage` is exactly Enquiry) | yes (capabilities both ways) |
| one-way notices | Surface / Control, distinct ordering contracts | server-streaming RPCs, per-stream ordering only | notifications | notifications | one-way calls |
| cancellation | out-of-band by law, wakes parked enquiries | per-RPC deadline/cancel, tied to the call | `$/cancelRequest`, advisory | advisory | promise drop |
| in-band ordering (events before result) | law §6.5 | not across RPCs — each stream independent | not guaranteed across methods | not guaranteed | pipelined, but per-capability |
| payload fit for ral data | native (`FOValue`: variants, bytes, NaN-by-bits) | protobuf: no variants-with-payload without wrappers, no NaN discipline, codegen | JSON: loses bytes/NaN/variants | JSON: same | capnp schema, codegen |
| schema evolution | classes on a stable envelope; unknown class = per-call error | field-number discipline, good; but every op is a `.proto` rev + regen on both ends | method strings, good | method strings, good | schema evolution rules, codegen |
| dependency weight | zero new (serde + the codec that exists) | tonic/prost + HTTP/2 stack | zero-ish | n/a (it *is* a JSON-RPC profile) | capnp runtime |
| fit with single-binary invariant | perfect: both ends are this repo, re-exec'd | poor: codegen artefacts, a second IDL as source of truth | good | good | poor |

gRPC is built for polyglot service meshes: many clients, independent teams,
a schema registry as the contract. This seam is two halves of one programme
that must also survive being split across machines. Its needs — engine→host
requests nested in a host→engine call, strict in-band event ordering,
out-of-band cancel, payloads in the language's own data model — are exactly
where gRPC is weakest, and adopting it would mean carrying an HTTP/2 stack
into a VM guest to lose the ordering law. The design that *does* match is
the LSP/JSON-RPC shape — bidirectional requests plus notifications — with a
typed envelope instead of method strings, which is what §1 is. MCP's
`sampling/createMessage` is the direct precedent for `` `agent-start ``: a
subordinate component requesting model work upward from its host, answered.
Cap'n Proto's contribution is kept as the growth path: the desk is a single
capability object, and if the class vocabulary ever grows unwieldy the move
is *several narrower desks on `TurnRequest`* (an agent desk, a schedule desk)
— more capabilities, still four channels. Erlang's lesson is kept as
discipline: the engine never assumes the host answers; the park is bounded
by the turn wall and woken by cancel, engine-side policy, not host courtesy.

## Alternatives considered

- **Make `surface` request-response.** Rejected. Emit is fired mid-turn from
  deep inside evaluation (every redirect read, every exec) and must never
  block; the boundary regime delivers after its producer settled, so half of
  surface's deliveries have no continuation to resume. The one-way-ness is
  also an attenuation — a detached worker holding the boundary sink cannot
  interrogate or block the host, by construction.
- **Builtins as boot-time closures capturing host `Arc`s** (the
  `host_handlers.rs` pattern — which is *correct* where it lives: the REPL's
  `jobs`/`fg`/`bg` are co-resident by definition). Rejected for the agent
  seam: a `Send + Sync` boot closure cannot hold `&mut Agent`; boot-time
  capture gets per-turn context wrong (the generation guard, the per-dispatch
  correlation); and under the wire the closure cannot be constructed
  engine-side at all, so the engine's vocabulary would silently depend on the
  transport. The desk is the same machinery with the capture named once, on
  the rail turn-local state already rides.
- **JSON as the payload vocabulary.** Rejected (§2 table): loses variants,
  bytes, and non-finite floats that `SerialValue` already solved (float bits);
  invites a second, drifting vocabulary beside ral's own.
- **A Rust frame variant per operation.** Rejected: every new operation
  becomes a protocol rev; version skew between machines turns into decode
  failure instead of a per-call error.
- **gRPC / protobuf.** Rejected (§7): wrong strengths, heavy guest
  dependency, breaks the single-binary re-exec story, cannot state the
  ordering law.
- **One uniform async substrate, sync as convention** (Erlang-style).
  Rejected: erases what-blocks from the type; deadlock discipline becomes
  folklore; the Frame enum is the protocol spec and should stay one.
- **Turn-as-coroutine** (suspend the evaluator on enquiry, resume with the
  answer). Rejected as implementation: semantically identical to the parked
  rendezvous, but requires a suspendable CBPV interpreter — a rewrite nothing
  here needs. Parking the dispatch thread *is* the coroutine, implemented
  with an OS thread already spent.

## Implementation plan

Phased so each step lands green. The tool migration is deliberately *not* in
this plan: every phase below leaves the provider tool surface untouched, and
the migration follows later on
[[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]]'s
own bench-gated schedule, riding the rail built here.

### Phase A — the rail, identity-only *(landed: `b164b69`, `c2d8170`)*

1. **`FOValue` and the vocabulary collapse** — `core/src/serial.rs`: factor
   the enum as §2 (generic over the uninhabited-by-default extension slot;
   `SerialValue = FOValue<Closure>`); implement `TryFrom<&Value>` /
   `From<FOValue> for Value`. **Delete** `Value::is_ground`
   (`types/value.rs:264`) and `SerialValue::from_ground` / `into_ground`
   (`serial.rs:383/396`). **Retype** `run_hook` to `args: Vec<FOValue>` —
   the manual ground loop (`driver.rs:473–488`) becomes unwritable, and its
   five direct callers (`repl/session/boot.rs:355`, `repl/prompt.rs:161`,
   `repl/plugin.rs:409`, `repl/plugin/load.rs:245`,
   `types/shell/bindings.rs:658`) encode at their own edge. Move the seam
   positions: `Event::Surface(FOValue)`, `Event::BoundarySurface(Vec<FOValue>)`,
   `Turn::Hook{args: Vec<FOValue>}`, `ResultMirror::Ok(FOValue)`. The decode
   arms at `shell_eval.rs:382/406/462`, `repl/exec.rs:204`,
   `engine.rs:122`, and `transport.rs:523` become total and their
   silent-drop branches are deleted with them. Encode moves to the door:
   `Shell::surface` does the one `TryFrom` (failure → `SystemNote`),
   `EventSink::emit` / `BoundarySink::deliver` take `FOValue`, the boundary
   buffer stores `FOValue`, and the sink-side drop arms
   (`transport.rs:360/773`, `engine.rs:24/53`) delete with them.
2. **The desk trait** — `core/src/types/shell/mod.rs`, beside
   `EventSink`/`SurfaceSink`/`BoundarySink`: `EnquiryDesk` + `Desk` as in §1.
3. **Turn threading** — `core/src/driver.rs`: `TurnRequest.desk:
   Option<Desk>`; `core/src/turn.rs::build_turn` threads it onto `TurnState`
   (new field beside `turn.surface`, `core/src/types/shell/mod.rs:249`),
   restored by the existing turn guard exactly as `surface` is (the test at
   `turn.rs` §"turn-local surface must be restored" gets a desk twin).
   `inherit_from` copies it; the worker build in `builtins/concurrency.rs` —
   the site that flows `boundary` in and swaps `surface` for the deferred
   buffer — leaves it at the fresh `TurnState`'s `None`, as `terminal_access`
   stays `Denied`: §3's containment law in the spawn direction, a stated
   rule, not an accident of the default.
4. **The engine door** — `core/src/types/shell/mod.rs`, beside
   `Shell::surface` (~line 427):

   ```rust
   pub fn enquire(&self, req: FOValue) -> Result<FOValue, Error> {
       match self.turn.desk.as_ref() {
           Some(desk) => desk.enquire(req),
           None => Err(self.err("this host answers no enquiries", 1)),
       }
   }
   ```

5. **Identity transport** — `core/src/transport.rs`: `EngineInner.desk:
   Option<Desk>`; `IdentityTransport::set_desk(&self, Desk)` beside
   `set_boundary`; `dispatch()` installs it into the `TurnRequest` it builds.
   `dispatch()` also stamps the dispatching thread id for the turn's
   duration, and every lock-taking door (`dispatch`, `shell_mut`,
   `with_shell`) panics with a didactic message when entered from the
   stamped thread — §3's reentrancy law, mechanised. Mint `EnquiryId(u64)`
   now (unused by identity, carried by Phase B).
6. **The exarch installation point** — `exarch/src/agent.rs::run_shell`,
   beside `set_boundary`:

   ```rust
   self.transport.set_desk(Arc::new(/* the host's desk */));
   ```

   Per-turn, so whatever the desk captures is fresh — the same reasoning
   `boundary_sink` documents. What exarch's desk actually captures
   (`HostServices`) and which classes it answers are the migration's first
   step, specified in
   [[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]]'s
   amendment; until that lands, exarch installs no desk and `enquire`
   answers its honest absence error, exactly like the bare REPL.
7. **Tests** — desk-absent error text (bare REPL and desk-less exarch);
   turn-guard restore; reentrancy law (a test desk that calls `shell_mut` —
   and one that dispatches — panics with the didactic thread-stamp message,
   never hangs); `enquire` round-trip through a stub desk installed via
   `set_desk`; desk flow (a same-thread thunk body enquires successfully; a
   spawned worker gets the absence error — never a park); ordering
   companion (a desk that asserts the turn's earlier surfaced value reached
   `on_surface` before the handler ran — green under identity now, re-run
   under the wire in Phase B).

### Folded simplifications — the rail lands smaller than it found the tree *(landed: `4cb867f`, `b40fd53`, `4751ae8`)*

Each of these is a deletion or de-duplication in code the phases already
touch; they are part of the plan, not optional polish. (The largest one —
the ground vocabulary and its six silent-drop arms — is Phase A item 1
above.)

- **One drain loop.** `exarch/src/shell_eval.rs::run_shell` and
  `ral/src/repl/exec.rs::execute_input` carry the same ~40-line
  mint-id / dispatch / drain-until-Report loop, and Phase B would grow the
  `Enquiry` arm in both. Extract `run_shell(transport, turn, on_surface,
  on_enquiry) -> ReportMirror` beside the `Transport` trait — the name moves
  down with the loop; exarch's `shell_eval::run_shell` becomes a thin
  wrapper that builds the `ReqMirror` and hands closures in. The ordering
  law (§6.5) gets exactly one implementation — including its companion:
  under identity, the extracted loop wraps `on_enquiry` in a
  drain-then-handle adapter (drain `events()` through `on_surface`, then
  invoke), so both transports answer every enquiry with the same surface
  prefix already delivered. The REPL gains surface rendering and enquiries
  by passing closures rather than by copy-paste, and Phase B's new arm
  touches one site. (Extract in Phase A, extend in Phase B.)
- **One `ReqMirror → TurnRequest` assembly.** `IdentityTransport::dispatch`
  (`transport.rs:499–514`) and the engine worker (`engine.rs:101–113`) both
  hand-assemble the request from the mirror plus their sinks — two sites to
  thread the new `desk` field through. `ReqMirror::into_request(surface,
  boundary, desk, lifecycle)` makes it one, and makes "forgot the engine
  side" unwritable. (Phase A.)
- **One sink family.** `PerDispatchSink`/`ChannelSurfaceSink` and
  `TransportBoundarySink`/`ChannelBoundarySink` differ only in where the
  frame goes; the boundary pair duplicates the `joined` test-and-set latch
  verbatim (`transport.rs:760–770`, `engine.rs:40–50`). Parameterise each
  sink over a frame writer (the mpsc sender or the wire channel) and the
  four collapse to two with one latch helper — and Phase B's `WireDesk`
  does not mint a third pair. (Phase A core-side, Phase B engine-side.)
- **One current-dispatch tracker.** `IdentityTransport` tracks the in-flight
  dispatch twice — `PerDispatchSink.current: Mutex<Option<DispatchId>>` and
  `EngineInner.current_dispatch: Arc<AtomicU64>` — set and cleared in
  tandem in `dispatch()`. One shared atomic (0 = none) read by both sinks,
  and by the desk installation, which would otherwise plausibly add a
  third. (Phase A.)
- **Delete the `RAL_WIRE` experiment now.** `Agent.wire:
  Option<WireTransport>`, its env-var branches (`agent.rs:353–360,
  435–443`), and the dual-transport `if` in `Agent::run_shell`
  (`agent.rs:1762–1768`) implement the incoherent mode this ADR documents —
  turns in the wire child, ledgers and snapshots on the identity shell.
  ~40 lines of live footgun; Phase B rebuilds wire mode behind the
  coherence gate. Landable today, independent of everything else here.
  (Pre-Phase A.)

### Phase B — the wire encoding, probes, pushed notices *(landed: `a5db7a8`–`da110a8`)*

As built, with the deltas from the plan noted:

1. **Frames** — `Event::Enquiry(EnquiryId, FOValue)`;
   `Frame::Answer(DispatchId, EnquiryId, Result<FOValue, EnquiryError>)`
   (§1: the error arm keeps message *and* status, so both transports raise
   the same error). `PROTOCOL_VERSION` is 2.
2. **Engine side** — `core/src/engine.rs`: `WireDesk` (rendezvous map
   `Mutex<HashMap<EnquiryId, Slot>>` + condvar) installed into the worker's
   `TurnRequest` beside `ChannelSurfaceSink`; the reader loop's `Answer`
   arm fills the slot, and a late answer for a dead id is dropped by id.
   The park polls the foreground cancel at the condvar wait's timeout
   (75 ms) through `process::foreground_cancel_cause` — a read-side dual of
   `request_foreground_cancel` added for it, since the parked desk holds no
   `&Shell` to poll through — and a cancelled park raises the same
   interrupted/cancelled/timed-out/aborted vocabulary as every other poll
   point. Shell parity landed as `EngineInstaller { tag, prelude, install }`:
   `run_engine` takes each binary's compiled-in table, blocks on `Attach`
   as the sole legal first frame, resolves the tag (`resolve_installer`,
   pure so the refusal is unit-testable), boots `boot_shell` + the named
   installer, and only then spawns the worker — an unknown tag refuses the
   session as loudly as a version mismatch. Exarch's table maps
   `"exarch-agent"` to `agent_builtins::install_on`; the REPL's maps
   `"repl"` to no installer — and the `ral` binary gained the `--engine`
   entry point it never had (only exarch handled the flag before).
3. **Front-end side** — the *extracted* drain loop (`dispatch_to_report`,
   which the folded simplifications had already made the one site) grew an
   `on_enquiry` handler parameter and the `Event::Enquiry` arm, answering
   through `Transport::answer(...)` — a no-op under identity, where the
   desk is a direct call and the frame never arrives. Both hosts pass the
   honest absence error until the migration installs a real desk.
4. **Probes** — landed as `Frame::Probe`, not `Turn::Probe`; §5 records the
   as-built shape and why it moved (the mirror retirement made `Turn` a
   struct carrying exactly what a probe must not have).
5. **Pushed notices** — `Shell::emit_ready_boundary_notices` at the turn
   door pushes the reap and large-binding classes; `decode_surface` gained
   `Kind::Notice` and `Kind::Done`; the polled call sites in
   `Agent::drain_*` retired and the two accessors went crate-private. The
   prune notice stayed host-composed — §4.2 and §"What remains" carry the
   residue.

The originally planned item 6 — the coherence gate — did **not** land with
Phase B, deliberately: gating before the remaining unlawful accessors can
pass it would be a permanently red configuration. It moved to §"What
remains", which carries its dependency structure.

### Phase C — the machine boundary

Transport constructor over vsock/TCP (+ TLS where the link is untrusted —
`exarch/src/tls.rs` exists), heartbeat/read-deadline liveness (law §6.4),
multi-session frames and engine-side `Fork` (§5), and the PTY bridge of
[[decisions/260628_host-seam-transport-parametric|host-seam Phase 3]].
Engine-side durability (§5) — the turn-boundary snapshot/rollback moving
from `Agent::drive`'s host-held clone into the engine's own dispatch — is
not gated on Phase C: it is what the coherence gate demands of
`mobile_snapshot`, and may land with it.
Register-as-`FOValue` (§4.3) can land any time after Phase A; it touches
`card.rs`/`shell_eval.rs`/`bus.rs` and the commitment orchestration, not the
transport. Neither has landed.

### What remains, and in what order

Four open items survive Phase B, and they are not independent — the
dependency order is the point of this section.

- **The idle-prune residue** (§4.2). The reap and large-binding notices
  moved to pushes because they are pure facts; the idle-prune could not,
  because `prune_idle_bindings` is a *mutation with a durability
  obligation*: its signature hands back the post-prune `MobileSnapshot` in
  the same statement, and the host must adopt that snapshot as its
  panic-recovery baseline atomically with the prune, or a later rollback
  resurrects pruned names. That snapshot is exactly what law §6.6 forbids
  on every channel, and the call runs *between* turns, where no live
  surface sink exists anyway. The unlock is Phase C's durability
  relocation: once the engine snapshots around its own dispatch, the
  prune-then-adopt pairing collapses into engine-internal housekeeping, the
  prune runs at the engine's own ready boundary like its siblings, and its
  notice becomes an ordinary `` `notice [kind: `prune] `` push — at which
  point `value_to_notice` regains the decode arm that was deliberately not
  written (decoding a class nothing emits would be speculative code).
- **The coherence gate** (originally Phase B item 6). Two things stand
  between here and turning it on. First, a construction seam: exarch
  builds `IdentityTransport` unconditionally, and the `RAL_WIRE` env
  plumbing that used to select the wire was deleted as a footgun — some
  deliberate way for the bench harness or a test configuration to hand
  `Agent::assemble` a `WireTransport` has to be decided, not resurrected.
  Second, the gate is only meaningful once the traffic that *cannot* pass
  it has a lawful shape: the prune residue above, and the Phase C
  lifecycle accessors (`fork_session`, `mobile_snapshot`, `cancel_workers`,
  `advance_worker_epoch`). Gating earlier is a permanently red CI lane.
  Once those land, the gate is what keeps the seam honest forever after: a
  future accessor added by reaching through `shell_mut()` fails loudly in
  the wire lane instead of silently reading a shell that never ran
  anything — the `RAL_WIRE` incoherence, made unrewritable.
- **`rc_path` at Attach.** The engine child boots the real shell but still
  ignores the rc file, honestly: rc loading is not `Shell::load_rc` — in
  the REPL it runs through host-side machinery (the `RcCtx`/plugin
  runtime) core does not have and should not grow. Either a core-legal
  "evaluate this file as session bootstrap" path is extracted, or —
  more in the seam's spirit — rc loading is declared a *host* concern and
  the front-end replays rc content as ordinary early dispatches, through
  the same door as everything else. Decide when the REPL-over-wire has a
  live caller; today it has none.
- **Reap-notice latency** — not work, a recorded cost. Between-turn reaps
  now surface at the next dispatched turn (§4.2), by construction of the
  ordering law: a notice with no turn has no dispatch to correlate to and
  no place in the per-dispatch FIFO. If an idle agent's silent transcript
  ever itches, the lawful mitigations are a heartbeat probe on the drive
  loop's idle path or an uncorrelated boundary-batch delivery (the
  dispatch-id-0 shape deferred batches already use between turns) — never
  a return to polling. Wait for the itch.

So: Phase C's engine-side durability unlocks the prune push, which together
with the lifecycle frames clears the last unlawful accessors, which is what
makes the coherence gate worth turning on. The rc question is independent
and waits for a caller.

## Consequences

- The engine→host quadrant is filled by one named channel with one trait,
  one pair of frames, and one payload type; every stateful tool migration is
  thereafter a class + a decoder arm + a builtin, never plumbing.
- The rail lands LoC-negative before the first enquiry class exists: the
  ground vocabulary (three names, ten silent-drop arms — six decode, four
  encode — one manual argument loop), the duplicated drain loop, the duplicated request
  assembly, the duplicated sink family and latch, the doubled dispatch
  tracker, and the incoherent `RAL_WIRE` plumbing all delete in its wake.
- The bare REPL's story is a clean absence: no desk, honest error. The REPL
  keeps its captured-closure builtins — co-residence is its definition, and
  `host_handlers.rs` is not debt.
- The wire mode stops being fictional: the engine child speaks the same
  shell as the identity path, and the migrated accessor traffic crosses the
  seam lawfully. "Or fails loudly" waits on the coherence gate (§"What
  remains") — until it is on, the prune and lifecycle accessors still read
  through `shell_mut()`, lawfully under identity, unexercised under the
  wire.
- The forensic record strengthens: `done` and the notice family are recorded
  as facts, registers hold data, and cards become presentation — derived,
  never load-bearing, one class on a channel that carries facts and state as
  peers and has room for classes not yet invented.
- [[decisions/260619_surface-carries-documents|surface-carries-documents]] is
  amended: its mark discipline survives inside the presentation class; its
  titular claim — that the document is the channel's vocabulary — retires.
- The tool migration becomes possible without further plumbing, on
  [[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]]'s
  own schedule and bench gate; if the ral-fluency tax eats the composition
  gains there, the tools simply stay — and the desk still earns its keep as
  the seam every host facility (§4's classes, §5's probes) rides.
- The seam's theory is statable in one sentence: dispatch installs the
  handler and delimits the scope; an enquiry is an operation with a one-shot
  resumption; a surface emission is an operation resumed immediately with
  unit; a boundary batch is an operation performed after its delimiter
  returned, which is why it structurally cannot be answered.

## See also

[[decisions/260628_host-seam-transport-parametric|host-seam-transport-parametric]]
(the frame algebra, the two transports, the whole-turn cut, cancel
out-of-band, disconnect semantics — everything this ADR extends),
[[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]]
(the migration this makes possible — amended with the class vocabulary, the
desk's `HostServices` and handlers, and the bench-gated schedule; deferred),
[[decisions/260622_surface-delivers-itself|surface-delivers-itself]] and
[[decisions/260622_sync-surface-async-notify|sync-surface-async-notify]] (the
two delivery regimes that stay untouched),
[[decisions/260619_surface-carries-documents|surface-carries-documents]]
(amended: the document becomes one surface class among several; its
closed-mark discipline survives inside that class),
[[decisions/260622_surface-pins-state|surface-pins-state]] (the register §4.3
re-types to carry `FOValue`),
[[decisions/260703_protected-commitment-pins|protected-commitment-pins]] (the
protected keyspace whose state stops being a parsed card),
[[decisions/260705_session-ledger|session-ledger]] (the notice and
probe populations §5 gives seam shapes),
[[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]] (the gate the desk
enforces), [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
(the representation boundary `FOValue` mechanises).
