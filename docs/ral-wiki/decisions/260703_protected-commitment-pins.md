---
status: active
---

# Protected commitment pins

`commitment:*` is a reserved pin keyspace: the actor may see it, but ordinary
`surface` cannot write or clear it.

## Decision

Commitment state reuses pins instead of adding a new orchestration channel. At
the public surface the distinction is only a reserved prefix; inside the agent's
pin mirror each slot has a small kind (`ordinary` or `commitment`) so nudges do
not infer obligations from rendered text:

- model-authored `surface `` `pin ``/`` `unpin `` to `commitment:*` is rejected
  at the exarch surface sink with a diagnostic;
- live `commitment:*` pins ride the same one pinned-state nudge every other
  pin kind does — uniformly, budget-free, for every agent regardless of
  role — rather than a separate commitment-only mechanism: pinned state is
  pinned state, and a returning sub-agent's independent obligation to call
  `reply` never suppresses it;
- `commit` is the actor's open request: its schema admits a key and a free-text
  `description`, never the criteria. The host builds a writer prompt from the
  description and launches an `amnemon` child the same launch-only,
  always-asynchronous way (sharing `Gate::Spawns`), opening the pin only on a
  structured `commitment_card` for the same key carrying at least one
  criterion — refused up front, before ever forking, if the key is already
  live;
- `verify_commitment` is the actor's check request: its schema admits only the
  protected pin key, the host builds the verifier prompt from the saved pin
  card, and launches an `amnemon` child the same way, accepting only a
  structured `commitment_verdict` for the same key;
- opening and passing a commitment are both represented by projecting the
  writer's/verifier's structured output onto the register — on the host's own
  thread, as each settled result drains — never by the actor pinning or
  unpinning it directly;
- `/clear` remains a session reset and clears commitments with the rest of the
  pin register.

The writer and verifier remain ordinary `amnemon` sub-agents, launched and
settled through the identical `spawn_async` mechanics
([[map/exarch/tools|tools]]). The host need not grow a sealed writer/verifier
path or a new authority type: it protects the cell where the obligation
lives, owns both prompts, and refuses to let the actor become quiet while
that cell remains live.

## Consequences

The host does not judge correctness. It enforces provenance for one register
prefix and liveness for unresolved state. A verifier that does not pass leaves
the pin live; the actor is nudged again by the existing `nudge` path.

The protection is intentionally narrow. Ordinary pins, including `tasks` and
`goal`, keep the old model-authored overwrite/unpin semantics.

## See also

[[design/pins|pins]], [[map/exarch/shell-eval|shell-eval]],
[[map/exarch/agent|agent]], [[map/exarch/tools|tools]],
[[design/agents|agents]].
