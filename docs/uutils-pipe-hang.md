# uutils-to-uutils pipeline hang — RESOLVED

## Resolution

Bundled coreutils dispatch now spawns a helper subprocess instead of
running `uumain` in-process.  ral itself is the helper: the parent
process spawns `current_exe()` with a hidden multicall sentinel as
the first argument, and the helper-recognising path at the top of
`main` dispatches to `ral_core::builtins::uutils::uutils_helper`,
which calls `uumain` and exits with the returned code.  Each helper
runs in a fresh OS process; pipe stdin/stdout are kernel-managed
across the parent/child boundary; there is no in-process fd 0/1
contention to serialise.

## Diagnosis (kept for reference)

The original in-process design serialised uutils calls through a
global `STDIO_REDIRECT_LOCK` to keep concurrent threads from
interleaving `dup2`s of fd 0/1.  Pipeline stages run on independent
threads (`core/src/evaluator/pipeline/stages.rs:674`).  The lock
**serialised** but did not **order** them: if the downstream stage
won the lock race it blocked on a pipe read while the upstream
stage was waiting for the same lock to write — classic out-of-order
acquisition deadlock.  Heisenbug behaviour observed earlier
(eprintln "fixes", `CwdGuard` no-ops "breaking" things) was
scheduling perturbation flipping which thread reached the lock
first.

## What actually changed

- `core/src/builtins/uutils.rs`: `uutils()` builds a `Command`,
  wires stdin/stdout from `shell.io` via `Sink::wire_command_stdout`
  + `Source::take_reader`, propagates `dynamic.env_vars` /
  `dynamic.cwd` through `Command::env` / `current_dir`, then spawns.
  The whole `EnvGuard` / `CwdGuard` / `_fd_lock` / `with_child_stdout`
  scaffolding is gone.
- `core/src/builtins/uutils.rs`: `try_run_uutils_helper` is the
  one-line check that ral and exarch's `main` call at the very top.
  When `argv[1] == "--ral-uutils-helper"`, dispatch and exit; else
  return `None` and let normal `main` run.
- `core/src/io.rs`: `Sink::with_child_stdout` removed (no callers).
- `core/src/compat.rs`: stdout-redirection helpers removed; stdin
  helpers and `STDIO_REDIRECT_LOCK` re-gated on `feature =
  "diffutils"` only (`uu_cmp` still uses them).
- `core/Cargo.toml`: drops the `clap` and `uucore` direct deps
  that the in-process check_paths / EXIT_CODE-reset experiments
  added; uutils crates pull them transitively as before.

## Tradeoffs

- ~1 ms fork+exec per uutils call.  Invisible interactively;
  noticeable in tight scripted loops over thousands of files.
- Multicall sentinel `--ral-uutils-helper` is a hidden flag on the
  binary.  Confused-deputy concerns considered: the helper grants
  no privilege beyond what spawning `/usr/bin/cat` did before
  bundling, exarch's OS sandbox is process-tree and constrains the
  helper child the same way it constrains any external command,
  and the flag's hiddenness is cosmetic — `strings ral` reveals
  it.  No user can stumble into it; only a process explicitly
  invoking via `Command::new(...).arg("--ral-uutils-helper")`
  triggers it.

## Verified

- `cat | head`, `cat | wc -l`, `cat | tac`, `cat | cat`, `ls | head`
  all pass.
- `cargo test -p ral-core`: 253 + 203 + 121 + 54 + 6 across suites,
  zero failures.
- All `.ral` integration tests under `tests/{builtins,lang,practical,unix}`:
  55 passing, 0 failing.
- `within [env: ...] { printenv X }` propagates correctly via
  `Command::env` (parallel to `apply_env` for external commands).
- `within [dir: ...] { ls }` resolves relative to scoped cwd via
  `Command::current_dir`.
