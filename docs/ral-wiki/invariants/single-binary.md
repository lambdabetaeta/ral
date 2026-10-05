# Single binary

**The ral language runtime ships as a single executable.** The lexer,
elaborator, typechecker, evaluator, the bundled coreutils and grep, and the
capability sandbox are all linked into the one binary — none is a separate
program ral shells out to. Every re-exec is a *multicall* of this same binary
behind a hidden sentinel flag, never a sibling helper — the flag is the first
argument, and `ral_core::classify` is the one place that reads it:

- a multi-stage pipeline pins its process group's pgid open for the
  pipeline's whole life with a lone anchor, `--ral-pipeline-anchor`
  ([[design/pipelines|pipelines]]) — the only stage-adjacent re-exec left,
  since a ral-written stage now runs on a thread of the parent process rather
  than a child of its own, and a bundled tool re-execs as
  `--ral-bundled-tool <tool>` whether standalone or a stage;
- an `fs`/`net`/`exec` [[design/grant|grant]] confines an external child by
  re-execing ral as `ral --warrant` (macOS directly, Linux inside `bwrap`),
  the confinement to enter and the program to become arriving on a descriptor
  rather than argv, or at spawn on Windows (an AppContainer, no child re-exec
  there), confined by the `sandbox_projection` of the live
  [[design/grant|grant]];
- a wire-seat agent hatch re-execs an engine child under `--engine`, seeded
  from an `EngineSeed` the parent packs ([[map/core/transport|transport]]).

This in-process linking is also what routes the bundled tools through the same
capability chokepoint as the structured primitives.

The hard rule concerns the runtime: do not factor ral's functionality into a
sibling helper crate or a second shipped executable, and do not introduce a
runtime dependency on an external program for core behaviour.

One deliberate piece stands *outside* the runtime: `ral-sh`, a thin POSIX-bridge
login shell (`docs/SPEC.md` §16.7). It carries no `ral-core` dependency and
forwards non-interactive invocations to `/bin/sh`; it exists so ral can be
registered as a login shell, not to divide the runtime. It is a registration
shim, not a functional split.
