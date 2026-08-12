---
status: accepted
---

# `reply` terminates every returning agent — including the headless root

**The gate on `reply` is not "sub-agent vs root" but "does this agent return."**
Every agent that terminates-and-returns — every sub-agent, *and* the root in
headless (non-interactive) mode — finishes through `reply`; it is the sole return
path, mandatory and enforced by bounded re-nudging rather than a final-prose
scrape. The interactive root, which converses across turns and never returns,
keeps `reply` withheld. The reply payload is rendered to its consumer: prose for a
model parent, ral text for a human-readable headless sink, and a faithful value
for the headless JSON harness.

This extends [[decisions/260622_agent-reply-tool|agent-reply-tool]], whose
"`reply` is sub-agent-only, withheld from the root" was reasoning about the
*interactive* root and never reached the headless one.

## Amendment — headless text uses the deliberate reply

As of 2026-08-12, `--headless --output-format text` is a sink for the same
deliberate `reply` artifact as JSON. It suppresses incidental root assistant
tokens and writes the reply once on successful completion: strings remain raw
text, structured values use ral's existing value renderer, and unit is empty.
Progress, tool calls, cards, failures, and the run summary remain on stderr;
`converse_on` remains the conversational token-streaming path.

## Context

[[decisions/260622_agent-reply-tool|agent-reply-tool]] made `reply` the way a
sub-agent constructs and returns its value, severing the positional final-prose
scrape that lost reports. It made two choices this decision revisits:

- **It withheld `reply` from the root** ("the root talks to the user across turns
  and never 'returns'… Unadvertised at the root, there is no root-level
  termination to define"). That premise is *interactive-only*. The **headless**
  root does return: it is seeded once, produces one result, and the process exits.
  The original implementation made that result the prose of the root's last
  completed message — `Headless`
  rolls root tokens into `cur_msg` and, at each `Kind::Boundary`, does
  `self.final_msg = std::mem::take(&mut self.cur_msg)` (`headless.rs:270-272`),
  which `result_json` emits as `"result"` (`headless.rs:152`). That is exactly the
  positional scrape 260622 indicted: a thin final sign-off wins over the
  substantive earlier step, and a trailing tool-only step (no text tokens) blanks
  `cur_msg` and empties the result. **The headless root still carries the bug
  260622 fixed for sub-agents, because 260622 never reached it.** The amendment
  above also removes the separate text-mode token scrape.

- **It accepted a no-reply finish as one-shot-nudge-then-`Empty`** and explicitly
  rejected "mandatory `reply` with forced `tool_choice`." Fail-fast spares wasted
  rounds, but it lets a single forgotten `reply` discard the agent's whole report:
  the work is committed to the agent's log and rail but is not the value handed up.
  Fail-fast and forced-choice are not the only two options.

## Decision

- **`reply` is advertised to every *returning* agent.** The discriminator on
  260622's reply-gating tool-set axis changes from `is_root` to
  `is_root && interactive`: a non-interactive (headless) root is a returning agent
  and advertises `reply`; the interactive root does not. The discriminator is the
  same `interactive` flag that already sets `park_when_idle` (`session.rs:313`), so
  "returning" and "does not park for a human" are one fact, read in one place. The
  `must_reply` nudge gate (`session.rs:555`, today `!is_root`) and the `stage`
  allow-check backstop (`session.rs:773`) move in lockstep with the tool gate.

- **`reply` is mandatory; a no-reply finish is re-nudged within budget, then
  fails — no scrape, no fallback.** This reverses 260622's "nudge once then
  `Empty`." The reversal is *bounded*: re-nudging rides the existing per-turn nudge
  budget (`nudge.rs`, `BUDGET`) and the step cap as the backstop — it is **not**
  forced `tool_choice`. 260622 rejected forcing a tool call mid-confusion (it
  corrupts the payload) and was right to; the distinction it missed is between
  *forcing the call* (bad) and *insisting within budget, then failing honestly*
  (good). A returning agent that never replies is `Failed`, not silently `Empty`:
  the sub-agent's parent sees a failure tag; the headless root's `result` is
  honestly empty/failed — which beats a confident trailing fragment that reads as
  an answer but is not one.

- **The tool is uniform; the *sink* renders the payload to its consumer.** `reply`
  carries a value, rendered by 260622's rule (a JSON **string** passes through raw,
  so markdown keeps real newlines; a JSON **object/array** is structured). The two
  return sinks differ because their consumers differ:
  - **Sub-agent → a model parent** that reads the value as prose in its context
    (`AgentResult.text` → the `↘` block). The object case is pretty-printed to
    text — 260622's text-out boundary, unchanged.
  - **Headless root → a machine** (the harness parsing `--output-format json`). Its
    payload is written to `result` as a **faithful value**: a string stays a
    string, an object/array stays an object/array. Pretty-printing structure into
    the string field would double-encode it — the harness then re-parses
    JSON-out-of-a-JSON-string, the escaped-`\n` hazard `shell_eval.rs:298-300`
    already names.
  - **Headless root → a human** (`--output-format text`). Its same value is
    rendered once through ral's existing text projection; it is not a stream of
    incidental assistant tokens.

