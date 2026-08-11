---
status: active
---

# One debug path: build-gated, no flags

**ral has a single debug-tracing primitive, `dbg_trace!`, gated on
`debug_assertions` alone — tracing is a property of how the binary was
compiled, never of the runtime environment.**

- `dbg_trace!(tag, …)` (in [[map/core/diagnostics|diagnostics]]) emits a
  tagged red line to stderr in debug builds and expands to nothing in
  release. Its call sites are *permanent instrumentation* — kept in the
  source, not added and removed per investigation.
- The same `debug_assertions` gate governs the `try`-caught-error echo
  ([[design/control-operators|control-operators]], `eval_try`): debug
  builds echo `ral: try caught error …` to stderr; release builds stay
  silent (`docs/SPEC.md` §8.6).

**Why one path.** A second, environment-gated channel earns nothing: a
developer who wants traces builds in debug, and a release user wants
silence and gets it unconditionally. A single build-time gate removes the
question of *which switch is live* — there is no switch.

**Consequence — drain stderr concurrently.** Because a debug build traces
unconditionally, any consumer of a debug ral child's stderr must drain it
*while* waiting on the child: the per-child wait/drain traces can outrun a
pipe buffer and block the writer. The runtime drains its pump streams on
their own threads; a harness spawning ral does the same — `run_with_timeout`
in `ral/tests/pipeline.rs` reads both pipes on reader threads rather than
after the child exits.
