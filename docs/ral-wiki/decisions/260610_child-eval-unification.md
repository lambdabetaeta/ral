---
status: active
---

# One re-exec'd-child eval protocol

**Evaluating a `Comp` in a re-exec'd child is one protocol with two
preludes, not two protocols that must agree.** The confined-eval child
(the OS sandbox, [[design/grant|grant]]) and the process-staged pipeline
stage (a helper subprocess, [[design/pipelines|pipelines]]) both pack a
body plus a `WireMobile` snapshot, reconstruct a shell, evaluate, and
report a structured outcome with audit nodes. That shared shape is now a
single module — `core/src/child_eval.rs`, a crate-root sibling of the
[[map/core/transport|transport]] layer it rides on.

- **One wire shape, one runner.** `ChildEvalRequest` in,
  `ChildEvalResponse` out — one frame each way. `run_child_eval(request,
  upstream, kind)` is the sole child runner; `pack_request` /
  `decode_response` / `break_response` are its inverse and its
  pre-eval-failure constructor. The old `EvalRequest`/`StageJob::Ral`
  pair — near-identical structs down to a verbatim-duplicated doc
  comment, and two `pack → reconstruct → run → report` state machines —
  collapse into it.
- **`ChildKind` is the one axis that cannot merge.** `Sandbox` runs
  `eval_comp` against a shell whose mobile already carries the lexical
  env, and returns the *mutated* mobile (the contract layer installs it).
  `PipelineStage` hydrates a separate captured closure env, builds
  `Shell::child_of`, applies the body by `call::invoke` data-last, and
  returns only `last_status` — a stage is a subshell. Folding the two
  eval shapes would change checker-visible semantics.
- **Value and mobile serialization are gated before serialization.**
  `wants_value` (a byte-mode stage that retains an incidental
  non-transferable value must not fail the run) and `wants_mobile` (a
  pipeline stage must not ship its post-run mobile at all). A
  non-transferable value becomes a structured boundary error carrying the
  value type's own hint when it has one.
- **The hazard removed is the agreement obligation.** Two snapshot
  protocols that must stay byte-compatible was the exact bug class the
  pipeline parcels had been hunting; with one definition there is nothing
  to drift. The value-edge force the pipeline applies before a value
  crosses an edge is likewise one shared `force_pipe_value`, called by
  both the parent fold and this runner — the runtime mirror of the
  [[decisions/260609_pure-pipe-equation|pure-pipe-equation]].
- **Live audit streaming was always cosmetic.** Both children drain their
  audit fragment *after* eval; the parent buffered frames until a final
  marker. So the unification collapses to one response frame with the
  nodes embedded, deleting both per-node frame loops. Reintroducing live
  streaming would reintroduce a frame loop; it does not belong here.
- **Surface events take one code path, replayed by choice.** The child
  always buffers them; the sandbox parent replays them through its own
  sink, the pipeline parent drops them (a stage child has no sink to
  replay to — preserving prior behaviour).

The OS-entry sequence stays per-consumer and untouched: the sandbox is
entered in `early_init` before the runner is reached, fd/handle
inheritance arming stays in each transport, and the pipeline keeps its
SIGPIPE/pgid/anchor placement. The runner acquires no new pre-eval
authority a confined child lacks ([[map/core/capabilities|capabilities]]).