- **The headless `result` field becomes `string | object`.** A deliberate
  divergence from Claude Code's always-string shape. 260622's parity goal was
  "field names mirror so a harness can parse either agent"; typing `result`
  faithfully keeps the names and the spirit — a run that asked for structure
  receives structure — while admitting a union the harness must handle. The
  always-stringify alternative preserves the exact shape at the cost of
  double-encoding, which defeats allowing JSON at all.

- **`reply` is rejectable — on conformance, never on syntax.** Tool-call arguments
  arrive *already parsed* (260622: "reply's argument arrives as JSON already"); a
  syntactically broken call fails upstream at the provider parse layer and never
  reaches `reply`. So inside `reply` an object is valid by construction and a
  string is legitimate prose — there is nothing to syntax-check. Rejection fires
  only against an *expectation*: a wrong call-shape (the existing `invalid_input`
  path) or, when the run requested a structured format, a payload that does not
  conform. A rejected reply re-nudges exactly like a no-reply. exarch must **not**
  parse a free reply string as JSON and fail when it does not parse — that
  re-creates the hand-serialized-JSON anti-pattern this decision removes and would
  reject a valid markdown answer that merely contains a `{`.

- **Schema-validated payloads stay deferred.** The freeform default —
  string = prose, object = structure — needs no validation. A declared output
  schema (oh-my-pi's JTD `yield`, deferred in 260622) is the home for the
  conformance rejection above; this decision provides the hook (reply is rejectable
  and re-nudged) without the schema machinery. Layer it on when a structured agent
  needs the guarantee.

## Why this shape

- **One return contract, not two.** The positional scrape is removed everywhere it
  survived — sub-agent returns (260622) and the headless result (here) — by the
  same mechanism: a deliberate `reply` artifact rather than whatever prose happened
  to land last.
- **The gate matches the real distinction.** "Returning vs conversing" is the
  actual axis. "Sub-agent vs root" was a proxy that read correctly only while the
  sole returning agents were sub-agents; headless makes the root a returning agent,
  so the proxy breaks and the real axis shows through.
- **Honest failure over confident fragments.** A run that never `reply`-ed did not
  complete its contract. Reporting that — a failure tag, an empty `result` — is
  truer than scraping a trailing sentence that masquerades as the answer.
- **It reuses what exists.** Re-nudging is the existing nudge budget and one-shot
  latch pattern; the value→text render and the `AgentResult` inbox delivery are
  260622's. The only genuinely new pieces are the headless `result` sink (a
  faithful value) and the gate's `interactive` discriminator.

## Consequences — and what this does *not* do

- **`reply`-as-done is orchestrate-and-abandon, not collect.** With the headless
  root at `park_when_idle = false` (`session.rs:313`), it cannot await a child's
  `AgentResult` before replying. `reply` gives a clean termination and lets the
  root abandon any still-running children — they are cancelled at the
  hard-terminate (registry clear), and their *partial* usage to that point is
  already counted at the emit-seam usage meter (the accounting sibling of
  [[decisions/260623_recording-follows-the-event|recording-follows-the-event]]). A
  headless root that needs a child's *result* still requires a registry-empty
  wait — an orthogonal decision, not this one.
- **Stragglers at root reply.** `reply` hard-terminates after the tool batch lands
  (260622); for the root that ends the drive loop with children possibly live.
  Those are cancelled there, their tokens counted to the moment of cancel.

## Alternatives considered

- **Keep `reply` root-withheld; improve the headless scrape instead.** Rejected:
  the scrape *is* the bug. Any positional rule loses the same reports 260622
  enumerated; the headless root deserves the same deliberate artifact, not a better
  guess.
- **Always stringify the headless `result`.** Rejected: double-encodes a structured
  reply so the harness re-parses JSON-in-a-string. Preserves Claude Code's
  always-string shape at the cost of the feature.
- **Keep one-shot-nudge-then-`Empty` (260622) for returning agents.** Rejected: a
  single forgotten `reply` discards the agent's work, and for the headless root
  yields a confident-looking fragment in place of the answer. Bounded enforcement
  plus honest failure is the better trade now that `reply` is the *sole* return.
- **Forced `tool_choice` reply after N retries.** Rejected, as in 260622: forcing a
  call mid-confusion corrupts the payload. Bounded re-nudging within budget is the
  enforcement; the step/budget cap is the backstop.
- **Validate the reply *string* as JSON, fail if it does not parse.** Rejected:
  re-creates hand-serialized JSON and punishes a markdown answer containing a brace.
  Conformance is checked against a *requested* schema (deferred), not against free
  prose.
- **A typed output schema per returning agent, now.** Deferred, as in 260622 — the
  string/structured render rule covers both shapes without validation, and this
  decision leaves the hook for it.

## See also

[[decisions/260622_agent-reply-tool|agent-reply-tool]] (the contract this extends),
[[decisions/260617_async-agent-tool|async-agent-tool]] (the async worker and
`AgentResult` delivery seam),
[[decisions/260623_recording-follows-the-event|recording-follows-the-event]] (the
emit-seam recording principle the run usage meter mirrors for accounting),
[[invariants/turn-ends-ready|turn-ends-ready]], [[map/exarch/agent|agent]],
[[map/exarch/tools|tools]], [[map/exarch/shell-eval|shell-eval]] (the value→text
render this reuses), and `docs/SPEC.md` §11.
