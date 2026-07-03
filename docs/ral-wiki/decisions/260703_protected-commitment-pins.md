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
- live `commitment:*` pins are mirrored as commitment pins, so a clean completion
  triggers a budget-free unresolved-commitment nudge;
- `verify_commitment` is the actor's check request: its schema admits only the
  protected pin key, the host builds the verifier prompt from the saved pin
  card, runs an `amnemon` child, and accepts only a structured
  `commitment_verdict` for the same key;
- passing a commitment is represented by clearing the protected pin through
  host-projected verifier output, not by the actor unpinning it;
- `/clear` remains a session reset and clears commitments with the rest of the
  pin register.

The writer and verifier remain ordinary `amnemon` sub-agents. The host need not
grow a sealed verifier path or a new authority type: it protects the cell where
the obligation lives, owns the verifier prompt, and refuses to let the actor
become quiet while that cell remains live.

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
