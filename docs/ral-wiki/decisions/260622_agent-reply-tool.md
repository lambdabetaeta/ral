---
status: proposed
---

# A sub-agent returns through an explicit `reply` tool

**The value a sub-agent hands its parent is the argument of an explicit,
hard-terminating `reply` tool call — not the text of whatever model step happened
to end the run.** The child *constructs* its return value; the harness no longer
scrapes it from the prose of the final tool-call-free step, and there is no scrape
fallback. `reply` is a model-level tool (a peer of `agent`), not a ral builtin,
because finishing-and-returning is an agent-protocol act, not a language operation.

## Context

Today a child's reply is whatever prose it streamed in its **last tool-call-free
step**. `Session::drive` returns `(AgentOutcome, String)`; the string is
`Session::apply`'s `last_text`, which is cleared at the top of every step
(`session.rs:541`) and returned as `TurnOutcome::Complete(last_text)` only when
`tool_calls.is_empty()` (`session.rs:650-659`), then reduced by `agent_digest`
(`session.rs:917`). The contract is therefore *positional*: the return value is
decided by where prose lands relative to tool calls.

That contract loses reports. An observed sub-agent run mapped a project's testing
setup in a 3354-character brief, then made a tool call, then signed off in a
396-character wrap-up. Because the wrap-up was the only tool-call-free step, the
parent received **only the 396-character wrap-up**; the brief was committed to the
child's own `SessionLog` and streamed to its rail, but was never the value handed
up. It was clipped by nothing — `AGENT_REPLY_CAP` is 6000 — and corrupted by
nothing. It simply was not in `last_text` when `apply` returned.

This is structural, not a prompting defect:

- **Concatenating all assistant prose** returns every step's scratch narration
  ("Let me check the Cargo files…") alongside the report.
- **Returning the last contiguous prose run** still fails the case above: the tool
  call and its result sit *between* the brief and the wrap-up, so the brief is not
  contiguous with the final step.
- **Prompting** ("end with your full report, no tool call after it") narrows the
  failure rate but cannot close it: one stray trailing tool call (a `ral` call, an
  `edit`) drops the report, and the contract stays fragile.

The cross-check is oh-my-pi, whose sub-agents (its `task` tool) return the
argument of a mandatory `yield` tool, serialized; assistant prose is a fallback
only, and the run terminates on `yield`. The return value there is an artifact the
agent deliberately constructs. exarch should take that shape — an explicit
return-and-terminate call — without its typed-schema apparatus, and (unlike
oh-my-pi) without a prose fallback at all.

## Decision

- **A sub-agent returns through a `reply` tool.** Its single argument is the
  child's result, and it is the *only* value a child hands its parent. Dispatching
  `reply` is the one way a child sets that value; there is no final-prose scrape,
  neither as the primary path nor as a fallback (see the no-reply finish below).

- **`reply` is a tool, not a builtin.** "I am finished, here is my result" is an
  agent-protocol action at the same level as *spawning* a child — and spawning is
  the `agent` tool, so finishing is its peer. Builtins (`view-text`, …) live below
  the Provider and are reached only by the model writing ral inside the `ral` tool;
  the return-and-terminate decision is not expressible there without dragging a
  model-level concern into the language and bubbling a termination signal up from a
  builtin through the `ral` tool result.

- **`reply` hard-terminates the child — after the batch's effects land.** A staged
  `reply` ends the drive loop; it does not loop for another round-trip. But
  termination waits for the whole tool-call batch to finish: every call in the
  assistant message is dispatched and *every* `call_id` gets its tool result, so
  the session reaches a clean `ReadyForUser` boundary instead of stranding
  mid-protocol with results pending — the same X6 hazard the truncation path at
  `session.rs:631-634` already guards by dispatching captured calls before
  returning. The dispatcher stashes the rendered payload on the session and sets a
  terminate flag; once the batch drains, `apply` returns a *replied* terminal
  carrying the payload. The turn still exits
  `ReadyForUser` ([[invariants/turn-ends-ready|turn-ends-ready]]).

