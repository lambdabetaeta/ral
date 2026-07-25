---
generated_at_commit: f7cf93a
generated_at_date: 2026-07-25
covers_paths: [ral/src/main.rs, ral/src/cli.rs, ral/src/batch.rs, ral/src/platform.rs, ral/build.rs]
---

# Map: repl / startup

The `ral` binary startup is a three-part front door: **`main.rs` performs
process dispatch, `cli.rs` distils argv into a `Mode`, and `batch.rs` runs
non-interactive programs through core's framed run door**. Interactive sessions
then hand to the REPL, above a pre-clap dispatch that lets the binary re-enter
itself in confined or helper roles.

## Pre-`main` dispatch

Before argv is parsed, `main` restores the Unix signal dispositions and runs a
chain of self-re-exec stages that can short-circuit the process. Each returns
an `Option<u8>` exit code; the currency is normalised so the same chain runs
identically in `main` and in a test constructor
([[decisions/260618_run-turn-host-loop|run-turn-host-loop]] keeps hosts
uniform). The chain is two-staged around the sandbox:

- **Engine entry** (Unix) — `--engine` hands the process to
  `ral_core::engine::run_engine` before anything else: the wire-engine child
  boots the real shell itself through the installer's boot recipe — the REPL's
  tag (`ENGINE_INSTALLER_TAG = "repl"`) carries `engine_boot_shell`, the baked
  prelude over the empty `HostSurface`, since the
  captured host builtins are boot-time closures a child cannot construct.
- **Helper trampolines** — `try_run_pipeline_stage_helper` (the parent re-execs
  `current_exe()` to run one pipeline stage in a fresh subprocess) and
  `test_helper::try_run_test_helper`.
- **Sandbox entry** — `ral_core::sandbox::early_init(&argv)` returns the
  *stripped* post-init argv together with an optional exit. It consumes
  `--sandbox-projection`, pins the binary, and enters the OS
  [[map/core/capabilities|sandbox]] for a confined re-exec.
- **Confined-child tails**, dispatched on the stripped argv *after* `early_init`
  so a projected child enters the sandbox first, then runs the target inside it:
  `ral_core::sandbox::serve_sandbox_exec` (`--ral-sandbox-exec <program> …`
  `execve`s the host program inside the Seatbelt just entered) and
  `ral_core::try_run_bundled_tool` (`--ral-bundled-tool <tool> …` runs the
  bundled coreutils/ripgrep image in-process, confined). A bundled tool runs as
  an external child whenever process semantics are required
  ([[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]]),
  and each external child launches under the effective policy
  ([[decisions/260617_sandbox-external-children|sandbox-external-children]]).

Both tails exit here, never reaching clap. After dispatch, `main` refuses to
run setuid on Unix.

## Modes

`ral/src/cli.rs` owns the clap surface. `Cli::into_mode` distils it into a
`Mode`: `Login(InteractiveOpts)`, `Interactive(InteractiveOpts)`, `Script`, or
`Command`. The login bit (`-l` or a `-`-prefixed argv\[0\]) does not
short-circuit: a login shell with `-c` or a script positional resolves to
`Command`/`Script` and runs it, as cron, `su -`, and `$SHELL -l -c …` require;
login only selects between the two interactive variants, decided after `-c` and
the script positional are ruled out, and carries the same `InteractiveOpts`
(so `--norc` survives). `RunOpts` (`--recursion-limit`, `--capabilities`) rides
every mode; `BatchOpts` adds `--audit` / `--pretty` / `--check` / `--dump-ast`;
`InteractiveOpts` adds `--norc`, `-i`, `-s`, and `--surface`.

- **Argv terminator.** `inject_arg_terminator` splices a `--` before the first
  positional, and immediately after a `-c`, so flag-shaped script arguments and
  a flag-shaped inline-code token (`ral -c '--version'`) survive clap parsing
  verbatim. A long flag that takes a separate value token (`--surface readline`,
  `--recursion-limit 4096`) must carry that value past itself rather than have
  the terminator fenced between them; the set of such flags is read from clap's
  own model (`value_taking_longs`), so the injector can never disagree with the
  `Cli` definition.
- **Frontend selection.** `--surface <minimal|readline|structural>` (a clap
  `ValueEnum`, default `readline`) picks the interactive frontend, overriding
  the rc `surface:` key; `None` defers to rc, then the default. The capability
  axis stays the `RAL_INTERACTIVE_MODE` env var — the escape hatch for setups
  that do not own argv — and still wins over the surface choice. Surface
  internals live in [[map/repl/frontend|frontend]].
- **Interactive/script fork.** `InteractiveOpts::reads_stdin_as_script` encodes
  the precedence: `-s` forces stdin-as-script and beats `-i`, `-i` forces the
  REPL, otherwise stdin being a tty decides; a script positional, if present,
  is run instead of either. The REPL path hands off to
  [[map/repl/loop|`repl::run_interactive`]].

## Batch execution

