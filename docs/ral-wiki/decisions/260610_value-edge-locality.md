---
status: superseded
superseded_by: decisions/260809_byte-only-pipelines
---

# Value-edge judgments live where their facts live

**Every judgment about a pipeline's value edges is taken at the site
that owns its facts — and the day this rule was enforced, two latent
bugs fell out of the places that violated it.** Follow-on parcel set to
[[260610_child-eval-unification|child-eval-unification]]; landed
2026-06-10 (`73db2c61..2df6db85`).

## The rule, instantiated four times

- **Resolve owns dispatch.** A bundled (uutils) head carrying a value
  edge routes `HelperEval`, not `HelperUutils`: the uutils arm execs the
  tool without entering the evaluator, so it cannot perform the
  data-last application (`x | f = f !{x}`) the edge demands.  Inside the
  evaluating child the bundled-first policy still holds — command
  dispatch reaches the in-process uutils path.  This was the latent bug:
  `true | cat` failed in any `coreutils`-feature build (the helper's
  wire guard rejected the value channels), invisible to per-package test
  runs and surfaced only by workspace feature unification.
- **The allocator owns the edge definition.** Stage `i` of `n` takes a
  value in iff `i > 0` and its input mode is non-`Bytes`, gives one out
  iff `i + 1 < n` and its output mode is non-`Bytes` — written once as
  `value_edge_in` / `value_edge_out` beside `open_stage_routes`
  (`core/src/runtime/pipeline/route.rs`), read by resolve's
  `carries_value_edge`.  No edge fact is cached on a struct, preserving
  the [[260610_child-eval-unification|unification]] series' stance that
  the modes are the single source.
- **The child owns forcing.** Whether a stage forces its result once
  before shipping (`force_pipe_value`) is a fact of holding a value-out
  channel.  It rides as `ChildKind::PipelineStage { force_output }`,
  derived in `serve_stage_core` from the channel the child holds — not
  as a wire-request field the parent must keep consistent with the
  wiring it performs.
- **The protocol owns its environment.** `HelperProtocol::wire` sets
  *or clears* every channel env var, so a spawned helper's protocol
  environment describes exactly that spawn.  This was the second latent
  bug: a helper-evaluated stage that launches a nested pipeline
  (bundled-tool dispatch re-enters `run_pipeline`) leaked its own
  `VALUE_IN_FD` to the nested helper, which EBADF'd on a fd closed in
  its process.

## The anchor is a multi-stage device

**A single-stage pipeline spawns no pgid anchor.**  The anchor's
invariant — a stage's `setpgid` join target must not die before a
*later* stage joins — is vacuous at n = 1, where the only child
establishes the group as its own leader on spawn, exactly the regime
`group.rs` already runs on non-Unix.  The earlier deferral ("only if
bundled-tool latency is felt") resolved against itself once
measurement showed every bundled `cat`/`ls` call pays the anchor fork
through `dispatch_uutils_via_pipeline`'s n = 1 pipeline, and a bundled
head inside a helper-evaluated stage pays it twice.  The case split is
one guarded early-return in `PipelineGroup::prepare`
(`core/src/runtime/pipeline/group.rs`), where the SIGINT/relay
invariant is documented.

## Consequences

- The wire request (`ChildEvalRequest`) carries no judgment a child can
  derive locally; `pack_request` has one caller-supplied gate fewer.
- One `transfer_error` constructor (`core/src/child_eval.rs`) phrases
  both serial-boundary crossings, preferring the value's own hint (a
  `Handle` says `await` on the pipeline edge too).
- The lambda-returning-block counterexample that overturned the
  original force-elaboration design is pinned in `ral/tests/pipeline.rs`
  (`value_producer_lambda_returning_block_is_forced_across_helper_value_edge`).
- Workspace-level `cargo test` (feature-unified, bundled tools on) is
  the suite that sees these paths; per-package runs do not.

Related: [[260609_pure-pipe-equation|pure-pipe-equation]] supplies the
equation each edge realises;
[[260610_evaluator-runtime-split|evaluator-runtime-split]] places all
of the above in `core/src/runtime/`.
