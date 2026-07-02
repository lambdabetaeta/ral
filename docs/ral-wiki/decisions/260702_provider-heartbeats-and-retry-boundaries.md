---
status: proposed
---

# Provider heartbeats are not model turns

**Exarch should judge provider liveness by *wire progress*, not by semantic
model output, and provider recovery should stay below the transcript.** Modern
Claude-class models, including `claude-fable-5` and `claude-sonnet-5`, may spend
many minutes in useful thinking before returning text, tool calls, or reasoning
deltas. A five-minute gap in decoded `ChatStreamEvent`s is therefore not enough
evidence to fail a run. It is only evidence that the adapter has not yielded a
semantic event.

## Context

The current Anthropic path has three layers with three different notions of
progress:

- **The wire stream has bytes and SSE events.** Anthropic's streaming contract
  allows arbitrary `ping` events during a response
  ([Streaming Messages](https://platform.claude.com/docs/en/build-with-claude/streaming)).
  A stream can be alive while yielding only pings.
- **Genai translates provider events into semantic chat events.** The
  Anthropic streamer consumes `ping` events internally and yields text,
  reasoning, tool-call chunks, and the final end event.
- **Exarch currently times out between semantic events.** `provider.rs` raises
  `Transient { cause: "stream idle: no event within timeout" }` when
  `resp.stream.next()` yields no decoded event inside the budget. The provider
  retry loop then spends its inner attempts, and `nudge.rs` may re-drive the
  whole request through a synthetic model-visible reminder.

That shape made a Terminal-Bench loss look like a model failure: after many
productive round trips, the stream produced no decoded event for the fixed
budget twice, then exarch surfaced `no_reply`. The exact duration was exarch's
policy shining through, not an Anthropic error message and not a reasoning
failure.

The hard requirement is stricter than "retry better": **a long-thinking model
must be allowed to remain semantically quiet for a long time, as long as the
transport keeps proving life.** The harness may still impose a wall-clock
ceiling, but that is a benchmark/runtime policy, not provider liveness.

## Decision

Split provider progress into three clocks and let only the right one fail a
request.

- **Raw silence clock.** Measures time since the last byte/SSE frame/transport
  heartbeat. If this expires, the connection is plausibly dead. It is retryable
  when no assistant content has been committed.
- **Semantic silence clock.** Measures time since the last text, reasoning,
  tool-call, or end event. If raw heartbeats continue, this clock emits
  operator diagnostics but does not fail the request by default.
- **Wall-clock budget.** Measures total request or run time. It is explicit
  configuration for a harness or human, never smuggled in as "stream idle."

The provider layer therefore treats these cases differently:

| Observation | Meaning | Action |
|---|---|---|
| no raw progress before any content | dead connection or stalled open | internal retry with backoff |
| pings/heartbeats continue, no semantic event | model or provider still working | keep waiting; emit wait diagnostics |
| semantic content streamed, then raw silence | committed partial reply hit a transport fault | keep the prefix and continue via truncation recovery |
| provider `error` event or HTTP 5xx before content | provider failure | internal retry according to typed classification |
| retry budget exhausted before content | provider failure | surface provider error; do not nudge the model |
| explicit wall-clock budget expires | host/harness cancellation | cancel and report the configured budget |

## Plan

### 1. Expose heartbeats through genai

Add a provider-neutral heartbeat event to the forked `rust-genai` stream model:

```rust
enum InterStreamEvent {
    Start,
    Chunk(String),
    ReasoningChunk(String),
    ToolCallChunk(ToolCall),
    ThoughtSignatureChunk(String),
    Heartbeat,
    End(InterStreamEnd),
}

enum ChatStreamEvent {
    Start,
    Chunk(ContentChunk),
    ReasoningChunk(ContentChunk),
    ToolCallChunk(ToolCallChunk),
    ThoughtSignatureChunk(String),
    Heartbeat,
    End(StreamEnd),
}
```

Anthropic maps `event: ping` to `Heartbeat` instead of `continue`. Other
adapters may yield no heartbeat until their providers expose one. Unknown
Anthropic event types still log and skip unless they are `error`, preserving the
current forward-compatibility rule.

This is an API change in the local genai fork, so exarch's stream match remains
exhaustive and must name the heartbeat arm explicitly.

### 2. Replace decoded-event timeout with a liveness state

Move the stream loop in `provider.rs` from one `idle_timeout(attempt)` to a
small `Liveness` state:

- `last_raw`: reset on `Start`, `Heartbeat`, and every semantic event.
- `last_semantic`: reset only on text, reasoning, tool-call, thought-signature,
  and end events.
- `attempt_started`: used for request-open diagnostics.
- `semantic_notice_after`: a diagnostic cadence, not a failure threshold.
- `raw_idle_timeout`: the actual dead-connection threshold.
- `wall_timeout`: optional; off by default for interactive use, supplied by
  headless harnesses when desired.

`tokio::select!` should race:

- cancellation,
- optional wall timeout,
- raw-idle sleep,
- semantic-notice sleep,
- `resp.stream.next()`.

On heartbeat, the loop records raw progress and continues. On semantic notice,
it emits a phase such as "model still working; heartbeat 24s ago" and continues.
On raw idle, it returns `Transient`.

The default must suit long-returning models: no semantic timeout, no total
request timeout, and a raw-idle bound only for actual silence. A model that
keeps sending pings may run until it completes, the user cancels it, or an
explicit harness deadline fires.

### 3. Keep provider retries below the transcript

Remove provider failures from the nudge rule set:

- delete `on_provider_recoverable` from `RULES`;
- remove the transient one-shot latch from `nudge::Registry`;
- keep nudges for model-behaviour outcomes: empty turns, early stops, output
  truncation, no `reply`, and pinned-state reminders.

If a provider request fails before producing assistant content, retry it inside
`retry_with_backoff`. If that retry budget exhausts, surface a provider error
and end the turn honestly. Do not append a synthetic user message that asks the
model to continue from a request it never saw.

Rate limits follow the same rule. A 429 can have a patient provider retry tier,
respecting `Retry-After`, but it is still not a model turn. If the patient tier
exhausts, the operator sees "rate limited after N attempts"; the transcript does
not grow.

### 4. Preserve committed partial work

Keep the existing "commit, do not double-render" rule after assistant content
has streamed:

- if text or reasoning was rendered, a later stream fault becomes
  `CutShort::Stalled`;
- the partial assistant message is admitted into the transcript;
- `Agent::apply` returns `ProviderError::Truncated`;
- the truncation nudge asks the model to continue from committed context.

This is the one provider-originated path that should still use a model-visible
continuation, because the model's partial work is now real transcript context.
It is not a retry of an invisible failed request.

### 5. Make headless results and accounting structural

Provider attempts, heartbeats, semantic notices, and nudges must be different
facts in logs and result JSON.

- Add provider-attempt breadcrumbs with attempt number, phase, error class, raw
  idle duration, semantic idle duration, and optional provider request id when
  available.
- Add heartbeat/wait breadcrumbs that are visible in headless logs but do not
  count as turns.
- Make exhausted provider errors produce a provider-error stop reason, not
  `no_reply`.
- Keep turn counts tied to real human/inbox turns and model-visible nudges only.
  Provider retries are attempts, not turns.

This closes the benchmark trap: a long-running task can be scored as cancelled,
provider-failed, wall-timeout, or solved, but not mislabeled as a model that
chose not to answer.

### 6. Add the right tests

The tests should pin the distinctions, not sleep for real minutes.

- **genai Anthropic ping test:** an SSE `event: ping` maps to
  `ChatStreamEvent::Heartbeat`.
- **heartbeat keeps stream alive:** with Tokio paused time, repeated heartbeats
  keep a request alive past the old semantic idle budget.
- **semantic notice is not failure:** no text for several notice intervals emits
  wait diagnostics and keeps polling.
- **raw silence still fails:** no heartbeat and no semantic event trips raw idle,
  classifies as `Transient`, and retries internally.
- **provider errors do not nudge:** a scripted exhausted `Transient` or
  `RateLimited` surfaces as `ProviderError` with no `InboxMsg::Nudge`.
- **partial content still continues:** a stall after text or reasoning commits a
  `CutShort::Stalled` assistant message and uses the truncation nudge.
- **headless accounting:** provider retries increment attempt counters, not
  turn counters; final JSON distinguishes provider failure from no-reply.

### 7. Ship in small parcels

1. Add genai heartbeat events and update exarch's exhaustive stream match.
2. Introduce `Liveness` in `provider.rs`, initially preserving old raw-idle
   durations but treating heartbeats as raw progress.
3. Add semantic wait diagnostics with no failure effect.
4. Remove provider recoverables from `nudge.rs`; surface exhausted provider
   errors directly.
5. Fix headless result classification and attempt accounting.
6. Re-run the failing Terminal-Bench tasks and compare logs: there should be
   provider attempts/heartbeats, but no synthetic recovery nudge unless partial
   assistant content was committed.

## Consequences

- Long-returning models are first-class. A quiet semantic stream is allowed when
  the provider continues to send heartbeats.
- Dead sockets still fail promptly. Raw silence remains bounded and retryable.
- Provider recovery stops polluting model history. Invisible failed attempts do
  not become fake user turns.
- Benchmark logs become legible: attempts, waits, heartbeats, nudges, and real
  turns are separately countable.
- The local genai fork carries a small API delta that should either be upstreamed
  or isolated behind an exarch-only adapter wrapper.

## Alternatives

- **Raise the decoded-event timeout.** Rejected. It only chooses a larger magic
  number, and modern models can exceed it legitimately.
- **Remove the timeout entirely.** Rejected. A dead TCP/SSE stream must not hang
  a run forever.
- **Keep provider recovery as a nudge.** Rejected. The model cannot fix a
  transport fault or a 429, and the synthetic prompt corrupts turn accounting.
- **Treat pings as textless semantic events.** Rejected. They are liveness, not
  model content; they should not enter transcript logic.
- **Make every harness set a huge wall timeout.** Rejected. Harness time is
  orthogonal to provider liveness; combining them hides the failure mode.

## See also

[[internals/provider-fault-recovery|provider-fault-recovery]],
[[map/exarch/provider|provider]], [[map/exarch/agent|agent]],
[[decisions/260625_shared-transport|shared-transport]], and
[[decisions/260623_recording-follows-the-event|recording-follows-the-event]].
