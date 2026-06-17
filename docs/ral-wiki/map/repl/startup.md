---
generated_at_commit: ae2a3f64
generated_at_date: 2026-06-17
covers_paths: [ral/src/main.rs, ral/src/platform.rs, ral/build.rs]
---

# Map: repl / startup

`ral/src/main.rs` is the process entry point. Before argv is parsed, `main`
restores the Unix signal dispositions and runs three multicall/sandbox
trampolines that can short-circuit the process: `try_run_pipeline_stage_helper`
(the parent re-execs `current_exe()` to run one pipeline stage in a fresh
subprocess), `test_helper::try_run_test_helper`, and `ral_core::sandbox::early_init`
(which consumes `--sandbox-projection`, pins the binary, and enters the OS
[[map/core/capabilities|sandbox]] for confined re-execs). It then refuses to run
setuid on Unix.

## Modes

`Cli` (clap) parses the surface, and `Cli::into_mode` distils it into a `Mode`:
`Login(InteractiveOpts)`, `Interactive(InteractiveOpts)`, `Script`, or
`Command`. The login bit (`-l` or a `-`-prefixed argv\[0\]) does not
short-circuit: a login shell with `-c` or a script positional resolves to
`Command`/`Script` and runs it, as cron, `su -`, and `$SHELL -l -c …` require;
login only selects between the two interactive variants, decided after `-c` and
the script positional are ruled out, and carries the same `InteractiveOpts`
(so `--norc` survives). `inject_arg_terminator` splices a `--` before the first
positional, and immediately after a `-c`, so flag-shaped script arguments and a
flag-shaped inline-code token (`ral -c '--version'`) survive clap parsing
verbatim. `RunOpts` (`--recursion-limit`, `--capabilities`) rides every mode;
`BatchOpts` adds `--audit` / `--pretty` / `--check` / `--dump-ast`.

The interactive/script fork lives in `main`: `-s` forces stdin-as-script, `-i`
forces the REPL, otherwise stdin being a tty decides. The REPL path hands off
to [[map/repl/loop|`repl::run_interactive`]].

## Batch execution

`run_batch` is the whole non-interactive pipeline: `parse` → `elaborate` →
`typecheck` → `eval_top_level`. The inference pass is not optional — it writes the
mode wires the evaluator reads
([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]) — so a batch
run always checks, taking `typecheck`'s `Result<Comp, Vec<TypeError>>`: a clean
check evaluates the fully annotated comp; any type error is fatal and blocks. A
script has no prior session, so the check seeds from the baked scheme list
(`SessionSchemes::from_schemes(PRELUDE.schemes())`)
([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]); `--check`
runs the same check and exits without evaluating. It owns
`--capabilities` composition through `apply_session_capabilities`, a thin map
from `ral_core::capability::apply_session_profiles`'s outcome to a process exit;
the composition itself (load each `.ral` profile, `meet` left-to-right, freeze
against home/cwd, push one permanent session ceiling) lives in core
([[design/grant|grant]]). `--audit` wraps the run in a traced
[[map/core/evaluator|execution tree]] emitted as JSON.

## Embedding and the baked prelude

A turn-evaluating host needs three things before its own builtins, rc files, or
capability frames: the prelude as a baked [[map/core|`Comp`]], its top-level
scheme list, and a `Shell` seeded from both. `main` reaches for them through
core's host-embedding API — a process-wide
`PRELUDE: ral_core::host::BakedPrelude` static built by the
`ral_core::baked_prelude!()` macro, and
`ral_core::host::boot_shell(terminal, &PRELUDE)`, which constructs the shell,
seeds default env vars, registers builtins against the prelude comp, and installs
the prelude's type hints. `BakedPrelude` lazily `postcard`-decodes the IR and
scheme blobs on first access.

`build.rs` is the git-hash block — stamping `RAL_GIT_HASH` into the version
string — plus one call to `ral_core::host::bake_prelude_to_out_dir`, which
parses, elaborates, and `bake_prelude`s `prelude.ral` (annotating each top-level
bind with its inferred scheme and harvesting those same schemes off one checked
pass), then serialises the *annotated* `Comp` and the harvested schemes into
`OUT_DIR`. This is the consumer half of core's schema-less prelude discipline
([[map/core|core]]): a crate's build script cannot depend on the crate it builds,
so core cannot bake its own prelude, and each embedding host bakes it from core's
source. Evaluating the annotated prelude installs each binding's scheme into
scope[0], so the per-turn seed and the baked list agree by construction
([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).

## Platform glue

`ral/src/platform.rs` centralises the host queries the binary needs:
`probe_terminal` (under `RAL_INTERACTIVE_MODE`), `home_dir` / `user_name`, and
`load_exit_hints` (user override in the data dir, else the embedded
`data/exit-hints.txt`). Default-env seeding is core's: `boot_shell` calls
`Shell::seed_default_env_vars`.

Before any builtin lookup, `main` calls `repl::register_host_surface()` to
publish the ral host surface — the [[map/repl/plugins|`_ed-*` builtins]] and
core's `WATCH_BUILTIN` — into core's host table for all modes, since the
typechecker consults `builtin_names()` even in batch. Registration is only the
typecheck half; each path also *installs* `WATCH_BUILTIN` into its shell so the
builtin runs — the REPL session at boot, the batch (`run_batch`) path after
`boot_shell`. The ral binary has a durable stdout sink in every mode, where an
agent host does not, so `watch` is the ral host's to install
([[decisions/260617_watch-repl-builtin|watch-repl-builtin]]).
