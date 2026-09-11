---
generated_at_commit: d9abfb52
generated_at_date: 2026-09-11
covers_paths: [ral/src/main.rs, ral/src/startup.rs, ral/src/cli.rs, ral/src/batch.rs, ral/src/platform.rs, ral/build.rs]
---

# Map: repl / startup

The `ral` binary startup is a four-part front door: **`startup.rs` decides what
kind of process this is, `cli.rs` distils argv into a `Mode`, `main.rs`
dispatches the answer, and `batch.rs` runs non-interactive programs through
core's framed run door**. Interactive sessions then hand to the REPL, above a
pre-clap dispatch that lets the binary re-enter itself in confined or helper
roles.

`main` itself holds no substance — it refuses to run setuid, adopts
the process dispositions, asks `startup::identify()` what this process is, and
runs the session named. `identify` answers with a two-armed `Invocation`:
`Shell(Mode)`, or `Exit(ExitCode)` for a re-exec child that has already done
its work (and for an invocation refused before it could become one).

## Pre-`main` dispatch

Before argv is parsed, `main` refuses to run setuid — the shell inherits the
caller's environment and must not run elevated, and neither must any re-exec
child, so the refusal precedes the whole chain rather than trailing it —
restores the Unix signal dispositions, and runs a chain of self-re-exec stages
that can short-circuit the process. Each core stage returns an `Option<u8>`
exit code, which `identify` lifts to `Invocation::Exit`. The chain is
two-staged around the sandbox:

- **Engine entry** (Unix) — `--engine` hands the process to
  `ral_core::engine::run_engine` before anything else: the wire-engine child
  boots the real shell itself through the installer's boot recipe. That recipe
  is one `EngineInstaller` const in `startup::engine` — tag (`"repl"`), boot
  shell, grant policy. The tag carries the baked prelude over the empty
  `HostSurface`, since the captured host builtins are boot-time closures a
  child cannot construct; the grant policy is a refusal, because the shell
  spawns no child engines and has no base-tag lexicon to resolve a grant
  against.
- **Helper trampolines** — `try_run_pipeline_anchor` (`--ral-pipeline-anchor`,
  the one multicall re-exec a pipeline still uses: a stage itself now runs on
  a thread of the parent process, but a multi-stage pipeline still needs one
  process to hold its pgid open for its whole life) and
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

Both tails exit here, never reaching clap.

## Modes

`ral/src/cli.rs` owns the clap surface, and `Mode::from_argv` is the one door
through it: terminator injection, clap, then `Cli::into_mode`. The mode tests
enter by the same door, so the parse they exercise cannot drift from the one
`startup::identify` performs. `Cli::into_mode` distils the parsed flags into a
`Mode`: `Interactive(InteractiveOpts)`, `Script`, or `Command` — login is not a
mode of its own but a flag on `InteractiveOpts` (`login`, set by `-l` or a
`-`-prefixed argv\[0\]), so a login shell with `-c` or a script positional
still resolves to `Command`/`Script` and runs it, as cron, `su -`, and
`$SHELL -l -c …` require; the flag matters only once resolution has landed on
`Interactive`, where it gates login-profile sourcing at REPL boot
([[map/repl/loop|loop]]). `RunOpts` (`--recursion-limit`, `--capabilities`)
rides every mode; `BatchOpts` adds `--audit` / `--pretty` / `--check` /
`--dump-ast`; `InteractiveOpts` adds `-l`, `--norc`, `-i`, `-s`, and
`--surface`.

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

`ral/src/batch.rs` owns the whole non-interactive pipeline, from the source
text inwards: `run_file` reads a script path, `run_stdin` reads a piped script,
and both meet `-c` at `run_source`, which is therefore where line endings are
normalised — one door, one rule. `run_source` parses,
elaborates, typechecks, then **runs the program through core's framed run door
rather than evaluating it directly**
([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]): the same
`Shell::run` entry every host shares, handed a `Program::Source` run
([[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]]).

- **The check.** The inference pass is not optional — it writes the ground
  annotations the evaluator reads
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
  `GrantStack::root()` with no wall or detached limit, inheriting IO and
  stdin. Its `RunReport` has two arms: `Ran` yields the `result` to score;
  `Static` cannot occur on this path (batch already typechecked) and is
  treated defensively as a fatal exit.
- **The foreground gate.** The request's `RequestedTerminalAccess` is `Leased`
  only when the probed terminal carries `startup_foreground`, else `Denied` —
  the authority to hand the controlling terminal to a child is a held value,
  not an inferred predicate ([[decisions/260619_terminal-lease|terminal-lease]]).
- **The verdict.** The `Settled` result is scored into an exit code: `Ok`
  reports 0; `Escape::Exit(code)` clamps and returns it
  (`platform::exit_byte`); `Error` prints a runtime diagnostic (unless
  `--audit` will carry it) and returns the error's exit code
  ([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]: there is no
  `Stopped` escape to score).
- **Capabilities and audit.** `--capabilities` composition goes through
  `platform.rs::apply_session_capabilities` (shared with the REPL boot), a
  thin map from `ral_core::capability::apply_session_profiles`'s outcome to a
  process exit; the composition itself (load and freeze each `.ral` profile
  against home/cwd, pushing it as its own layer onto the session
  `GrantStack`) lives in core ([[design/grant|grant]]). `--audit` wraps the
  run in a traced [[map/core/evaluator|audit trail]] emitted as JSON.

## Embedding and the baked prelude

A run-evaluating host needs three things before rc files or capability
frames: the prelude as a baked [[map/core|`Comp`]], its top-level scheme
list, and its own builtin surface as a `ral_core::HostSurface`. Every mode
reaches for them through `ral_core::boot` — the Shell-embedding seam — via the
crate-root `PRELUDE: ral_core::boot::BakedPrelude` static built by the
`ral_core::baked_prelude!()` macro, and
`ral_core::boot::boot_shell(terminal, &PRELUDE, surface)`, which constructs
the shell, installs the surface next to `CORE_BUILTINS`, seeds default env
vars, and registers builtins against the prelude comp. `BakedPrelude` lazily
`postcard`-decodes the IR and scheme blobs on first access. Probing the
underlying machine is a separate concern, owned by `ral_core::host`.

`build.rs` is the git-hash block — stamping `RAL_VERSION_SUFFIX` (`+<hash>`
in a git checkout, empty in a release tarball) into the version string — plus
one call to `ral_core::boot::bake_prelude_to_out_dir`, which
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
