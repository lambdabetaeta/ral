---
status: accepted
---

# The transcript is a value

**The provider-facing history is a persistent value shared by reference across
the fold, and owned `genai` wire values are manufactured at exactly one door,
at most once per HTTP attempt.** A live 36-session fleet run showed ~12 MB of
`record.jsonl` becoming a 2303 MB peak footprint — `MALLOC_SMALL` churn, no
leak. The class of defect: **the manufactured view**, an O(|history|) *owned*
materialisation of immutable data recomputed inside the O(steps × attempts)
loop — up to three whole-history deep copies per deliberation step
(`render_messages`, `build_cached_request`, and a clone inside the retry
closure), the last of which bought nothing even on its own terms: `genai`
consumes a `ChatRequest` by value per call, so a kept template existed only to
be cloned again by a retry.

## The property

> **Sharing follows immutability.** The committed transcript crosses module
> boundaries only by shared reference. An owned whole-history value exists at
> exactly one named door, manufactured at most once per HTTP attempt — and at
> one other, the mnemon child seed, where ownership genuinely transfers.

## Rejected candidates

- **Borrowed projection** (`&ChatMessage` iterator over the ledger). The
  projection renders under `LogCell`'s lock, which panics on contention rather
  than blocking; a borrow held across a multi-minute network round trip would
  pin that lock while the streaming path itself needs to take it. The
  lifetime forbids it. Also incomplete: the projection interpolates repair
  stubs that exist in no ledger slot, so it is not a pure subsequence to
  borrow.
- **Owned snapshot per step.** The status quo — the defect.
- **Message-level interning** (`Protocol` holding `Arc<ChatMessage>`). Rejected
  because it puts sharing plumbing into the verbatim wire payload
  `Protocol` is deliberately kept as, and nests two sharing granularities once
  spans are also shared for no gain the span does not already give.
- **The rendered view as primary state** (a running accumulator patched per
  event). Rejected: overturns the recompute invariant the model fold already
  relies on for correctness, and a pure memo suffices without it.
- **Pre-serialised body.** Impossible at genai's seam — `exec_chat_stream`
  takes an owned `ChatRequest`, not bytes — and would trade a typed boundary
  for an untyped one.

## The decision

### The value

`record/model.rs` defines `Transcript`: private fields, `Vec<Arc<[ChatMessage]>>`
segments plus a cached byte length. Clone is `Arc` bumps. No method yields an
owned `ChatMessage`; the only routes back to owned messages are the wire door
and the mnemon child seed (`Memo::inherited_context_messages`).

The model fold's memo caches each *closed* span's rendering once, keyed by
span id and its end index (`SpanRender`) — a span id never recurs, so `(id,
end)` determines a rendering globally and forever, and staleness is
inexpressible. The renderer is split so nothing can smuggle a flag into the
cached path:

- `render_closed_entry` takes the full range and repairs its end; no flags
  parameter exists to vary, so the memo's key is the whole of its input.
- `render_tail` covers only the last, still-growing span; it takes the two
  retroactive flags (`omit_tail_assistant`) and is never cached.

`Memo::transcript()` (the `model_messages` replacement) assembles the digest
segment, `render_closed_entry` per closed span through the memo, and
`render_tail` fresh for the live tail. `history_bytes` and `context_survey`
read the cached per-segment byte counts; only the tail is ever re-serialised.

### The wire door

`exarch/src/provider/wire.rs` is the one place an owned `genai::ChatRequest`
is made:

- `Sealed(ChatRequest)` — deliberately not `Clone`.
- `manufacture(adapter, system, transcript: &Transcript, tail: Option<ChatMessage>, tools: &[Tool]) -> Sealed`
  clones each shared message once into a fresh `Vec`, applies the Anthropic
  cache-control marks to that owned copy alone (never to the shared
  segments), prepends the system message, and attaches the tool set. `tail`
  carries `summarize`'s trailing instruction — a freshly minted message, never
  a second clone site.