`ral/src/batch.rs` owns the whole non-interactive pipeline. `run_batch` parses,
elaborates, typechecks, then **runs the program through core's framed run door
rather than evaluating it directly**
([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]): the same
`Shell::run` entry every host shares, handed a `Program::Source` run
([[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]]).

- **The check.** The inference pass is not optional — it writes the mode wires
  the evaluator reads
  ([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]) — so a
  batch run always typechecks, taking `typecheck`'s
  `Result<Comp, Vec<TypeError>>`. A script has no prior session, so the check
  seeds from the baked scheme list plus the surface's builtin table
  (`SessionSchemes::from_schemes(PRELUDE.schemes(), host_surface.builtin_table())`)
  ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]); a
  clean check returns the fully annotated comp, any type error is fatal, and
  `--check` runs the same check and exits without evaluating.
- **The run.** `shell.run(RunRequest { … })` with
  `Program::Source(source)` runs the annotated comp under
  `Capabilities::root()` with no wall or detached limit, inheriting IO and
  stdin. Its `RunReport` has two arms: `Ran` yields the `result` to score;
  `Static` cannot occur on this path (batch already typechecked) and is
  treated defensively as a fatal exit.
- **The foreground gate.** The request's `RequestedTerminalAccess` is `Leased`
  only when the probed terminal carries `startup_foreground`, else `Denied` —
  the authority to hand the controlling terminal to a child is a held value,
  not an inferred predicate ([[decisions/260619_terminal-lease|terminal-lease]]).
- **The verdict.** The `Settled` result is scored into an exit code: `Ok`
  reports the shell's `last_status`; `Escape::Exit(code)` clamps and returns it
  (`platform::exit_byte`); `Error` prints a runtime diagnostic (unless
  `--audit` will carry it) and returns the error's exit code; on Unix a
  `Stopped` escape exits 1.
- **Capabilities and audit.** `--capabilities` composition goes through
  `platform.rs::apply_session_capabilities` (shared with the REPL boot), a
  thin map from `ral_core::capability::apply_session_profiles`'s outcome to a
  process exit; the composition itself (load each `.ral` profile, `meet`
  left-to-right, freeze against home/cwd, push one permanent session ceiling)
  lives in core ([[design/grant|grant]]). `--audit` wraps the run in a traced
  [[map/core/evaluator|execution tree]] emitted as JSON.

## Embedding and the baked prelude

A run-evaluating host needs three things before rc files or capability
frames: the prelude as a baked [[map/core|`Comp`]], its top-level scheme
list, and its own builtin surface as a `ral_core::HostSurface`. `main`
reaches for them through core's *driver* — the Shell-embedding seam — via a
process-wide `PRELUDE: ral_core::driver::BakedPrelude` static built by the
`ral_core::baked_prelude!()` macro, and
`ral_core::driver::boot_shell(terminal, &PRELUDE, surface)`, which constructs
the shell, installs the surface next to `CORE_BUILTINS`, seeds default env
vars, and registers builtins against the prelude comp. `BakedPrelude` lazily
`postcard`-decodes the IR and scheme blobs on first access. Probing the
underlying machine is a separate concern, owned by `ral_core::host`.

`build.rs` is the git-hash block — stamping `RAL_VERSION_SUFFIX` (`+<hash>`
in a git checkout, empty in a release tarball) into the version string — plus
one call to `ral_core::driver::bake_prelude_to_out_dir`, which
parses, elaborates, and `bake_prelude`s `prelude.ral` (annotating each top-level
bind with its inferred scheme and harvesting those same schemes off one checked
pass), then serialises the *annotated* `Comp` and the harvested schemes into
`OUT_DIR`. This is the consumer half of core's schema-less prelude discipline
([[map/core|core]]): a crate's build script cannot depend on the crate it builds,
so core cannot bake its own prelude, and each embedding host bakes it from core's
source. Evaluating the annotated prelude installs each binding's scheme into
scope[0], so the per-run seed and the baked list agree by construction
([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).

## Platform glue

`ral/src/platform.rs` centralises the host queries and shared exits the
binary needs: `probe_terminal` (under `RAL_INTERACTIVE_MODE`), `home_dir`,
`load_exit_hints` (user override in the data dir, else the embedded
`data/exit-hints.txt`), `exit_byte` (the one clamp-and-narrow every mode's
final code funnels through), and `apply_session_capabilities` (above).
Default-env seeding is core's: `boot_shell` calls
`Shell::seed_default_env_vars`.

Builtins are shell-scoped: each mode declares its surface as one
`HostSurface` value and hands it to `boot_shell`, so the checker surface and
the runtime surface cannot drift. Batch's surface is core plus
`WATCH_BUILTIN`, and the same value seeds `--check`'s builtin table
(`HostSurface::builtin_table`, the checker with no live shell); the REPL's
adds the [[map/repl/plugins|`_ed-*` builtins]] and the captured session
commands. The ral binary has a durable stdout sink in every mode, where an
agent host does not, so `watch` is the ral host's to install
([[decisions/260617_watch-repl-builtin|watch-repl-builtin]]).