- **The replied terminal is distinct from a prose finish.** A `reply` and a
  tool-call-free prose step must not collapse to the same `TurnOutcome::Complete`:
  the nudge layer reacts to *every* outcome (`drive` → `nudge::react`, matching
  `Ok(TurnOutcome::Complete(_))`, `nudge.rs:105`) and could not otherwise tell
  "already returned" from "stopped without returning" — it would re-nudge a child
  that already replied. So `reply` produces its own terminal (a `Replied(payload)`
  variant, or `Complete` plus the session flag `agent_digest` reads); the nudge
  layer accepts it silently and `agent_digest` maps it to
  `(AgentOutcome::Complete, clip(payload, AGENT_REPLY_CAP))`.

- **The payload is the tool's JSON argument, rendered by exarch's existing
  value→text rule.** A JSON **string** passes through raw, so a markdown report
  keeps real newlines (the escaped-`\n` hazard `shell_eval.rs:298-300` already
  names); a JSON **object/array** is pretty-printed so the shape stays legible;
  then the result is `clip`ped at `AGENT_REPLY_CAP`. This is the convention the
  `ral` tool already applies inline in `run_shell` (`shell_eval.rs:297-313`); the
  JSON→text tail is factored into one shared function both call. That function
  takes a `serde_json::Value` — `reply`'s argument arrives as JSON already, while
  the `ral` path keeps its `RalValue`→JSON lossy-byte decode
  (`value_to_json_lossy_bytes`) as the adapter ahead of it. "Markdown only" is the
  string case and needs no special mode; a reviewer's findings or an explorer's
  file list is the object case. A `reply` with an empty or missing argument
  renders to nothing and settles as `AgentOutcome::Empty`.

- **`AGENT_REPLY_CAP` now governs the deliberate reply, and earns more room.** With
  the scrape gone, the cap bounds exactly the reply payload — a curated report, not
  a scraped tail. The first explorer's good brief was 5787 against a 6000 cap: too
  close. Raise the cap so a deliberate report fits, and keep middle-elision (with
  `clip`'s inline truncation marker, `digest.rs`) only as the hard backstop, not
  the common outcome.

- **A no-reply finish is nudged once, then returns empty — no scrape.** A child
  that finishes a tool-call-free step without having called `reply` earns one
  reminder to call it, via a **one-shot, budget-free latch** in the nudge registry
  — structurally the existing idle/verify latches (`nudge.rs:60-63, 105-124`),
  which already fire on a clean `Complete`. If the child still does not reply, it
  returns `AgentOutcome::Empty`: the parent sees "[agent '…' finished with no
  output]" (`bus.rs:90`), and the child's prose is **not** harvested. A child that
  is cancelled, hits `MAX_STEPS`/`Capped` (`session.rs:124`), or fails delivers its
  outcome tag with no text, exactly as those tags already render. The child is
  async to its parent ([[decisions/260617_async-agent-tool|async-agent-tool]]), so
  a missing reply costs the parent nothing but the tag.

- **The reply re-enters the parent through the inbox, unchanged.**
  `AgentResult.text` (`bus.rs:108`) is the rendered `reply` payload; the async
  delivery seam — settle, push `InboxMsg`, render a synthetic turn at the next
  boundary ([[decisions/260617_async-agent-tool|async-agent-tool]]) — is untouched.
  Only how `text` is *computed* changes.

- **`reply` is sub-agent-only, withheld from the root.** The root talks to the user
  across turns and never "returns", so it must not see `reply`. The existing
  `ToolSet` axis (`spawns()`, `tools.rs:44-49`) is the *inverse* gate — it withholds
  the spawn family from peers — so this needs a new axis, its mirror: `reply` is
  unadvertised under the root's tool set (so the root's model never calls it), with
  `stage`'s allow-check (`session.rs:773`) as the backstop. Unadvertised at the
  root, there is no root-level termination to define.

## Why this shape