`Engine::complete` and `Engine::summarize` (`provider/stream.rs`) hold `&str`
system prompt and `&Transcript` and call `manufacture` inside the retry
closure, per attempt. There is no `request_template` in scope for either path
any more, and no pre-made request to clone: cloning a request is no longer
expressible on this path.

Settled against the locked genai 0.7.0-beta.19 source
(`adapter/adapters/anthropic/adapter_shared.rs:110-132,325-341`, recorded in
`manufacture`'s doc comment): the Anthropic adapter consumes the request by
value and reads `msg.options.and_then(|o| o.cache_control)` purely as a value;
`ChatMessage` carries no id, and neither the adapter (a unit struct) nor the
client holds anything across calls. No behaviour is keyed on message
identity, so marking a fresh clone per attempt is safe. The system prompt
stays an in-sequence `ChatMessage::system` on Anthropic rather than
`ChatRequest.system`, because the in-sequence form is load-bearing for the
two-anchor cache scheme: the request-level field is barred from a
cache-control mark, while an in-sequence System-role message keeps one, and
the request-level auto-mark defers to any message-level breakpoint — which the
last-two-messages marks guarantee exist. Every other adapter keeps the
dedicated `system` field.

Seam changes rippled mechanically: `Provider::complete`/`summarize`
(`provider.rs:135,177`) take `&Transcript`; `CompactionPlan.prefix`
(`agent/event.rs:269-271`) is a `Transcript`, not a `Vec<ChatMessage>`;
`AgentLog::render_messages` (`agent/event.rs:555`) returns
`Result<Transcript, String>`.

### The prose

`Agent::{system, system_base}` (`agent.rs:99,101,105`) are `Arc<str>`: a
fork's inheritance and every avatar's per-step read of the ~38 KB system
template are refcount bumps, not copies. The per-attempt system-message copy
inside `manufacture` remains — the genai floor, below.

## The genai floor

`genai::Client::exec_chat_stream`/`exec_chat` consume an owned `ChatRequest`,
and the system slot is owned too, whichever field carries it. One
whole-history copy per HTTP attempt — the ~38 KB system string included — is
therefore the boundary's unavoidable price; `Arc<str>` reaches the per-fork
and per-avatar copies and cannot touch this one. A retry storm still pays it
per attempt, bounded by the retry budgets. Everything above that floor —
the render-time copies, the kept request template, the double clone inside
the retry closure — is what this decision removes.

## Enforcement

- **Types.** `Transcript`'s fields are private and it yields only
  `&ChatMessage`; `Sealed` is not `Clone`; `ChatRequest` is named only in
  `provider/wire.rs`. A future per-step deep clone of history requires either
  a new `Vec<ChatMessage>` boundary (visible in review) or `.clone()`ing
  through the iterator (caught by nothing automatic — named honestly, not
  closed by a lint: clippy's `disallowed-methods` cannot scope a trait
  method, `ChatMessage::clone`, to one impl).
- **Tests landed with `wire.rs` and `record/model.rs`**, exercised by the
  ordinary `cargo test`: `manufacture`'s cache-control placement and system
  handling per adapter, that it leaves the source transcript untouched, and
  the existing model-fold projection tests that assert the transcript's
  message sequence through edits, folds, repairs, and resume — narrower in
  scope now that flag staleness is closed at `render_closed_entry`'s
  signature rather than defended by convention.

The source-scan meta-test, allocation-budget test, and ledger-residency
test the originating plan proposed were judged not worth their weight: the
types carry the guarantee.

## See also

[[internals/session-record|session-record]] (the transcript-as-value
narrative and the renderer split in the model fold),
[[internals/provider-fault-recovery|provider-fault-recovery]] (a retry now
re-manufactures the request rather than cloning one),
[[map/exarch/provider|provider]] (the wire door and the transport it sits
under), [[map/exarch/frontend|frontend]] (the model fold and `AgentLog`),
[[decisions/260625_shared-transport|shared-transport]] (the sibling decision
that ruled out a deeper `Engine`-level seam for the same reasons this one
keeps the projection-level boundary honest),
[[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]
(the sibling type-enforced ownership boundary this follows in shape: private
fields plus a closed door, not a convention).
