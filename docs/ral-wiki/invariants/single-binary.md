# Single binary

**The ral language runtime ships as a single executable.** The lexer,
elaborator, typechecker, evaluator, the bundled coreutils and grep, and the
capability sandbox are all linked into the one binary — none is a separate
program ral shells out to. Every re-exec is a *multicall* of this same binary
behind a hidden sentinel flag, never a sibling helper — the flag is the first
argument, `ral_core::Role` spells each flag once, `ral_core::classify` is the
one place that reads it, and `ral_core::invocation::serve` the one place that
serves it; a re-exec is launched from a `Role`, never a string
(`sandbox::reexec::launch`). The test roles (`PgidCheck`, `DetachBirth`) exist
only under `test-util`, so release binaries ship none:

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

One more multicall exists, and it is by *name* rather than by sentinel flag: the
POSIX-bridge login shell (`docs/SPEC.md` §16.7), `ral` invoked as `ral-sh`, a
symlink beside it. It is the one case where a flag cannot serve, since an
`/etc/shells` entry takes no arguments and `argv[0]` is the one bit that
survives `exec`. `ral/src/bridge.rs` is the one place that reads the name; it
either replaces the process with `/bin/sh` or lets it continue as `ral`. It
divides nothing: there is no second executable.