- **The return value becomes a deliberate artifact, not a positional accident.**
  The prose/tool-call ordering that loses a report today cannot lose it: the report
  is the `reply` argument, and a trailing tool call or sign-off is irrelevant to it.
  Severing the scrape entirely means there is no positional path left to fall back
  into.
- **It reuses what exists.** The value→text renderer (the `run_shell` tail), the
  reserved `AGENT_REPLY_CAP`, the inbox delivery, the `AgentOutcome::Empty` tag, and
  the nudge registry's one-shot latch pattern (idle/verify). The genuinely new code
  is one tool, one inverse-of-`spawns` gating axis, one terminate-after-batch check
  in `apply` with its replied terminal, and one shared renderer lifted out of
  `run_shell`.
- **It takes oh-my-pi's shape without its weight.** An explicit return-and-
  terminate call, yes; per-agent JTD schemas, schema-retry, and `report_finding`
  splicing, no — exarch agents are conversational, returning markdown or an
  ad-hoc record, and the string/structured render rule covers both.
- **The residual risk is a premature `reply`, and it is acceptable.** Where the old
  contract lost a report at the *end*, the new one can be cut short by a `reply`
  called too *early* — and hard-terminate makes that final. But a premature reply is
  still a deliberate value the child chose to send, not silently-dropped prose, and
  it is prompt-addressable (the nudge handles the opposite, under-termination, case).
  An unrecoverable early stop in exchange for never losing a finished report is the
  right trade.

## Alternatives considered

- **`reply` as a ral builtin.** Rejected: it puts a model-level act at the language
  level. Termination would have to bubble from a builtin through the `ral` tool
  result into the session loop, and the payload would be composed in ral rather than
  by the model. The one thing it buys — handing back a *live ral binding*
  (`reply $findings`) without transcription — is not needed: the agent boundary is
  deliberately text-out ("you receive a string, not its bindings or intermediate
  findings", `tools/agent.rs:85`), and sub-agents are report-writers, not
  value-computers. Making value-handoff first-class is a separate decision about the
  boundary, not a reason to demote `reply` to a builtin.
- **Concatenate all assistant prose across the run.** Rejected: returns scratch
  narration and conflates report with thinking.
- **Return the last contiguous run of prose.** Rejected: the incident proves it
  fails — a tool call between the report and the sign-off breaks contiguity, so only
  the sign-off survives.
- **Keep the final-prose scrape as a fallback for a child that never replies.**
  Rejected: it reintroduces the exact positional accident this decision removes. A
  child that cannot manage a `reply` even after the nudge returns `Empty` honestly;
  handing its parent an arbitrary trailing prose fragment is worse than handing up
  nothing, because the fragment reads as an answer when it is not one.
- **Fix it by prompting alone.** Rejected as *sufficient*: it narrows the failure
  rate but leaves a fragile positional contract. Prompting is a complement (tell
  the child to `reply`), not the mechanism.
- **A typed output schema per agent (oh-my-pi's JTD `yield`).** Deferred: exarch
  agents are conversational, not typed-record producers; the string/structured
  render rule covers both shapes without validation, retries, and finding-
  splicing. A schema can be layered on later if a structured agent needs it.
  Inventing an error-shaped reply payload (`{error: …}` → a failure tag) is the
  first step down this path and is likewise deferred: an empty reply maps to
  `AgentOutcome::Empty`, and a child that fails signals it in prose, not a reserved
  shape.
- **Mandatory `reply` with forced `tool_choice` after N retries.** Rejected:
  unnecessary given async + `MAX_STEPS`. A one-shot nudge and the step cap suffice,
  and forcing a tool call mid-confusion tends to produce a worse payload than a
  nudge does.

## See also

[[decisions/260617_async-agent-tool|async-agent-tool]] (the async worker, inbox
delivery seam, and `AgentResult` this rides on),
[[invariants/turn-ends-ready|turn-ends-ready]], [[map/exarch/agent|agent]],
[[map/exarch/tools|tools]], [[map/exarch/shell-eval|shell-eval]] (the value→text
render this reuses), and `docs/SPEC.md` §11.
