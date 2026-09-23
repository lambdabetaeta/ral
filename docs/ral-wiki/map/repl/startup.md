---
generated_at_commit: e2b7067b
generated_at_date: 2026-09-23
covers_paths: [ral/src/main.rs, ral/src/startup.rs, ral/src/cli.rs, ral/src/batch.rs, ral/src/boot_door.rs, ral/src/platform.rs, ral/build.rs]
---

# Map: repl / startup

The `ral` binary startup is a four-part front door: **`startup.rs` decides what
kind of process this is and holds the binary's engine installers, `cli.rs`
distils argv into a `Mode`, `main.rs` dispatches the answer, and `batch.rs`
runs non-interactive programs through an engine it boots, speaking only the
protocol**. Interactive sessions then hand to the REPL, above a
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
  `ral_core::engine::run_engine(&engine::INSTALLERS)` before anything else,
  so a wire-engine child boots through the very recipes the in-process
  front-ends boot through. `startup::engine::INSTALLERS` holds two
  `EngineInstaller`s: `repl` (`boot_repl` over the REPL surface, decoding
  `Attach.config` as `ReplConfig {login}`) and `batch` (`boot_batch` over the
  batch surface, decoding `BatchConfig {args}`), each registering the boot
  door. Both share one grant policy, a refusal (`no_seeded_children`),
  because the shell spawns no child engines and has no base-tag lexicon to
  resolve a grant against.
- **Helper trampolines** — `try_run_pipeline_anchor` (`--ral-pipeline-anchor`,
  the one multicall re-exec a pipeline uses: a stage itself runs on
  a thread of the parent process, but a multi-stage pipeline needs one
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
normalised — one door, one rule. Batch holds no `Shell`: it boots an
`IdentityTransport` from the `batch` installer and speaks only the protocol,
through `dispatch_to_report` under the mute host `()`, so it shares the REPL's
one ending law and one typecheck
([[design/engine-protocol|engine-protocol]]).

- **Static work.** `--check` and `--dump-ast` parse, elaborate and (for
  `--check`) typecheck the source against `PRELUDE`'s schemes plus the batch
  surface's builtin table, and boot nothing.
- **Boot, stage one.** `IdentityTransport::boot` with the `batch` installer
  (the table is not `cfg(unix)`-gated, only `run_engine` is), whose recipe
  decodes `Attach.config` — `{args}` — strictly, boots the batch surface, sets
  exit hints and args, and registers the boot door. `platform::local_attach`
  builds the attach at this process's cwd and home, the probed terminal in
  `Attach.terminal`. A refusal exits 2 with its sentence on stderr.
- **Boot, stage two.** The first dispatch is `Program::Hook` on
  `Session "boot"`, the `_ral-boot` builtin itself (`ral/src/boot_door.rs`),
  applied to the REPL's own record, `{login: false, no_rc: true,
  recursion_limit, capabilities}`. The door is one-shot — it unregisters its
  own hook — sources no startup file here, sets the recursion limit, and
  applies `--capabilities` under its own mooring. `boot_door::settle` reads
  its `Ending`: a load failure is `Raised` status 2, an `exit N` is
  `Exited(N)`, and either stops batch there, exactly as it stops the REPL.
- **The run.** The script is a `Program::Source` dispatch under
  `GrantStack::root()` and the mute host, inheriting IO and stdin; the engine
  typechecks it once against the live session. The terminal is `Leased` only
  when the probed terminal carries `startup_foreground`
  ([[decisions/260619_terminal-lease|terminal-lease]]). A watched worker's
  lines reach batch's deferred sink, which prints them to stdout.
- **The verdict.** batch prints the `Report`'s `rendered` as the REPL does and
  exits with the ending's status through `platform::exit_byte`.
- **Audit.** `--audit` asks the script's dispatch for its trail
  (`Run.trail`) and builds the envelope from the `Report`: the trail, and an
  `` `err `` outcome from the failing ending's `record` — the one `try` hands
  its handler.

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
binary needs: `probe_terminal` (under `RAL_INTERACTIVE_MODE`),
`local_attach` (an in-process engine's `Attach`, at this process's own cwd and
home), `load_exit_hints` (user override in the data dir, else the embedded
`data/exit-hints.txt`), and `exit_byte` (the one clamp-and-narrow every mode's
final code funnels through).
Default-env seeding is core's: `boot_shell` calls
`Shell::seed_default_env_vars`.

Builtins are shell-scoped: each mode declares its surface as one
`HostSurface` value and hands it to `boot_shell`, so the checker surface and
the runtime surface cannot drift. Batch's surface is core plus
`WATCH_BUILTIN`, `SURFACE_BUILTIN` and the boot door, and the same value seeds `--check`'s builtin table
(`HostSurface::builtin_table`, the checker with no live shell); the REPL's
adds the [[map/repl/plugins|`_ed-*` doors and the plugin load doors]], all
static. Both ral front-ends print a watched worker's lines, so `watch` is the
ral hosts' to install
([[decisions/260617_watch-repl-builtin|watch-repl-builtin]]).
