# Single binary

**The ral language runtime ships as a single executable.** The lexer,
elaborator, typechecker, evaluator, the bundled coreutils and grep, and the
capability sandbox are all linked into the one binary — none is a separate
program ral shells out to. Every re-exec is a *multicall* of this same binary
behind a hidden sentinel flag, never a sibling helper:

- a process-staged pipeline runs each ral stage — and each bundled tool, which
  is demoted to a ral stage — through `--ral-pipeline-stage-helper`, and pins
  the group's pgid with `--ral-pipeline-anchor` ([[design/pipelines|pipelines]]);
- an `fs`/`net` [[design/grant|grant]] that drops into an OS sandbox re-execs
  under `--internal-sandbox-block`, confined by the `sandbox_projection` of the
  live [[design/grant|grant]].

A re-exec'd child reconstructs a shell from a wire snapshot, evaluates, and
reports a structured outcome through one shared protocol ([[map/core/runtime|child_eval]]).
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
