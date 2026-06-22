# Log

Append-only timeline. Each entry begins `## [YYYY-MM-DD] kind | title`
(kind ∈ ingest, query, lint, migrate) so `grep '^## \[' log.md | tail` works.

## [2026-05-30] migrate | Wiki scaffolded

Established the wiki at repo-root `wiki/`: maintainer contract, index, log, and
the three layers (`design/` + `invariants/` durable, `decisions/` chronological,
`map/` volatile and git-drift-stamped).

## [2026-05-30] migrate | RATIONALE and dev/docs absorbed

Authored the durable and decision layers.

- **design/**, distilled from `docs/RATIONALE.md` (cross-linked, not copied):
  cbpv, effects-handlers, grant, row-types, control-operators, scoping.
- **invariants/**: fixed-arity, single-binary, ir-pure-cbpv,
  optionality-via-variants.
- **decisions/**: the settled decisions and recorded design proposals from the
  dated `dev/docs` memos (hot-path-cancellation, redirect-drop, typed-state-flow,
  repl-architecture, background-tool-calls), plus the standing design decisions
  (handlers deep + self-masking, completion-escape refactor, escape-propagation
  bug fixes, repl-builtins layering, ast-stays-flat, infer-case-stays-whole,
  env-overrides overlap, linux-exec-confinement).
- **map/**: core, repl, exarch, all stamped @88e94f0.

The exploratory backlog memos in `dev/docs/` (`old_points_of_smell`, `patterns`,
`improve-language`, `plugin-improvements`) are open backlog, not durable
knowledge; they stay in `dev/docs/` as the raw source the decision pages cite.

## [2026-05-30] lint | Claims verified against code

Verified the pages against current code and git. Two corrections:
[[invariants/single-binary]] was rescoped from "exactly one executable" (the
`ral-sh` companion exists, `docs/SPEC.md` §21.1) to the runtime;
[[decisions/260530_ast-stays-flat]] corrected to 22 variants. Confirmed the
completion/escape refactor is complete — `EvalSignal` is gone and the surface is
`Settled<Value>` with `Escape` / `BodyResult`.

## [2026-05-30] ingest | ral-core mapped

Decomposed the thin `map/core` placeholder into an overview hub plus ten
subsystem map pages under `map/core/`, each stamped `@c6b3da5` with tight
`covers_paths`:

- syntax (`core/src/syntax/`, `classify.rs`), elaboration (`elaborator.rs`),
  ir (`ir.rs`), typecheck (`typecheck/`, `ty.rs`), evaluator (`evaluator/`).
- capabilities (`capability/`, `sandbox/`, `path/`), io-process (`io/`,
  `process/`, `stream.rs`), builtins (`builtins/`), shell-state (`types/`),
  prelude (`prelude.ral`).

Pages name real module paths, key types, and entry-point functions
(`compile`/`compile_and_typecheck`, `elaborate`, `typecheck`,
`eval_top_level`/`apply`/`evaluate`, `EffectiveGrant`, `early_init`,
`CORE_BUILTINS`), and link out to the durable theory, invariants, and decision
pages rather than restating them. `index.md` lists the new pages under the
`map/core` hub.

Correction folded in: the prior placeholder said `if` / `case` / `try` were
defined in `prelude.ral`. They are IR primitives (`CompKind::If` / `Case` /
`Try`) introduced at elaboration; the prelude carries the library-level
combinators (`for`, `each`, `map`, `filter`, `fold`, …) and defines no `while`.
The prelude page states this accurately.

## [2026-05-30] ingest | exarch and ral binary mapped

Thickened the two remaining thin hubs into concrete subsystem maps, stamped
`@ce51d4e`.

- **map/exarch/**: session (turn loop, auto-compaction, sub-agent fork),
  provider (transport + retry + pricing), shell-eval (a tool call as a ral
  top-level turn under a pushed grant frame), policy (capability composition and
  the bake-in profiles), tools (`shell`/`agent`/`fff`), frontend (event bus,
  session log, inline TUI + headless). exarch's sandbox is ral's `grant`, not a
  separate mechanism.
- **map/repl/**: startup (argv → `Mode`, baked prelude), loop (the `Session`
  state machine), frontend (the `Frontend` trait, minimal + rustyline editors),
  plugins (the `_ed-*` editor builtins), jobs (process-group control).

Both confirm rather than contradict existing decisions. The repl-builtins
layering holds — core keeps the editor context type-erased as `Box<dyn Any>` and
never inspects it. The backgroundable-tool-work direction is partly realised in
`exarch/src/session.rs` (staged dispatch, `agent` fan-out); flagged against
[[decisions/260523_background-tool-calls]] for follow-up.

## [2026-05-30] lint | Reconciled wiki with implemented reality

Checked three drift points against the code. The backgroundable-tool-work
decision was recorded as *proposed*, but its first phase has landed: `dispatch`
stages calls into a `Vec<Staged>` and joins in a second pass, with the `agent`
tool fanning out concurrently on the scope. The Provider is still borrowed (no
`Arc<Provider>`), `Staged` has only `Done`/`Spawned`, the `shell` tool is
synchronous, and there is no job registry, auto-notify, or per-job cancel. Set
that decision to *active* and rewrote it to state the landed half and the open
half plainly; bumped its index tag accordingly. The session map page already
described the staging accurately, so it was left unstamped.

Confirmed the provider set is exactly Anthropic / OpenAI / OpenRouter /
DeepSeek; the exarch hub and provider map pages already reflect this, no change.
Confirmed the `_ed-*` family is eighteen ops (the `ED_BUILTINS` table); the
plugins map page enumerates them correctly. Generalised the repl-builtins
decision's parenthetical so it no longer names only two of the family — the
decision is about layering, not the roster.

## [2026-05-30] migrate | Wiki moved under dev/

Relocated the vault from repo-root `wiki/` to `dev/wiki/` so it inherits the
`dev/` exclusion in `deploy-public.ral` and never reaches the public mirror —
the wiki is private working knowledge, browsed locally, not published. Internal
wikilinks are relative and unaffected by the move. The maintainer contract
(`CLAUDE.md`) is force-tracked, since the repository's `.gitignore` ignores
`CLAUDE.md` by basename. Local browsing is `dev/wiki-quartz.sh` (Quartz under
bun) or Obsidian opened on `dev/wiki/`.

## [2026-05-30] migrate | Added the audit concept page

Authored [[design/audit]] — the durable concept previously folded into
[[design/control-operators]]: execution as a structural tree owned lexically by
the producing scope, with process boundaries only transporting fragments back,
and each node carrying its `principal`. Grounded against
`core/src/evaluator/audit.rs` and `docs/SPEC.md` §10.1, §10.3.

## [2026-05-30] migrate | Browse via Obsidian

Settled on Obsidian for local browsing: open `dev/wiki/` as a vault for
wikilinks, graph view, backlinks, and search with no build step. The earlier
Quartz helper was dropped — Quartz v5's interactive TUI-config flow does not
bootstrap headlessly, and a viewer that needs manual setup is not worth
carrying.

## [2026-05-31] migrate | Maintainer contract moved to AGENTS.md

`git mv dev/wiki/CLAUDE.md dev/wiki/AGENTS.md` so the contract sits under the
harness-agnostic filename rather than a Claude-specific one — the body was
already harness-neutral ("you — the LLM"), only the filename was not. A side
benefit: the repository `.gitignore` ignores `CLAUDE.md` by basename, so the old
path was force-tracked; `AGENTS.md` is not ignored and tracks naturally, dropping
the hack. The three inbound wikilinks (`index.md`, `map/core.md`, `map/repl.md`)
were repointed from `[[CLAUDE|…]]` to `[[AGENTS|…]]`. No CLAUDE.md stub was left —
keeping one would re-introduce the force-track and the harness dependency the move
removes.

## [2026-05-31] migrate | Navigation polish — start-here path, design→internals back-links

Two discoverability touches, no new content.

- `index.md` gained a **Start here** section above the catalog: a guided
  why-before-how path (cbpv → types → the ladder → the machine → grant →
  capability enforcement → exarch-architecture → a turn end to end).
- Each `design/` page with a corresponding `internals/` mechanism page now closes
  with a **Realised in** link, so the durable *why* points forward to the
  operational *how*: cbpv → compilation-ladder + evaluator-machine, types &
  row-types → type-inference, grant → capability-enforcement, pipelines →
  pipeline-execution, effects-handlers → handler-dispatch, scoping →
  evaluator-machine, exarch-architecture → a-turn-end-to-end. The reverse links
  (internals → design) already existed; this closes the loop.

## [2026-05-31] ingest | Capability and pipeline mechanism pages; stamp reconciliation

Two more `internals/` mechanism pages, source-verified against `@80744ab`, plus a
lint pass.

- [[internals/capability-enforcement]] — how a [[design/grant|grant]] check runs:
  the single `EffectiveGrant` chokepoint, the `Meet` fold over the `GrantStack`
  (omit = inherit, present = narrow, deny anti-monotonic), the four-stage
  canonicalise-then-match path rule, in-process exec gating (`check_exec_args`
  before spawn) vs the OS sandbox (Seatbelt / bwrap / AppContainer) for fs/net,
  and `run_confined` re-execing the pinned current binary with a
  `SandboxProjection` over IPC.
- [[internals/pipeline-execution]] — how each pipeline model is wired:
  `eval_pipeline` → `run_pipeline`, value edges as in-process folds, byte edges as
  one pgid-anchored process group (`PipelineGroup`, `spawn_with_pgid`), the
  foreground exec trampoline that wins the `tcsetpgrp` race, Windows Job Objects,
  and out-of-process stages as subshells.

Lint: ran the mechanical map-drift check — `git log <stamp>..HEAD` over every
map page's `covers_paths` returned **zero** source changes since `@c6b3da5`
(core) and `@ce51d4e` (repl/exarch), so the map layer is not drifted. Bumped the
seven prior `internals/` pages' `verified_at_commit` from `@8cd5879` / `@1f67423`
to `@80744ab` for a consistent provenance stamp (no source moved, so the anchors
trivially still hold). `index.md` Internals section now nine pages.

## [2026-05-31] ingest | Four more internals pages — front end, inference, builtins, handlers

Thickened `internals/` with four mechanism pages, source-verified against
`@1f67423`. First refined the layer's scope rule in the contract: it admitted
only cross-subsystem *flows*, but an algorithm walkthrough (inference, handler
dispatch) is equally "how it runs" and lives in neither `map/` (where) nor
`design/` (why). The rule now reads "one page per *flow or mechanism* … never a
file-by-file restatement of a single `map/` page."

- [[internals/surface-syntax]] — the front end: context-free lexing (no
  POSIX-style mode switching), recursive-descent parsing with a Pratt core for
  `$[…]`, the flat AST, and single-point head `classify`ication shared by checker
  and runtime. Consolidates the lexer and parser into one flow rather than two
  thin subsystem pages.
- [[internals/type-inference]] — the HM algorithm: the Inferencer walking the
  typed IR, the Unifier solving value/comp types (equi-recursive, no occurs
  check), rows (Rémy rewrite) and byte modes, and SCC-driven generalisation at
  `Bind`. Distinct from [[design/types]], which states the system.
- [[internals/builtins-registry]] — the six-facet `builtin_registry!` macro that
  keeps a builtin's arity/type/body from drifting, registration-as-seeding vs
  evaluator dispatch, bundled coreutils via `declare_coreutils!`, and host-layer
  builtins above core.
- [[internals/handler-dispatch]] — the runtime realisation of deep self-masking
  handlers: `HandlerStack::lookup`'s two innermost-first passes, the
  `strip_matched` / `restore_matched` single-frame lift that makes wrap-and-forward
  terminate, and why deep falls out of the stack living on the dynamic context.
  Distinct from [[design/effects-handlers]], which states the discipline.

`index.md` Internals section extended to seven pages.

## [2026-05-31] migrate | Added the internals/ layer — how it runs

The wiki answered *why* (`design/`) and *where* (`map/`, a thin catalog) but
never *how it runs*: a reader could not reconstruct the lifecycle of an input or
the evaluator as a machine. Added a fourth layer, `internals/` — a small curated
set of operational narratives that thread several subsystems into one flow. It is
*semi-durable*: stamped with `verified_at_commit` + `anchors` (load-bearing
symbols/invariants) rather than map's `covers_paths`, and its lint is semantic
(do the anchors still hold, has a decision superseded it) rather than mechanical
(did files change). The maintainer contract gained the layer description, the
four-layers count, the internals-drift lint rule, and an ingest step. Inclusion
test: file/symbol content → `map/`, argues a choice → `design/`, narrates runtime
flow → here.

Three pages, source-verified against `@8cd5879`:

- [[internals/compilation-ladder]] — source to typed IR down the fixed ladder;
  `compile` / `compile_and_typecheck`, the postcard prelude bake.
- [[internals/evaluator-machine]] — the trampolined CBPV machine, the
  `Mobile`/`Local` Shell split, the `pub(crate)` `Tail`/`Control`/`Raw` discipline
  that makes a tail call unable to cross a public boundary, and the
  `Break`/`Escape` exit channels.
- [[internals/a-turn-end-to-end]] — one top-level turn for a ral REPL line and an
  exarch tool call, and the shared `eval_top_level` + `Mobile`-install spine that
  is why exarch needs no runtime of its own.

`index.md` gained an Internals section.

## [2026-05-31] ingest | Three durable design pages closing coverage gaps

A lint of design coverage found three central, durable concepts living only in
the volatile `map/` layer. Promoted each to a `design/` page, written terse and
cross-linked into the existing graph:

- [[design/types]] — the Hindley–Milner type system: value vs computation types,
  the byte I/O modes on `F[I,O] A` (the "pipeline modes" previously described
  only in [[map/core/typecheck]]), and SCC-driven let-generalisation. Grounded in
  `docs/SPEC.md` §20.
- [[design/pipelines]] — the two execution models (value folds vs byte-process
  pipelines) selected by the connecting edge's type, the `|`/`?` separation, and
  process-stage isolation. Grounded in RATIONALE §"Byte pipelines are processes;
  value pipelines are folds" and `docs/SPEC.md` §4, §13, §20.4.
- [[design/exarch-architecture]] — exarch's thesis as a cross-cutting concept:
  an agent as a provider loop over one `shell` tool, each turn a grant-framed ral
  top-level turn against a persistent `Shell`. Ties the exarch maps back to
  [[design/grant]], [[design/cbpv]], [[design/audit]].

Also resolved a contract inconsistency: `CLAUDE.md` referenced a `concepts/`
folder that never existed. `design/` is the documented home for cross-cutting
concept pages, so the stray reference was struck and the Query section now says
so. `index.md` updated with all three new pages.

## [2026-05-30] migrate | Path wikilinks given display aliases

Every path-style wikilink (`[[a/b/c]]`) was reading-view-noisy in Obsidian,
rendering the whole slash-path as link text. Aliased all 172 of them across the
durable, decision, and map layers to their clean leaf name —
`[[design/cbpv|cbpv]]`, and for dated decisions the prefix stripped,
`[[decisions/260514_completion-escape-refactor|completion-escape-refactor]]`.
Link targets are unchanged; only the displayed text is cleaner. `log.md`'s own
historical entries were left untouched to honour the append-only rule. The
maintainer contract's Linking section now mandates the alias for path links.

## [2026-05-31] lint | Map coverage gaps closed — transport, diagnostics, ral-sh

A coverage lint compared every `core/` source file against the map's
`covers_paths`. The durable, decision, internals, and invariant layers were
complete and the map showed no git drift, but ~2,100 lines of `core/` sat under
no map page — including the entire diagnostics subsystem and the cross-process
wire protocol the internals pages narrate only at a high level. Added three map
pages:

- [[map/core/transport|transport]] — `serial.rs` (`SerialValue` / `InternCtx`
  serde mirror of `Value`/`Env`), `subprocess.rs` (the `WireMobile` envelope
  mirroring the `Mobile` bundle), and `subprocess_codec.rs` (the shared
  length-prefixed JSON framing). This is the mechanism behind the Mobile/Local
  split in [[internals/evaluator-machine|the evaluator machine]] and the confined
  evaluation in [[internals/capability-enforcement|capability enforcement]].
- [[map/core/diagnostics|diagnostics]] — `source.rs` (`SourceDb` / `Span`,
  byte-range locations resolved at render time), `diagnostic.rs` (the ariadne
  rendering entry points), `ansi.rs` (color gating), `exit_hints.rs`.
- [[map/ral-sh|ral-sh]] — the POSIX-bridge login-shell dispatcher, the one piece
  outside the runtime ([[invariants/single-binary|single-binary]] §21.1).

`map/core.md` hub and `index.md` updated with all three; the new pages are
stamped `@c164cff`.

## [2026-05-31] ingest | Resolved env_overrides / scope overlap — environment is dynamic-only

Resolved the open thread [[decisions/260530_env-overrides-scope-overlap|env-overrides-scope-overlap]].

The startup pass seeded `HOME`/`USER`/`PATH`/… into **both** the lexical scope
(`Env`) and the dynamic `env_overrides` (`EnvVars`). Only `env_overrides` is
updated by `within [env: …]`, read by `apply_env` / `~` / `principal` / PATH
resolution, flows through `inherit_from`, and serialises across the re-exec —
the lexical `Env` does none of these, so it could never carry the environment
to a child. The scope copy was a read-side convenience for a bare `$HOME`, and
the two views drifted: `within [env: [HOME: '/x']] { echo $HOME }` printed the
seeded value while `~` and child processes saw `/x`.

`docs/SPEC.md` already commits to `$env[KEY]` as the read path (`~` is
`$env[HOME]`, command resolution falls back to `$env[PATH]`), so the lexical
copy was the anomaly, not the contract. Resolution recorded in
[[decisions/260531_env-is-dynamic-only|env-is-dynamic-only]] (*active*):

- Dropped the `scope.set(…)` half of `seed_default_env_vars`
  (`core/src/types/shell/init.rs`); `env_overrides` is now the sole home.
- A bare `$HOME` is now an undefined variable; the environment is reached
  through `$env[KEY]`. Verified: `within [env: [HOME: '/x']]` is observed
  identically by `$env[HOME]` and `~`.
- Fixed a stale `within [shell: …]` doc comment (`scope.rs`, the keyword is
  `env:`) and the one `ral.md` example using bare `$PATH`.

`design/scoping.md` gained a clause stating env vars are dynamic, read via
`$env[KEY]`; `map/core/shell-state.md` re-stamped and its overlap note dropped;
`index.md` updated. Whole workspace test suite green.

## [2026-05-31] query | Filed exarch's hash-addressed editing model

A study of how exarch mutates files — and how its line-identity hash is sized —
filed as durable design plus the missing map coverage.

- **design/**: [[design/hash-addressed-editing|hash-addressed-editing]] — a line
  is named by a Blake3 content hash of its own text (trailing-trimmed, 24 bits).
  The hash *is* the address: stateless (recomputed from live bytes, no remembered
  snapshot, so no recovery step), ambiguity rejected not resolved (zero/several
  matches both error back to the model), whitespace-insensitive in identity but
  ending-preserving on write. Sizing is a birthday bound over a file's distinct
  lines, not a one-in-N margin — 24 bits keep a targeted collision near 10⁻⁴ for
  a several-thousand-line file and degrade safely to the ambiguity error; the
  cryptographic primitive's collision-resistance is unexercised (non-adversarial
  inputs), bought only for uniform truncation at negligible cost.
- **map/**: [[map/exarch/builtins|builtins]] (@11d8a43) — the previously unmapped
  `agent_builtins.rs` atoms (`hash-lines`, `hash-replace`, `grep-files`,
  `explore-dir`) and the `agent.ral` edit helpers (`hash-view`, `view-around`,
  `file-replace-hash-line`, `_surface`). `map/exarch/tools.md` gained a pointer
  noting editing is host atoms, not a tool.
- Cross-linked from [[design/exarch-architecture|exarch-architecture]] and the
  [[map/exarch|exarch]] hub; `index.md` updated under both Design and Map.

## [2026-06-01] ingest | A turn always ends ready — the session-wedge fix

A transport failure mid-turn (web-stream drop, three retries spent) left the
session wedged: `apply` commits the prompt before the request, so an error
between commit and reply stranded the state machine in `AwaitingAssistantAfterUser`
with no reply recorded, and every later prompt hit `append_user`'s `ReadyForUser`
guard and was rejected until `/clear`. Only the cancel path had recovery. Fixed
by enforcing the invariant the driver already assumed.

- **invariants/**: [[invariants/turn-ends-ready|turn-ends-ready]] — `run_turn`
  returns the session `ReadyForUser` however the turn ends; every exit passes
  through one `quiesce` point, guarded by a `debug_assert` post-condition. The
  cancel-only `quiesce_after_cancel` generalised to `quiesce(QuiesceReason)`
  (Cancelled | Aborted, selecting the synthetic stub text); the boundary is named
  `is_ready`, which `can_compact` now reads.
- **map/**: [[map/exarch/session|session]] (@2e0942d) — `run_turn`'s single-exit
  ready-invariant; [[map/exarch/frontend|frontend]] (@2e0942d) — `SessionLog`'s
  `is_ready` / `quiesce`.
- **internals/**: [[internals/a-turn-end-to-end|a-turn-end-to-end]] re-verified at
  @2e0942d; the exarch round-trip now notes the always-ready exit, mirroring the
  ral turn's resume point.
- `index.md` updated. exarch suite green — 119 tests, including the
  `quiesce_after_abort_admits_next_prompt` regression. genai was not at fault: it
  reported the dropped connection correctly; recovery was always the session's.

## [2026-06-01] ingest | Reduced-authority witness; B6 stop; axis-independence note

Capability layer: `EffectiveGrant` is now an unreduced borrow whose sole
operation is `reduce`, yielding `Reduced` — the meet-fold witness every
authority decision (exec, fs, editor, shell, `SandboxProjection`) hangs off.
The stack-walk primitives in `check.rs`/`exec.rs` take `&Reduced`, never a bare
`&Context`, so the single-front-door chokepoint is a visibility fact. Scope:
items 1+2; the in-ral exec/fs fold stays per-layer (three-valued exec, per-layer
short-circuit, the sandbox-vs-host resolver distinction the projection lacks).
Sandbox suite and `grant_policy.rs` pass unchanged — enforcement is identical.

B6 (PATH-source consistency in `policy_names`) left undone: the host baseline
*is* the spoof guard — switching it to the effective PATH makes
`baseline == self.resolved` by construction, so a `within [shell: PATH]` redirect
would re-admit a spoofed binary under a bare-name grant key
(`policy_names_drop_bare_when_scoped_path_diverges` fails). Weakening enforcement
is forbidden, so the change was reverted; the real inconsistency
(`resolve_allow_names` uses `get` not `get_or_host`) fails closed and is recorded
as a follow-up.

B7 (no code): `docs/SPEC.md` §11.3/§11.7/§11.8 and `design/grant.md` now state the
grant axes are independent (`None` = inherit = ⊤, the `Meet` identity), that
restricting one axis does not restrict another, and that `net` has no in-process
gate — it is OS-sandbox-enforced only. Verified the exarch baked profiles
(`reasonable`, `read-only`, `minimal`, `confined`) each set `net` explicitly and
are unaffected by the refactor.

Wiki: `decisions/260601_reduced-authority-witness.md` added; `map/core/capabilities`
and `internals/capability-enforcement` re-stamped (added the `Reduced` anchor);
`index.md` updated. `ral-core` builds and tests green; clippy clean on `ral-core`.

## [2026-06-01] ingest | One XDG resolver; exhaustive serial walks

Two independent hardenings under `core/src/path/` and `core/src/serial.rs`.

XDG base directories were resolved by two divergent rules: the `xdg:` grant
sigil through `path::sigil::resolve_xdg` (override honoured only when absolute
per the spec; home-joined Linux defaults everywhere) and the binary's config /
data loaders through `path::config::config_base` / `data_base` (override
verbatim even when relative; process `$HOME`; a `%APPDATA%` tail). A grant and
the rc/history/plugin paths could thus name different directories. Consolidated
onto one resolver, the new `path::basedir` (`XdgKind`, `resolve_xdg`,
`XdgKind::default_suffix`); `sigil.rs` and `config.rs` both defer to it.
Relative-`$XDG_*_HOME`-verbatim and the `%APPDATA%` fallback are dropped;
no `%HOMEDRIVE%%HOMEPATH%` added; `home_dot` (`~/.ralrc`) untouched. XDG-everywhere
incl. macOS. Recorded in
[[decisions/260601_xdg-resolver-consolidation|xdg-resolver-consolidation]].

In `serial.rs`, the handle-sanitiser `value_carries_handle` (over `Value`) and
the dependency collector `collect_scope_deps` (over `SerialValue`) ended in
`_ =>` catch-alls — a future container-shaped variant would compile while being
silently not descended. Both walks now list every variant, so a new variant
fails the build at each walk. Behaviour unchanged.

`map/core/capabilities.md` and `map/core/transport.md` re-stamped; `index.md`
updated. `ral-core` + `ral` build, test, and clippy green (pre-existing clippy
findings in `repl/complete.rs` test code and `plugin_ed_builtins.rs`, both
outside this change, left untouched).

## [2026-06-01] ingest | Pipeline modes consolidated into one lattice + one unify rule

ral had two mode-inference engines (static `typecheck/`, runtime `ty.rs`) that
had drifted: the static checker made `None ↔ Bytes` *succeed* (dead
`ModeMismatch` arm), the runtime correctly rejected it. Consolidated:

- New `core/src/mode.rs` owns `PipeMode` / `ModeVar` (`u32`) / `PipeSpec` (+ the
  four constructors), all `Copy`, serde-derived; the `ModeMismatch` neutral
  error; the `ModeStore` trait; and `unify_mode<S: ModeStore>` carrying the
  equality rule. Both engines import the lattice and call the one function.
- Static `unify.rs` now rejects `None ↔ Bytes` (live T0012). Runtime `ty.rs`
  renamed `Mode → PipeMode`, `CompType → PipeSpec`; its `ModeUnifier`
  implements `ModeStore`. `--no-typecheck` behaviour unchanged.
- Strictness revealed two genuine over-constraints (not SPEC violations), fixed
  by making them mode-polymorphic: the `map`/`each`/`filter`/`fold`/`sort-list-by`
  callback result (`builtins.rs`), and the `try`/`guard`/`audit` thunk body
  (`scope.rs`). The prelude still typechecks; all examples `--check` clean.
- Build-script `cargo:rerun-if-changed` extended to the moved shape files
  (`mode.rs`, `typecheck/ty.rs`, `typecheck/scheme.rs`) in `ral/build.rs` and
  `exarch/build.rs`.

STOPPED on the §4.2.1 value→byte swallow at `infer.rs::apply_piped_value`:
enforcing it rejects `map-lines … | from-lines`, which the runtime accepts,
because the prelude streaming reducers are statically typed value-output
(they wrap the `fold-lines` decoder) while the runtime reclassifies them
byte-output. Recorded as a known gap, swallow left in place — no hack, no
silent re-loosen. Decision:
[[decisions/260601_modes-equality-constrained-shared|modes-equality-constrained-shared]]
(*active*). `map/core` + `map/core/typecheck` + `map/core/evaluator` re-stamped;
`index.md` updated. Whole workspace test suite green; clippy clean for the
touched crate (`ral-core`).

## [2026-06-01] migrate | Streaming-reducer §4.2.1 gap closed

Followed up the gap left open above: the static and runtime engines disagreed on
the *output mode of streaming reducers*, which forced the `apply_piped_value`
swallow. Closed it by making the static output mode honest at its source, no
re-loosening of unification:

- **`fold-lines` is now mode-polymorphic**: `∀α μ. U(α → Str → F[∅,μ] α) → α →
  F[Bytes,μ] α`, via a new `CommandKind::ByteFold` mirrored in both engines
  (`classify.rs`, runtime `ty.rs::fold_sig`, static `builtins.rs`). The
  callback's output mode is the reducer's, so an `echo`-per-line callback lifts
  the stage to `F[Bytes,Bytes]`.
- **A `Seq`'s byte-output is a join over its statements**, not the tail's:
  `seq_byte_output` (runtime) and `lift_seq_output` (static) make it `Bytes` if
  *any* statement emits bytes. Return type and input mode stay the tail's.
- **The `apply_piped_value` swallow is removed** — strict `unify_comp_ty`. The
  pipeline loop discriminates application from channel adjacency via
  `consumes_value_arg` (peers past block-literal thunks), so a value→function
  consumer iterates element-by-element while a value→`from-X`-decoder edge is
  rejected as the §4.2.1 mismatch it is.
- **`if`/`case` branches union their modes** (`union_mode`/`merge_branches`,
  mirroring runtime `infer_branches`): a mode clash between arms yields a fresh
  variable, since only one branch runs. **Parked** for maintainer review — green
  but not ratified.

Fix during verification: the initial gate used `!matches!(next, Return(..))` to
route the value path, which mis-classified a block-literal consumer
(`{ |v| … }` infers as `Return(_, Thunk(Fun))`) as a channel edge and so missed
the `stream_pipeline_rejects_non_recursive_tail` stream probe. Replaced with the
`consumes_value_arg` discriminator. Whole workspace test suite green
(31 binaries, 1202 tests, 0 failures); `cargo build --workspace -D warnings`
clean; `cargo clippy -p ral-core --all-targets -D warnings` clean; prelude bake
and all `examples/*.ral --check` pass. Decision:
[[decisions/260601_modes-equality-constrained-shared|modes-equality-constrained-shared]]
§ "value↔byte structural gap — now closed".

## [2026-06-01] lint | Reconcile map + internals against `main` after the three merges

Post-merge drift pass. The mode-consolidation (Merge A), reduced-authority
(Merge B), and XDG+serial (Merge C) merges, plus ~24 commits of surface-effect
and session-log work, had left several `map/` stamps at pre-merge feature
commits. Ran `git log <stamp>..HEAD -- <covers_paths>` for every map page;
re-ingested and re-stamped the drifted ones to HEAD (`e8227dc`):

- **core / typecheck** — the streaming-reducer §4.2.1 paragraph rewritten: the
  gap is closed (`ByteFold`, `lift_seq_output` / `seq_byte_output` join, strict
  `apply_piped_value` + `consumes_value_arg`), and the `if`/`case` branch-mode
  union recorded as **open** pending maintainer review (not as settled).
- **core / evaluator** — `validate_pipeline`/`pipeline_mismatch` run the runtime
  mode engine over the shared `mode.rs` lattice; `pipeline/helper.rs` builds its
  stage child via `subprocess::reexec_child_shell`.
- **core / capabilities** — surface events carried on the IPC `surface_events`
  response and replayed by the parent sink; child shell via `reexec_child_shell`.
- **core / shell-state** — the `SurfaceSink` on `Local`; `checks.rs` forwarders
  reduce through the `Reduced` witness.
- **core / syntax** — `CommandKind` spans, `ByteFold` for `fold-lines`.
- **core / transport** — `reexec_child_shell` as the one host-builtin-reinstalling
  re-exec constructor.
- **core / builtins** — the `surface` builtin in `misc.rs`.
- **core (hub)** — re-stamped (already named `mode.rs`).
- **exarch (hub / session / shell-eval / frontend / builtins)** — durable session
  logs under `$XDG_STATE_HOME/exarch/<project>/<run>/` (`log_run_dir`,
  `project_slug`; the `exarch-scratch-latest` symlink was dropped, corrected the
  draft); the `surface` host sink replacing the stderr-sentinel narrative
  (`value_to_kind` decodes a tagged variant onto the bus); `_surface` helper gone
  from `agent.ral` (direct `surface` calls); incremental `user.log`.
- **repl/{startup,frontend,plugins}, exarch/{policy,provider,tools}** — clippy /
  doc-link / build.rs / prompt-text drift only; re-stamped, prose unchanged.

Internals re-verified to HEAD: **type-inference** (the `Unifier` shares the
`mode.rs` lattice via `ModeStore`; modes equality-strict; `ModeMismatch`/T0012
live — anchors gained `unify_mode`, `PipeMode`, `PipeSpec`, `ModeStore`) and
**capability-enforcement** (already narrated the `EffectiveGrant`→`Reduced`
witness; anchors confirmed). `compilation-ladder` and `evaluator-machine` were
inspected; their narratives and anchors are unchanged by the merges, so their
stamps stand.

Decision pages: the three `260601_*` pages are accurate and cross-linked; tidied
the modes page's "value↔byte" section out of dated-update/branch-name archeology
into present-tense prose while keeping the parked branch-union note. The two new
`dev/docs` backlog memos (theme D external-stdio three renderings; theme E
resolution-decided-twice / `CommandKind`-missing-type) are linked as known smells
from `map/core/evaluator`, `map/core/syntax`, and `map/core/elaboration`.

Lint: no broken wikilinks, no orphans, no contradictions (verified the
scratch-latest symlink and surface-sentinel narratives are no longer claimed
anywhere). `index.md` map stamps and the shell-eval / modes summaries updated.

## [2026-06-01] ingest | A command's mode is the projection of its type — resolution-decided-twice resolved

`6bfeced` resolves the theme-E smell the reconcile entry above filed as *known*:
the `CommandKind` reverse-engineering and the elaborator/typechecker split that
let a command's runtime mode and its static type drift. A command's boundary
modes are now the modal projection of its declared type, read once and shared by
both engines. `classify.rs` / `CommandKind` are gone; `dev/docs/260601_resolution_decided_twice.md`
deleted (the memo's two design questions are now answered, not parked).

- **core / builtins** — `BuiltinTypeRule` is now `Sig | Scheme | Reducer`. The
  streaming reducer `fold-lines` is named *structurally* as `Reducer` (typed
  exactly as a `Scheme` by the static checker; only the runtime reads its
  reducer mode), replacing the `classify.rs` `"fold-lines"` name-string
  special-case. Re-stamped `6bfeced`.
- **core / typecheck** — `typecheck::builtins::sig_pipe_spec` projects a
  signature's result template onto a `PipeSpec`; `reducer_spec` builds the
  reducer boundary (bytes in, output following the callback). The static checker
  builds its `CompTy` from these and the runtime engine reads them directly, each
  over its own `ModeStore` (`fresh_mode` mints the open modes) — so runtime modes
  for builtins now *equal* their static types. Re-stamped `6bfeced`.
- **core / syntax** — `classify.rs` reduced to the residue no declared signature
  can supply: `PRELUDE_BYTE_FNS` (`map-lines`, `filter-lines`, `each-line`,
  `view`) and `prelude_command_spec`, the shell-free fallback (byte-mode prelude
  export or unknown external → bytes-in/bytes-out; any other prelude export →
  value-mode). Re-stamped `6bfeced`.
- **core / elaboration** — the bound/unbound split's second half: `exec_comp_ty`'s
  richer-precedence arms over an `Exec` head were unreachable (bindings, builtins,
  and handlers resolve as a bound `App` first; a prelude function never reaches
  the checker as a bare `Exec`), so an unbound `Exec` head collapses to
  `external_exec_comp_ty`. Re-stamped `6bfeced`.

Also corrected a stale `CommandKind::Value` reference in `typecheck/scope.rs`'s
control-wrapper note (the conceptual claim — the wrapper returns a value, so its
own computation type stays `F[none,none]` — is unchanged). `index.md` map stamps
for the four pages bumped to `6bfeced`; the hub (`map/core.md`, covers
`lib.rs`) is untouched by the refactor and stands at `e8227dc`.

## [2026-06-01] ingest | CBPV soundness made explicit on three design pages

Three design pages tightened where the *why* was under-stated; each fact verified
against the code:

- **`design/cbpv.md`, `design/types.md`** — captured external output is decoded
  UTF-8 *lossily* (`from_utf8_lossy`, U+FFFD on invalid bytes), so the
  `F[I,Bytes] String` decode at the `let` boundary is total.
- **`design/types.md`** — generalisation soundness stated as two legs: the SCC
  discipline for recursion, and the absence of a value restriction (immutable
  bindings give no polymorphic references; CBPV `Bind` sequences the effect before
  binding a value).
- **`design/effects-handlers.md`** — handlers framed as the tail-resumptive
  fragment of algebraic-effect handlers: the return value is the implicit single,
  tail resumption, so only non-tail/multi-shot `resume` is absent. Forward/fail
  options added per SPEC §3.2.

`AGENTS.md` gained a "lead with the thesis, then bullet the structure" house-style
rule; `index.md` effects one-liner updated.

## [2026-06-01] migrate | Fold the redirect-drop memo into its wiki decision; delete the memo

`dev/docs/260526_redirect_drop_on_handler_resolution.md` (fixed in `270ca13`) is
fully captured by [[decisions/260526_redirect-drop-on-handler-dispatch|redirect-drop-on-handler-dispatch]],
now self-contained with the fix location (`command_call.rs` `Resolution::Handler`
arm) and the regression coverage (`ral/tests/pipeline.rs`). The verbose memo is
deleted and its dangling `Memo:` back-reference removed.

## [2026-06-01] lint | hot-path-cancellation corrected: baseline is in, hot-loop polling is a plan

The decision page claimed the collection builtins poll at their loop tops and a
1024-chunk extend helper exists; neither is in the code. Corrected to the real
state — `process::check` polls only at the evaluator boundaries
(`trampoline`/`comp`/`pipeline`), so callback-driven builtins (`map`/`filter`/`fold`/
`each`/`sort-list-by`) are interruptible *incidentally* while a pure-Rust loop like
`range` is not — and recorded the planned extension (loop-top checks + a chunked
`range` poll; no bulk-clone helper, since `List` is `imbl::Vector` with O(1)
append). Status `active` → `proposed`; `index.md` updated. `dev/docs/260504_hot_path_cancel.md`
stays as the plan's source.

## [2026-06-01] migrate | Fold the grant + hot-path memos into the wiki; delete both

The two `dev/docs` memos behind these pages are now fully captured in the wiki and
removed.

- **grant** — `dev/docs/260504_grant_capability_design.md`. The durable rationale is
  on [[design/grant|grant]]: the 3-valued exec lattice was already there; the
  **concessions** (ambient bare names, TOCTOU on path resolution, the name-layer
  confused deputy) are now folded in. The schema table (covered by SPEC §11), the
  literature mapping, and the now-landed port plan are dropped. The "deep design in
  dev/docs" citation is removed.
- **hot-path** — `dev/docs/260504_hot_path_cancel.md`.
  [[decisions/260504_hot-path-cancellation|hot-path-cancellation]] is now
  self-contained (plan, non-goals, verification); the "Deep notes" reference is
  dropped.

## [2026-06-01] lint | De-duplicate the lexical-ownership model onto audit

The lexical-ownership model ("each scope node owns its body's audit nodes; processes
only transport fragments") was stated in full on both [[design/audit|audit]] and
[[design/control-operators|control-operators]]. Made [[design/audit|audit]] the single
home — it is the page about the audit tree — and thinned control-operators' paragraph to
a one-line pointer. control-operators still justifies `audit` as an operator via its
criteria list; it no longer re-explains the tree.

## [2026-06-01] query | pipelines.md no longer leads with `?`

The opening sentence led with `?` ("`|` moves data; `?` reacts to failure") — a contrast
against an operator this page never develops and that has no home page of its own. Rewrote
the opener to lead with the actual thesis (the connecting edge's type selects fold vs
process) and demoted the failure-orthogonality note to a one-line aside pointing at
[[design/control-operators|control-operators]]. Corrected the aside: a pipeline *propagates*
a stage's failure (SPEC §10) — what `|` does not do is *react*; reaction is `?`/`try`.
Open follow-up: a dedicated failure-model page (`?`, `try`, success≠truth, no command-level
`||`) reflecting RATIONALE §"Piping and failure" — pending confirmation of `?`'s exact
chain semantics (SPEC §10 "first success wins" vs the failure-propagation test read).

## [2026-06-01] ingest | new design/failure.md; `?` semantics resolved

Resolves the open follow-up from the prior entry. The SPEC-vs-test tension was not
real: SPEC §10 ("first success wins") and §4.4 ("a `false` predicate is still a
successful command") are both correct. Traced the code — `eval_chain`
(`core/src/evaluator/comp.rs`) returns on the first `Ok` and advances only on
`Break::Error`; `eval_return`/`set_status_from_bool` write `$status` from a `Bool` but
return `Ok`, so a `Bool` never raises a failure and never drives `?`. The wrong artifact
was the comment in `tests/lang/failure-propagation.ral` (`# Expected: chain continued`),
which nothing asserts — no harness runs `tests/lang/*.ral`. Corrected that comment and
strengthened the block to exercise real fallthrough (failing arm → next runs) and all-fail
propagation, verified against the interpreter.

Filed the durable answer as [[design/failure|failure]] — "failure is status, not truth":
the `?` fallback chain, `try` as recovery and the only `||`, the `Bool`-is-data principle,
and how failure crosses sequences/pipelines/for/spawn/top-level. Reflects RATIONALE
§"Piping and failure" + §"No command-level `||`", which had no wiki page. Relinked
[[design/pipelines|pipelines]]'s demoted aside and [[design/control-operators|control-operators]]
(which argues *why* `?`/`try` are syntax but defers what they *do*) to it; added to index.

## [2026-06-02] lint | readability sweep — index orientation, decisions one-liners, two dense openers

Acted on the note that the wiki read as muddled. The page-level content held up: every
`design/`, `internals/`, `invariants/`, `decisions/`, and `map/` opener was checked and
nearly all already lead with a clean thesis. The muddle was concentrated, so the fixes are
surgical rather than a rewrite.

`index.md`, three changes. (1) Added a **How this is laid out** block — one line per layer
(`design` why, `internals` how, `invariants` rules, `decisions` why-changed, `map` where) so
a first-time reader is oriented before the catalog instead of inferring the layer model from
`AGENTS.md`. (2) **Decisions list rewritten to genuine one-liners.** The summaries had drifted
into multi-clause changelog entries (e.g. `reduced-authority-witness` carried three claims plus
parentheticals); each is now the page's own H1 thesis, with status moved to a leading `*badge*`
so the list scans by status at a glance. Full reasoning stays on the page, restoring the
AGENTS "one-line summary" contract. (3) Section headers simplified to `folder/ — gloss`,
dropping the maintainer-facing lint jargon ("anchor-stamped", "git-drift-stamped") that was
navigation noise; the orientation block now carries that meaning.

Two prose openers on the Start-here path de-densified, both by splitting a sentence whose verb
sat behind a stacked aside, no claim lost. [[design/cbpv|cbpv]]: the "shell collapse" sentence
now leads with the bolded rule (*captured data is never re-lexed, split, or globbed*) and
states the consequence after. [[design/types|types]]: the external-command sentence had an
em-dash aside nested inside another; split into the shape claim and the decoding claim.

No files moved or renamed and no links changed — the five-layer structure is sound; it was its
legibility on the index, not its shape, that needed work.

## [2026-06-02] lint | readability sweep — prose walls broken into bullets

A maintainer read of the vault found most pages still hard to read: a clear bolded thesis per
section, but dense prose underneath, including enumerations written inline as "A by x, B by y,
C by z". Swept all five layers into the house "thesis + bullets" register — one precise claim
per bullet, 1–2 sentences of connective prose kept.

Mechanics: triaged objectively by longest unbroken prose run per page, then fanned the work
across the corpus. 41 pages restructured (e.g. [[design/hash-addressed-editing|hash-addressed-editing]]
from a 58-line prose wall; [[internals/capability-enforcement|capability-enforcement]]'s decision
methods, `Meet` rules, path stages, and exec/fs split; [[map/core/shell-state|shell-state]]'s
Mobile/Local split). Pages already in house style (e.g. [[map/core|core]], [[map/exarch|exarch]],
[[internals/compilation-ladder|compilation-ladder]]) were left untouched.

Presentation only: no frontmatter or stamp changed (this is not a re-ingest/re-verify), every
`[[wikilink]]` preserved, no technical claim or idea order altered. Sustained single-argument
paragraphs (e.g. [[internals/handler-dispatch|handler-dispatch]], [[internals/pipeline-execution|pipeline-execution]])
were deliberately kept as prose rather than fragmented into bullets.

## [2026-06-02] lint | capability-enforcement: corrected the in-process-vs-OS split

[[internals/capability-enforcement|capability-enforcement]]'s thesis line drew a false binary —
"exec is gated in-process; fs and net are enforced by the OS." It contradicted both the page's own
`check_fs_read`/`check_fs_write` list and the code: `fs` *is* gated in-process on every platform, and
on macOS `exec` is *also* enforced by the OS (`project()` sets `exec_triggers_sandbox` and the Seatbelt
profile renders a `process-exec` allow-list — `core/src/capability/effective.rs:271`,
`core/src/sandbox/macos.rs:235`), matching SPEC §11.8. The companion bullet ("every sandbox backend passes
exec through") carried the same error.

Re-cut to the true axis — **what ral dispatches (in-process gate) vs. what a spawned child does (OS
sandbox)** — as three per-dimension bullets: exec gated in-process everywhere plus an OS `process-exec`
allow-list on macOS; fs gated in-process and OS-backed; net OS-only because ral dispatches no network
operation. Fixed the matching `index.md` summary. Re-stamped `verified_at_commit` 6aa517a and added the
`check_fs_write` anchor (the corrected thesis now leans on the in-process fs gate).

## [2026-06-02] query | why does ral gate in-process rather than leaving it all to the sandbox?

Filed the answer as [[design/two-enforcers|two-enforcers]] — the durable *why* the `internals/` page
(which narrates the *how*) does not argue. Thesis: the two enforcers are not redundant; each is
authoritative exactly where the other is blind, so "all sandbox" is strictly weaker. Grounded the
argument in the code: `check_exec_args_impl` matches the runtime argv for the Subcommands lattice
(`core/src/capability/check.rs:49`), so `exec: [git: [status]]` is in-process-only — the OS layer
renders a path allow/deny list (`reduce_exec`, `effective.rs:300`) that gates which binary, never which
subcommand. Other pillars: the sandbox never sees ral's own in-process operations (builtins are not
subprocesses); it is uneven across platforms (the [[decisions/260530_linux-exec-confinement|linux-exec-confinement]]
gap, coarse Windows container, unscopable `net`); its denials are opaque `EPERM`/`SIGKILL` vs. a
structured [[design/audit|audit]] escape. Cross-linked from [[design/grant|grant]] and `index.md`.

## [2026-06-02] ingest | capability meet-fold unified through the `Meet` trait (b19a151)

Landed refactor of `core/src/capability/`: the grant lattice's meet was hand-inlined as
`acc = Some(match acc { Some(p) => combine(p, x), None => x })` in the reducers, re-implementing the
existing `impl<T: Meet> Meet for Option<T>`. Introduced `GrantPathSet` (a `Vec<GrantPath>` newtype) whose
`Meet` impl *is* the symlink-aware prefix-set intersection (formerly the free fn `intersect_grant_paths`),
and routed the read/write-prefix, subpath-allow, and exec-policy folds through `Option::meet`. The literal
exec map keeps its free-fn meet (`meet_literal_exec`) — `types/capability.rs` deliberately keeps map-level
meets out of the trait, so that one is left as-is by design. Lesser smells fixed in the same pass:
`resolve_allow_names`/`resolve_deny_names` unified into one `resolve_exec_names` (keep-predicate +
`path::is_absolute`, B6 no-host-fallback preserved); redundant second `trim_end_matches('/')` on exec dir
raws dropped. Pure refactor, 353 ral-core tests pass + new `GrantPathSet` lattice-law tests, clippy clean.

Wiki: re-stamped [[map/core/capabilities|map/core/capabilities]] to b19a151 and named `GrantPathSet`;
refreshed the now-renamed symbol pointers in [[decisions/260601_reduced-authority-witness|reduced-authority-witness]]
(B5 `intersect_grant_paths`→`GrantPathSet`'s `Meet`; B6 `resolve_allow_names`/`resolve_deny_names`→`resolve_exec_names`).
The B6 fail-closed property it documents is unchanged.

## [2026-06-02] lint | "exec is not sandbox-enforced" — the same error the internals fix missed

Re-audit of `core/src/capability/` + `core/src/sandbox/` against the wiki. The code is clean and matches
the `Reduced`-witness design; the only finding was residual doc drift the earlier
[[internals/capability-enforcement|capability-enforcement]] correction did not reach. [[map/core/capabilities|map/core/capabilities]]
still asserted "Exec is *not* sandbox-enforced: every backend passes exec through", contradicting both the
internals page and `core/src/sandbox/macos.rs` (`emit_exec_rules` renders `(allow file-read* process-exec …)`
when `ExecProjection::Restricted`). Re-cut the OS-sandbox paragraph to the per-platform truth — in-process
`Reduced::check_exec_args` everywhere, plus a macOS Seatbelt `process-exec` allow-list, bwrap standing alone
on Linux.

Same correction applied to the source comments that carried the stale claim or pre-`Reduced` symbol names:
`core/src/sandbox.rs` (module doc: exec enforcement + `EffectiveGrant::check_exec_args`→`Reduced::check_exec_args`),
`core/src/types/capability.rs` (`SandboxProjection`: `fs+net`→`fs+net+exec`, producer is `Reduced::sandbox_projection`),
`core/src/capability/effective.rs` (`sandbox_projection` returns `None` unless fs/net — or, on macOS, exec — restricts).
Docs-only; no enforcement behaviour changed.

## [2026-06-02] ingest | fix: directory `Deny` lost (and flipped to `Allow`) through `ExecMap` meet/join

`ExecMap::meet`/`join` combined the `dirs` halves by their bare key sets, ignoring each key's
`ExecDir` verdict and re-labelling every survivor `Allow`. So `meet` turned a `dir: Deny` that
prefix-overlapped the other side into `Allow` (a privilege-escalation inversion — meet widened
authority) and dropped a non-overlapping one-sided deny. Latent because no shipping base profile uses a
directory `Deny`, and the per-layer matcher / projection always honoured it; the gap was only in
`RawCapabilities::meet`/`join` composition (exarch's `base.join(extend).meet(restrict)`). Fixed in
`core/src/types/capability.rs`: dirs now split by sign and follow the `ExecDir` lattice like the literal
half — meet intersects allows / unions denies (sticky, deny wins clashes); join unions allows /
intersects denies (allow wins). Four regression tests added (`meet_exec_dirs_deny_beats_allow`, …).
Updated the [[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]] page from
"known issue, deferred" to the corrected semantics. ral-core 356 lib + grant_policy 10 +
sandbox_fail_closed 6 + exarch 121; clippy clean.

## [2026-06-02] ingest | capability simplification: partitioned exec, tighter verdict, shared meet (db266b7..a28f980)

A four-commit, enforcement-preserving simplification pass over the capability layer, filed as
[[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]]:

- **db266b7** — collapsed the editor/shell bool-gate `effective.rs → check.rs → effective.rs` round trip
  (deleted two adapters; narrowed `check_grant_bool` to private).
- **6732c32** — gave exec authority its missing partitioned type: `Option<ExecMap>` with
  `literals: BTreeMap<String, ExecPolicy>` and `dirs: BTreeMap<String, ExecDir>` (two-valued). Makes
  `Subcommands`-on-a-directory unrepresentable, drops the trailing-`/` tag (dirs stored slash-free —
  safe because `path_within` is component-wise), and deletes `split_exec_map`/`partition_exec`/
  `is_subpath_key`. Also fixed a prompt-faithfulness slip (denied dirs no longer render as admittances).
- **83ca4fa** — tightened the admitted verdict to `Admit { Any, Subcommands }`, removing the
  "unreachable" `Allowed(Deny)` arm.
- **a28f980** — shared the prefix-set meet as `crate::path::meet_prefix_sets_by`, completing the
  follow-up [[decisions/260601_reduced-authority-witness|reduced-authority-witness]] §B5 named.

Settled in the negative: **full unification of the in-process check fold with the OS projection fold.**
A subject-free intermediate cannot preserve the argv-level three-valued verdict or the process-split,
per-access, mode-dependent resolver without re-embedding both representations — duct tape that would also
fail the enforcement guard. Recorded in the decision page. Also flagged a preserved latent quirk: dir
`Deny` is discarded through `ExecMap::meet`/`join` (cross-map composition only; per-layer and projection
honour it). Re-stamped [[map/core/capabilities|map/core/capabilities]] to a28f980 and named `ExecMap`/
`ExecDir`/`Admit`/`meet_prefix_sets_by`. Verified: ral-core 352 lib + grant_policy 10 + sandbox_fail_closed
6 + nested-dispatch 2 + ral capabilities 4 + exarch 121; clippy clean.

## [2026-06-02] query | builtins gap: a design/why page, and a map fix

"The wiki doesn't have enough about builtins." It had the *mechanism*
([[internals/builtins-registry|builtins-registry]]) and the *where*
([[map/core/builtins|map]]), but no *why* — every other subsystem has a
`design/` page and builtins did not. Read the full `builtin_registry!` (47
core entries), `codecs.rs`, `caps.rs`, and SPEC §16/§17/§21 + RATIONALE to scope
the gap, then (with the user picking scope) wrote [[design/builtins|design/builtins]]:
the layering of head names (control operators · core builtins · `_`-primitives ·
prelude · bundled coreutils · host builtins · externals), the placement principle
(effects→coreutils with **no** `copy-file`/`make-dir` primitives, structured
queries→Rust builtins as a syscall bridge, conveniences→prelude by the same
HM-derivability test that keeps `for`/`retry` out of the grammar), and the
`from-X`/`to-X` codec family as the one place the language names the
[[design/cbpv|byte-mode]] boundary.

Also fixed a map inaccuracy: [[map/core/builtins|map/core/builtins]] listed
`caps.rs` under "bodies grouped by concern — capability introspection," but no
registry entry calls into it — `caps::decode_capability_map` is consumed by the
`grant` operator (`evaluator/scope.rs`) and the `--capabilities` ceiling
(`capability/load.rs`). Corrected the prose, added `shell.rs`'s `cd`/`alias`
inventory, re-stamped 6bfeced→047708b. Updated `index.md`.

## [2026-06-02] ingest | codecs page; fix dead `_fs`/`_path` drift in SPEC + RATIONALE

Follow-up to the builtins design page. Two parts.

**Drift fix.** `_fs` / `_path` are gone from the code (no hits anywhere in
`core/`), but two normative docs still named them: RATIONALE §"Syscall bridge,
not text parsing" listed `_fs lines/size/mtime/empty/list` and `_path …`, and
SPEC §11.2 named "the `_fs` query ops". Rewrote both to the live builtins —
`list-dir`, `file-info`, the `is-*` predicates, `resolve-path`, `glob` — keeping
the principle (structured queries are a syscall bridge, not a stat/ls text
re-parse) intact.

**New page [[design/codecs|design/codecs]].** Per the user's steer, the spine is
the *byte I/O mode in typing*: `mode.rs` names four `PipeSpec` shapes —
`none` `F[∅,∅]`, `ext` `F[Bytes,Bytes]`, `decode` `F[Bytes,∅]`, `encode`
`F[∅,Bytes]` — and `from-X` *is* `decode`, `to-X` *is* `encode`. The codecs are
exactly the builtins with a byte mode on one side and `∅` on the other, so the
[[design/pipelines|pipe]]'s equal-mode rule lets a value↔byte crossing happen
only by naming a codec, never by accident. Also: whole-buffer decoders vs. the
streaming `from-lines` (lazy `Step`) / `fold-lines` (output mode parametric in
the callback, `F[Bytes, μ] α`), and the strict-structured / lossy-line-stream
contrast (the latter total, like external-command capture). Linked from
[[design/builtins|design/builtins]] and [[map/core/builtins|map/core/builtins]];
added to `index.md`.

## [2026-06-03] migrate | split design/builtins → name-resolution + a real builtins page

The user flagged that `design/builtins` "talks about everything BUT builtins —
resolution, handlers, blahblah." True: its title was *"where a capability
lives,"* its body was the seven-layer head-name resolution + the cross-layer
placement principle (builtins one bullet among seven), and its codecs section
duplicated [[design/codecs|design/codecs]] (written the next day). It was a
name-resolution page wearing a builtins title.

`git mv`'d it to [[design/name-resolution|design/name-resolution]] — refocused
on the layering and the placement triad (effects→coreutils, queries→builtins,
conveniences→prelude, ambient→control operator), codecs section cut to a
one-line pointer.

Wrote a fresh [[design/builtins|design/builtins]] that is actually about the
primitive set: the three reasons a capability earns a Rust body (a syscall /
structured query, the shell's runtime state, a primitive HM can't derive in the
prelude); the families across the ~47 `CORE_BUILTINS` entries; the three typing
rules (`Scheme` first-class polytype · `Sig` command signature for variadic /
optional / divergent / probe shapes · `Reducer` for `fold-lines`, a `Scheme` to
the checker but flagged for the runtime mode engine); and reification — `$name`
η-expands to a *name-dispatched* command so handlers/aliases still intercept.

Fixed inbound links: `index.md` (one entry → two), [[map/core/builtins|map/core/builtins]]
(placement → name-resolution, added the "what a builtin is" → builtins link),
and [[internals/builtins-registry|internals/builtins-registry]] (added its
missing up-link to `design/`). `design/codecs`'s two links resolve unchanged.

## [2026-06-03] ingest | design/syscalls-are-effects — the foundational principle

Filed a new durable design page elevating ral's organising principle: **a system
call is an algebraic effect.** An external command is an *operation*; its
*interpretation* — by default the OS performing the syscall — is supplied
separately. The page maps how this ripples: the value language is pure, authority
is permission over the effect set (`grant`), the capability check sits at the
effect-performance site ([[design/two-enforcers|two-enforcers]]), dynamic scope is
for authority, failure is an operation's exceptional outcome, audit is a trace of
operations, exarch's sandbox is authority over effects.

The user corrected an early draft that folded handlers into the definition:
**handlers are orthogonal.** The layering is now explicit along two independent
axes — (1) whether to install a handler to reinterpret an operation, (2) whether
that handler reifies a continuation. ral takes the minimal setting on both: the
tail-resumptive fragment, no first-class `resume`, deliberately *less* than
algebraic-effect handlers usually offer. The principle holds *period*,
independent of either choice. [[design/effects-handlers|effects-handlers]]
reframed to open as that orthogonal layer.

Rewrote `README.md` from this understanding: the old "Algebraic effects" bullet
conflated "external commands are operations" (the principle) with "`within`
installs handlers" (the orthogonal layer). Split into a "System calls are
algebraic effects" lead bullet and a separate "Handlers are an orthogonal layer"
bullet, with `dir:` / `env:` / capability scoping moved to an "Authority is
dynamic" bullet. Inbound `See also` links added from
[[design/cbpv|cbpv]], [[design/name-resolution|name-resolution]],
[[design/grant|grant]], [[design/scoping|scoping]], [[design/failure|failure]],
[[design/two-enforcers|two-enforcers]], [[design/builtins|builtins]],
[[design/audit|audit]]; added to `index.md` (head of the design/ list and the
start-here path). Flagged for follow-up: `docs/RATIONALE.md` §"Effect handlers:
deep with self-masking" leads with the same conflation and likely warrants the
same split.

## [2026-06-03] ingest | RATIONALE: principle section added, handlers demoted

Did the follow-up flagged above. Added `docs/RATIONALE.md` §"System calls are
algebraic effects" directly after §"Values and commands" — operation vs
interpretation, the OS as the default interpretation, the two orthogonal axes
(reinterpret? capture the continuation?), and the continuation-free reading —
cross-referencing §"Effect handlers" by name rather than by adjacency. Reframed
§"Effect handlers: deep with self-masking" to open as that orthogonal
reinterpretation layer (additive; tail-resumptive, no first-class `resume`, less
than algebraic-effect handlers usually offer). `site/rationale.html` renders
`RATIONALE.md` live via marked.js, so there is no hand-edited HTML mirror to
sync. [[design/syscalls-are-effects|syscalls-are-effects]] now cites RATIONALE
too, and its "not every kernel call is an effect" example dropped `cd` (shell
state, not a record-returning query) for `glob`.

## [2026-06-03] migrate | new related/ layer; seed System C comparison

Added a fifth wiki layer, `related/`, for comparison to existing work — its own
*comparative* staleness character: durable on the external work (a published
calculus does not change), keyed to ral's design via an `against` stamp listing
the pages a comparison leans on. The drift signal is a `decisions/` page that
supersedes a compared-against design page, not a file move. Documented in
[[AGENTS]] (the five-layers count, the layer description with stamp shape, a
*Related drift* lint rule) and listed in [[index]] between decisions/ and map/,
with a hub at [[related/index|related/index]].

Seeded [[related/system-c|system-c]] from a full read of Brachthäuser, Schuster,
Lee, Boruch-Gruszecki, *Effects, capabilities, and boxes* (OOPSLA 2022, Zotero
`4CDKYAI4`). The comparison: the shared *box = thunk / unbox = force* identity
(their §6.8 names ral's CBPV substrate); self-masking as the `\ {f}`
capability-set subtraction with an effect-safety theorem; grant as the
*restriction* (input) half of System C's `𝒞` with no *requirement* (output)
half; capability sets as the dual lattice to grant's meet; regions as `within`.
Divergences ral keeps: open external names vs bound capabilities (the
object-capability concession), full CBPV vs fine-grain CBV, and the
no-first-class-`resume` choice vindicated against System C's fixed-point
capability inference (their hardest open problem, §5.3/§5.6). Cross-linked
inbound from [[design/grant|grant]], [[design/effects-handlers|effects-handlers]],
and [[design/cbpv|cbpv]] so the page is not an orphan.

## [2026-06-03] query | related/scoped-labels — Leijen 2005 read against row-types

Second `related/` page, from a full read of Leijen, *Extensible records with
scoped labels* (TFP 2005, Zotero `AKRCBMLD`). ral takes the calculus nearly
whole: scoped labels (retained duplicates, first-match selection), free
extension as spread shadowing, the distinct-label swap equality, and Leijen's
*(uni-row)* rewrite — ral's row-spine occurs check is his `tail(r) ∉ dom(θ₁)`
termination condition (the one TREX got wrong). The one cut primitive is
restriction `r − l`: verified against `docs/SPEC.md` §20.7 ("without needing a
restriction operator") and RATIONALE, so update/rename (Leijen derives both
from restriction) do not exist in ral and a shadowed field is unreachable —
Leijen's override-then-expose environment idiom is routed through `within` on
the dynamic context instead. Also recorded: two label alphabets vs Leijen's
one, and single-spread-precise / multi-spread-imprecise typing vs his exact
iterated extension. Borrowable: restriction's one-line update/rename, the
fixed-type duplicate-label warning, labeled-vector offset folding. Inbound
link: [[design/row-types|row-types]]'s bare "(Leijen 2005)" citation now links
to the page. While answering the user's "CLAUDE.md updated?" — the wiki
contract is `AGENTS.md` since 2026-05-31 and was updated in the same commit as
the layer — found and fixed stale root-`AGENTS.md` drift in a separate commit
(590acb1): it still pointed at `dev/wiki/CLAUDE.md` and enumerated only three
of the five layers.

## [2026-06-03] query | related/handlers-of-algebraic-effects — Plotkin–Pretnar 2009

Third `related/` page, from a full read of Plotkin & Pretnar, *Handlers of
Algebraic Effects* (ESOP 2009, Zotero `QIBEF77V`). The founding handler
calculus is built on CBPV and its §6.2 example is shell stream redirection
(`> /dev/null`, `yes |` as handlers) — ral's domain is the paper's example
domain made primary. Correspondences: handling as the unique homomorphism out
of the free model is the operation/interpretation separation of
[[design/syscalls-are-effects|syscalls-are-effects]]; the §7 handling equation
recurses only through the continuation variables, so clause bodies escape to
the ambient model — ral's deep + self-masking strip-and-restore stack is that
semantics run operationally. Divergences: ral keeps only tail resumption
(their nondeterminism/rollback/timeout examples need multi-shot or parameter
passing); ral merges their two calculi into one language because the
external-command theory is *free* — no equations, so every clause set is
trivially a correct model and handlers can be runtime blocks; the pipe they
could not express as a handler (their principal open question) is ral's
primitive computation combinator over the byte channel. Borrowable:
parameter-passing handlers degenerate, in the tail-resumptive fragment, to a
stateful clause `(args, param) → (result, param′)` — expressiveness without
touching the no-first-class-`resume` line. Inbound links from
[[design/effects-handlers|effects-handlers]] and
[[design/syscalls-are-effects|syscalls-are-effects]].

## [2026-06-03] ingest | REPL default prompt is a session-scope thunk

The default prompt is now a `RAL_PROMPT` thunk (`{ return "❯ " }`) bound by
`install_default_prompt` at session boot, after prelude registration and
before rc sourcing — so the rc `prompt:` key or a plain `let RAL_PROMPT = …`
overwrites it, and `echo $RAL_PROMPT` shows it. Value bindings can be
overwritten but never removed, so `RAL_PROMPT` is always bound and the
renderer carries no value-shape casing: a block is evaluated, any other value
is its display form (a plain string prompt is the string itself). A failing
user thunk renders an empty prompt beside a diagnostic — the prompt is
cosmetic, the session survives so the user can rebind it; the default thunk
itself cannot fail (constant return), and its boot installation panics on the
impossible. The `RAL_PROMPT` environment variable line left `ral --help`: env
is dynamic-only ([[decisions/260531_env-is-dynamic-only|env-is-dynamic-only]]),
so an exported `RAL_PROMPT` never reached the value scope. Updated
[[map/repl/loop|map/repl/loop]].

## [2026-06-03] ingest | prompt failure fallback is a bare `> `

A failing user `RAL_PROMPT` thunk now falls back to `> ` rather than an empty
prompt — deliberately distinct from the default `❯`, so a broken prompt is
visible at a glance next to its diagnostic. Updated
[[map/repl/loop|map/repl/loop]].

## [2026-06-03] query | related/rows-and-handlers — Hillerström–Lindley 2016

Fourth `related/` page, from a full read of Hillerström & Lindley, *Liberating
Effects with Rows and Handlers* (TyDe 2016, Zotero `6EEPMYUD`) — the effect
typing ral declined: one Rémy row system for variants, records, *and* every
arrow's effect signature. Correspondences: their interpreter is nearly ral's
runtime (FILO handler stack, innermost-match unwinding, forwarding, deep
frames), parting only at the reify point — their generalised CEK captures the
unwound frames as a first-class `k`, ral resumes in place; both drop equations;
both are Levy-lineage substrates under HM+rows. Divergences: effects in types
vs ral's effects in scope (the missing *requirement half* of
[[related/system-c|system-c]]); the **wild inversion** — Links' intrinsic
I/O-like effects are the unhandleable wild row while abstract operations have
no meaning until handled, ral exactly inverted (handleable operations *are*
the world effects, with the OS as default meaning; handlers shadow, never
confer); and even the row flavour differs — Links is Rémy (distinct labels +
presence), ral/Koka/Frank are scoped labels, where a duplicate effect label
models a shadowed outer handler, i.e. ral's stack discipline. Multicore
OCaml's no-effect-types one-shot design bounds the same axis ral ends.
Inbound links from [[design/types|types]] and [[design/row-types|row-types]].

## [2026-06-03] query | related/call-by-push-value — Levy at the source

Fifth `related/` page: Levy's CBPV (TLCA 1999 `EBD23DBT`, the 2003 book
`GMGNPJTX`; the jumping-semantics `83ADMPBG` and stack-adjunction `3JTC3NB8`
papers as the machine vocabulary). Written against ral's own pages rather
than a fresh read — the substrate's facts are foundational. Takes whole: the
two sorts surfaced as the two sigils, blocks as honest `U(A → B)`, `let` as
`to`, and the CBPV-specific soundness fact that `Bind` sequencing is why no
value restriction is needed; the subsumption is live (eager application = CBV
image, passing `{M}` recovers CBN call sites); the trampoline is the jumping
reading of CK. Divergences are extensions: `F` graded by the two byte modes
(`F[I,O] A`), the pipe as a computation combinator CBPV lacks (where ral is a
shell, not a λ-calculus), no computation products (records of thunks serve),
and the effect interface pinned at the external-command boundary. Borrowable:
Levy's stack machine / adjunction models as the off-the-shelf framework if
SPEC §4 is ever formalised, and the βη-theory for the pure fragment. Inbound
link from [[design/cbpv|cbpv]].

## [2026-06-03] ingest | prompt failure falls back to the default ❯

A failing user `RAL_PROMPT` thunk now falls back to the default `❯ ` rather
than a distinct `> `: the per-render diagnostic is already the breakage
signal, and degrading to the out-of-box prompt is the least surprising
behaviour. One `DEFAULT_PROMPT` constant; session boot templates it into the
default thunk source. Updated [[map/repl/loop|map/repl/loop]].

## [2026-06-03] ingest | related-work borrowables triaged: one proposed, five rejected

The maintainer triaged the borrowables the `related/` reading surfaced.
Proposed: [[decisions/260603_stateful-handlers|stateful-handlers]] — a handler
frame threading a state value across interceptions, Plotkin–Pretnar's
parameter-passing handler degenerated to the tail-resumptive fragment
(`(args, state) → (result, state′)`, a fold; no continuations, self-masking
untouched, state dies with the frame, serialises as ordinary shell state).
Rejected, in one batch page
[[decisions/260603_related-borrowables-rejected|related-borrowables-rejected]]:
the duplicate-label lint (warnings are a third diagnostic channel; duplicates
are legal and meaningful under scoped labels), the `--effects` static
operation listing (fails open to ⊤ on computed heads — false assurance; grant
and audit answer the question honestly), record restriction `r − l`
(re-admits un-shadowing; `within` owns the idiom), effect rows (handlers
shadow meaning, never confer it; open namespace makes rows noise), and
capability boxing (contradicts the dynamic-authority commitment; research
direction at most). The four `related/` pages' borrow sections now point at
their verdicts; `index.md` lists both decisions; the contract's status
vocabulary widened to
`proposed | active | fixed | superseded | rejected | open`, matching practice.

## [2026-06-03] ingest | one-mode-engine ADR series: the runtime mode judgment retires

A design conversation found the architectural fault that ral's pipe modes are
operationally a calling convention (they select byte-pipe vs value transport)
yet are erased at the typecheck→evaluate boundary, forcing the second
mode-inference engine in `core/src/ty.rs` —
[[decisions/260601_modes-equality-constrained-shared|modes-equality-constrained-shared]]
shared the lattice and the equality rule, but the judgment itself remained
duplicated. Filed a four-ADR series (all *proposed*) whose end state is one
engine and net code removal:
[[decisions/260603_session-scheme-continuity|session-scheme-continuity]] →
[[decisions/260603_handler-alias-mode-preservation|handler-alias-mode-preservation]] →
[[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]] →
[[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]].
Notable source finds along the way: `CommandTypeResolution.internal` is written
at three sites and read nowhere (dead), and the prelude byte-mode residue
(`classify::prelude_command_spec` / `PRELUDE_BYTE_FNS`) is a third copy of mode
knowledge — both deleted by the series.

## [2026-06-03] query | ADR 1 review: schemes live on the runtime binding, not in a session ledger

A critical review of
[[decisions/260603_session-scheme-continuity|session-scheme-continuity]] found
the draft's session-`Vec` + post-eval-intersection mechanism unsound at its
seams: harvested mono schemes carry unifier-relative variable ids that alias
the next turn's fresh variables (ids restart at zero per `InferCtx::new`); the
"newly present names" intersection misses rebinds and admits the poisoning
turn `let x = 1; boom; x = "two"` (turn-level harvest vs statement-level
install); and `TyEnv::all_named_schemes` skips the handler map, so aliases —
the one cross-turn dynamic rebinding — were never carried. The follow-on
design conversation settled the term-vs-environment question: ground,
occurrence-indexed facts (the ADR-3 wires) belong on the IR because only the
term survives substitution; quantified, name-indexed facts (schemes) belong in
an environment — and the right environment is the `Shell` scope itself, typed
(`Binding { value, scheme: Option<Scheme> }`), with the `Bind` node as the
checker→evaluator courier. Install discipline then keeps schemes and values
coherent for free. Not on the `Value` enum: the evaluator cannot maintain
value-level tags without becoming the re-inference engine the series deletes,
and every value would pay for a per-name, per-turn read. The ADR was rewritten
to this shape; it now also deletes the `bindings` name-set parameter and the
bake's separate `TyEnv` walk.

## [2026-06-03] ingest | session-scheme-continuity lands — schemes live on the runtime binding

[[decisions/260603_session-scheme-continuity|session-scheme-continuity]] (1/4 of
the one-mode-engine series) lands in a four-commit arc (`0e163e9..bd8df30`),
status `proposed → active`.

- *Phase A — checker becomes a transformation.* `typecheck` now returns
  `Result<Comp, Vec<TypeError>>`: `annotate_binds` walks the top-level spine and
  writes each `Name`-pattern bind's generalised `Scheme` onto its `Bind` node
  (`CompKind::Bind { scheme: Option<Box<Scheme>> }`, boxed to stay one pointer
  wide), closed by generalising against the empty environment so no residual
  variable aliases the next turn's fresh ids. Destructuring binds carry none.
- *The one-seed collapse.* The two former inputs (`bindings` name set,
  `prelude_schemes` list) become one `SessionSchemes { bindings, aliases }`;
  `seed_env` is the single seeding routine behind `typecheck`, `bake_prelude`,
  and `alias_arm_scheme`. `compile_and_typecheck(source, SessionSchemes)`.
- *Phase B — the install rule.* A scope entry is `Binding { value, scheme }`;
  `eval_bind` → `assign_pattern`'s `Name` arm installs value and scheme together,
  governed by the statement-level rule already governing the value: a rebind
  replaces both, a statement that never ran installs neither. Schemes ride
  `SerialBinding`/`WireHandlerFrame` so confined turns preserve them.
- *Install-time alias arm inference.* `install_alias` computes the arm's scheme
  at install via `typecheck::alias_arm_scheme` (runtime handler calling
  convention, seeded from `session_schemes()`, closed) and stores it on the
  `HandlerEntry`; `HandlerStack::alias_schemes` is the alias half of the seed.
  All three install paths (alias statement, rc `aliases:`, plugin loads) route
  through it.
- *Phase C — frontends seed from the live session.* REPL turns, exarch tool
  calls, and rc files all pass `shell.session_schemes()` and evaluate the
  annotated comp; rc files seed from the live shell, so an earlier file's binds
  are visible to a later file's check. Batch/`--check` seed from the baked list
  (`SessionSchemes::from_schemes(baked_prelude_schemes())`). `bake_prelude`
  replaces `bake_prelude_schemes`: the build scripts serialise the annotated
  prelude and harvest its bind schemes from one pass.
- *One checker-judgment fix* (03a3fee): in `consumes_value_arg`, a `Return`
  stage whose `spec.input` resolves to ground `Bytes` is a channel consumer
  regardless of its (possibly polymorphic) return value — so a `∅`-output
  producer feeding a byte decoder (`from-json : F[Bytes,∅] α`) is now a static
  T0012, not only the runtime `pipeline_mismatch`.
- *Net deletions:* `Env::binding_names` (→ `binding_schemes`),
  `bake_prelude_schemes`' separate `TyEnv` walk, and `TyEnv::all_named_schemes`
  (orphaned by the single `harvest_schemes`). Tests: `core/tests/session_schemes.rs`,
  one per Verify bullet. Pages touched: the ADR (status + the boxed-slot and
  install-time-arm-inference specifics), `map/core/typecheck`, `map/core`,
  `map/core/ir`, `map/core/shell-state`, `map/core/transport`, `map/repl/loop`,
  `map/repl/startup`, `map/exarch/shell-eval`, the three internals narratives
  (`compilation-ladder`, `type-inference`, `a-turn-end-to-end`), and a one-symbol
  fix in the sibling ADR `ir-pipespec-annotation` (`bake_prelude_schemes` →
  `bake_prelude`).

## [2026-06-03] ingest | ADR-1 implementation learnings folded into ADRs 2–4

What landing session-scheme-continuity taught about the rest of the series,
amended into the three `proposed` pages (statuses unchanged):

- *handler-alias-mode-preservation:* the alias half of the install-time check
  needs no ADR 3 annotation — the frame already stores the arm's closed scheme
  at install (`alias_arm_scheme`), and a closed scheme carries the arm's
  `PipeSpec`; the check is one unification against the head's spec,
  implementable now. The one inference run at install is the static engine
  itself. With aliases covered, computed-`within` opts are the only remaining
  consumer of install-time mode knowledge — an empty set by the page's own
  search, strengthening reject-outright.
- *ir-pipespec-annotation:* the thunk-root `Wire` slot is now conditional on
  ADR 2 keeping computed `within` opts (its alias consumer is served by the
  frame scheme); the rebuilt-IR mechanism is established by `annotate_binds`
  (spine → full traversal; `Wire` rides unboxed where the scheme slot is
  `Option<Box<Scheme>>`); the dual-run rationale gains concrete evidence — one
  static/runtime divergence (`consumes_value_arg` on ground-`Bytes`-input
  stages, 03a3fee) found and retired in advance.
- *unconditional-mode-pass:* the deprecation window forces a rule the total
  error path never needed — a turn whose check reports errors contributes no
  schemes; a warned turn evaluates with its mode wires written but its `Bind`
  scheme slots empty, so turn *N+1* checks its names as bare names and the
  cross-turn seed stays sound.

## [2026-06-03] query | quantifier binding in a Scheme — filed back

A maintainer question ("how is quantifier binding represented in a scheme?")
answered against `scheme.rs`/`generalize.rs` and filed back per the contract:

- New [[invariants/schemes-leave-closed|schemes-leave-closed]] — the one hard
  rule: a `Scheme` leaves its minting unifier only if closed. Quantification is
  nominal-by-listing over unifier root ids, so an open scheme's residual ids
  silently alias a foreign unifier's variables. Two sites mint closed schemes
  (`annotate_binds`, `alias_arm_scheme`); the bake harvest and the serial scope
  tables only transport them. α-equivalence is not quotiented (structural
  `PartialEq` on ids) — comparisons stay within one run or go through
  `fmt_scheme`.
- [[internals/type-inference|type-inference]] gains the representation
  subsection: the ∀-prefix is four id `Vec`s over an ordinary body (bound iff
  listed, no binder node, no de Bruijn); elimination is
  substitution-with-freshening; recursive types are μ-equations on the prefix,
  re-tied in fresh union-find slots per instantiation.

## [2026-06-03] ingest | handler-alias-mode-preservation lands — arms preserve the head's PipeSpec

[[decisions/260603_handler-alias-mode-preservation|handler-alias-mode-preservation]]
(2/4 of the one-mode-engine series) lands as `ed043029`, status
`proposed → active`. The rule: a handler or alias arm for head `h` preserves
`h`'s `PipeSpec` — the mode pair `[input, output]` is pinned by unification
against `h`'s known spec, while the arm's value type stays whatever scheme
inference yields. A mode-changing arm fails that unification as a `ModeMismatch`
([[decisions/260601_modes-equality-constrained-shared|modes-equality-constrained-shared]]:
`none` and `bytes` do not unify), so a handler can no longer flip a head from
byte- to value-output and silently re-wire a pipeline compiled before the
handler existed.

- *One pin function.* `Inferencer::pin_arm_to_head` (`typecheck/infer.rs`)
  extracts the arm's return-`CompTy` modes, reads the head's spec via
  `head_pipe_spec`, and unifies each mode pair. It is consumed by the two static
  sites — the literal `within [handlers: …]` arm path (`typecheck.rs`) and the
  alias-statement path (`typecheck/infer.rs::infer_seq_with_alias_bindings`) — so
  both literal install shapes share one mode-preservation check.
- *`alias_arm_scheme` is now head-aware and fallible.* It takes the head name and
  returns `Result<Scheme, ModeMismatch>`: it pins the arm's spec to the head
  before closing the scheme, so the persistent-frame schemes the two install
  sites store (`evaluator/scope.rs::WithinScope::parse`'s computed-opts arms and
  `types/shell/scope.rs::install_alias`) are mode-checked at install. A computed
  `within [handlers: $h]` opts map and an rc/plugin alias therefore fail closed
  on a mode mismatch where the static check cannot see them.
- *Open point resolved.* The page's open choice — reject computed `within` opts
  outright versus mode-check them at install — settles on the install-time mode
  check: a computed opts map **stays legal**, uniform with the runtime-alias
  case, and is the one remaining consumer of install-time mode knowledge that
  [[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]]'s thunk-root
  `Wire` slot keeps. The frontmatter is now `status: active`.
- *Test-suite migration.* Value-output arms installed over external (byte-output)
  heads were re-spec'd across `core/tests/{typecheck,session_schemes,eval_fuzz}.rs`,
  `ral/tests/spawn_dyn_context.rs`, and `tests/builtins/{with-mocking,with-scope}.ral`
  to use mode-preserving arms or heads whose spec admits the arm's output.

Pages touched: the ADR (status + open-point resolution) and
[[map/core/typecheck|map/core/typecheck]] (the `pin_arm_to_head` /
head-aware-`alias_arm_scheme` description); `index.md` status badge flipped
`proposed → active`.

## [2026-06-04] ingest | ADR-2 implementation inverts ADR 3's thunk-root slot — dropped unconditionally

What landing handler-alias-mode-preservation (`ed043029`) settled for the two
remaining `proposed` pages, amended in (statuses unchanged, still `proposed`):

- *ir-pipespec-annotation:* the thunk-root `Wire` slot, last left *conditional*
  on ADR 2 keeping computed `within` opts, is now **dropped unconditionally** —
  the choice the page anticipated was made (computed opts stay legal,
  mode-checked at install), yet no wire is needed. ADR 2's implementation serves
  *both* install-time halves with ADR 1's mechanism: `WithinScope::parse`
  (`core/src/evaluator/scope.rs:149`) calls `alias_arm_scheme` (head-aware,
  `core/src/typecheck.rs:248`) seeded from `session_schemes()`, exactly as
  `install_alias` (`core/src/types/shell/scope.rs:137`); the computed scheme is
  discarded for `within` (`HandlerEntry.scheme` stays `None`), the pin
  (`pin_arm_to_head`, `core/src/typecheck/infer.rs:743`) runs inside the engine,
  and the catch-all `handler:` arm is exempt. Both halves read no IR node. The
  stronger reason: a *ground* wire cannot serve this consumer by the page's own
  grounding rule — the install check's `PipeSpec` unification exploits
  polymorphism (a divergent arm's free output mode pins to `Bytes`), which the
  `Var → Empty` defaulting would freeze to `Empty` and over-reject. The minimum
  slot set is therefore two: per-stage `Pipeline` wires and the `Bind` RHS
  output mode. The "ADR-2 install check" is struck from the `Wire`-consumer
  lists (Transition section, and ADR 4's sequencing step 1).
- *unconditional-mode-pass:* sequencing step 1 strikes the same phantom consumer;
  one clause now notes ADR 2's install-time `alias_arm_scheme` runs are the
  single engine invoked at install and survive unchanged.
- *handler-alias-mode-preservation* (now `active`): the cross-link asserting
  "ADR 3's thunk-root `Wire` slot keeps that consumer" is corrected — the
  computed-`within` arm is mode-checked by the install-time engine and reads no
  IR node, so ADR 3 carries no thunk-root slot.

## [2026-06-04] ingest | ir-pipespec-annotation lands — the checker writes ground mode wires into the IR

[[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]] (3/4 of the
one-mode-engine series) lands in three commits, status `proposed → active`:

- `d4aa75be` — the ground slots: `ByteMode { Bytes, Empty }` and
  `Wire { input, output }` in `core/src/mode.rs` (the two-valued image of a
  `PipeSpec` with the `Var` arm removed); `CompKind::Pipeline` becomes a struct
  variant `{ stages, wires: Option<Vec<Wire>> }`; `CompKind::Bind` gains
  `rhs_output: Option<ByteMode>`. Construction sites set `None`; nothing writes
  them yet.
- `e3c23b4b` — the writer: the checker records per-node verdicts during
  inference (`InferCtx.stage_specs`, keyed by stage `Comp` address, written at the
  end of `infer_pipeline`; `InferCtx.bind_outputs`, written by the Bind rule for
  every pattern, a `Fun` RHS recording `∅`). `annotate_binds` becomes `annotate`,
  a full structural rebuild over `Comp` and `Val`; the `spine` flag confines
  schemes to Seq parts and `Bind.rest`, while wires/`rhs_output` go everywhere
  inference visited. Grounding (`Var → Empty`) happens once, in `annotate`.
  `bake_prelude` flows through the same pass — the baked blob carries wires with
  no build-script change.
- `edf47cd9` — the readers: `eval_bind_rhs` branches on `rhs_output`;
  `resolve_pipeline(stages, wires, shell)` splits `specs_from_wires` (wire-driven,
  adjacent-wire `debug_assert`, plus a debug-only `runtime_specs` cross-check, no
  argv re-evaluation) from `specs_from_runtime` (the unchecked `--no-typecheck`
  path, byte-for-byte). No deletions — deferred to ADR 4.

*The dual-run caught six divergence classes, each fixed at the root* — on whichever
engine was wrong, never by weakening the assert (a seventh, `consumes_value_arg`,
predates the dual-run, aligned while landing ADR 1, `03a3fee`). The six in
`edf47cd9`: an external head's input is open (`F[var, Bytes]`; runtime `ext()`
aligned via `external_spec`); `named_head` peers through `Force(Thunk(..))` and a
`Bind`'s `rest`; `try`/`guard`/`audit` own no byte output (matching the static
`CompTy::pure(α)`), which exposed **wrong goldens** — `tests/builtins/audit.out`
and `audit-tree.out` were missing body lines the old bind-capture swallowed, a
user-visible bug, re-blessed; `env_binding_type` resolves a closure body
lexically against its capture, treats a bound non-callable value as `∅`, and
breaks recursion with a `seen` set; and `infer_pipeline` records a
value-arg-consumed stage on its own channels (`F[∅,∅]`), the pipeline's tail
output reading the same. Suite: 33 binaries, 0 failures, dual-run asserts live;
clippy clean; `-D warnings` build.

Pages touched: the ADR (status + landing commits + the dual-run learnings folded
into the Transition and consumer-slot sections);
[[internals/compilation-ladder|compilation-ladder]] (the typed-IR rung now
carries ground mode wires, not just schemes; re-stamped, anchors `annotate`/`Wire`
added); [[map/core/ir|ir]] (the new `Pipeline`/`Bind` slots and the
ground-by-type fact; re-stamped); [[map/core/evaluator|evaluator]] and
[[internals/pipeline-execution|pipeline-execution]] (staging reads wires off the
node; the runtime engine is fallback-only; both re-stamped). ADR 4
[[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]] updated: its
ADR-3 dependency landed (commits cited), sequencing step 1 ("Flip authority") is
**DONE** by `edf47cd9` with the dual-run green, and the deletion inventory now
notes the dual-run alignment grew `ty.rs` to **616 lines** (`wc -l`, up from the
predicted 506) with transitional code (`external_spec`, the `seen`-set
`env_binding_type`, `named_head` peering) that dies with the file, that
`validate_pipeline`/`pipeline_mismatch` are already bypassed on annotated IR
(reachable only from `specs_from_runtime`/`runtime_specs`), and the net-deletion
count corrected to 649 lines. `index.md` status flipped `proposed → active`,
`ir`/`evaluator` map stamps bumped.

## [2026-06-04] ingest | unconditional-mode-pass lands — one judgment, one engine

[[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]] (4/4, the
final ADR of the one-mode-engine series) lands in two commits, status
`proposed → active`. The series' end state is reached: a single mode-inference
engine, the static checker, whose verdict the evaluator reads off the IR.

- `767a96bf` — the inference pass goes unconditional on every evaluated path
  (batch, rc, REPL turn; exarch already did). `TypeErrorKind::fragment()` splits
  errors into `ErrorFragment::{ Mode, ValueType }` — a `ModeMismatch`, or a
  `CompTyMismatch` carrying a `Stdin`/`Stdout` channel diff, is the mode fragment;
  all else is value-type. `typecheck_verdict` returns the three-armed
  `Verdict::{ Clean, ValueErrors { comp, errors }, ModeErrors }`; the `ValueErrors`
  comp is wired-but-scheme-less (`annotate(_, false)`). `compile_to_verdict` is the
  flag-honouring twin of `compile_and_typecheck` (the latter stays strict for
  exarch, `--check`, the build). `--no-typecheck` narrows from "skip the checker"
  to "downgrade value-type errors to ariadne warnings"; REPL/batch/rc honour it,
  `--check` stays strict. rc files report-and-skip on a fatal fragment, warn-and-
  apply under the flag, the boot always survives. A warned turn installs
  scheme-less bindings — no scheme cascade. New CLI suite `ral/tests/no_typecheck.rs`.
- `602218d3` — the harvest. The runtime engine `core/src/ty.rs` (616) and the
  prelude byte-mode residue `core/src/classify.rs` (33) come out whole (these two
  whole-file removals physically rode the concurrent docs commit `25bc13f5`; the
  engine-internal test target `core/tests/type_system.rs` (65) rode `55ba22c3`;
  `602218d3` completes the parcel). The annotation slots drop their `Option` —
  `Pipeline.wires: Vec<Wire>`, `Bind.rhs_output: ByteMode`, the elaborator emitting
  `Wire::EMPTY` placeholders the annotator overwrites. `ModeStore` + the free
  `unify_mode` fold into the checker's `Unifier` as a plain method; the lattice
  (`PipeMode`/`PipeSpec`/`ByteMode`/`Wire`/`ModeMismatch`) survives.
  `validate_pipeline`/`pipeline_mismatch` deleted, demoted to the adjacent-wire
  `debug_assert!` in `specs_from_wires` (static T0012 unaffected). The two
  remaining unchecked-eval paths — the `source`/`use` loaders (`check_source` over
  `compile_to_verdict`, both fragments fatal) and plugin files — now check. The
  synthetic single-stage uutils pipeline synthesizes `Wire { input: Empty, output:
  Bytes }`. The dual-run scaffolding (`runtime_specs`, the `specs_from_wires`
  cross-check, `rhs_is_byte_output`'s comparison) is torn down with the engine.

*Two latent bugs surfaced and fixed* — both invisible while modes were re-inferred
at runtime, fatal once the evaluator reads them off the node: the `Ast::Background`
elaborator arm hoisted `cmd &`'s pipeline eagerly via `to_val` instead of
suspending it in a thunk (now `Background(Val::Thunk(..))`, three behavioural
tests); `eval_fuzz`'s harness evaluated the bare elaborated comp, discarding the
annotation (now runs the checked IR).

*Implementation order reversed two sequencing steps:* the pass went unconditional
(`767a96bf`, steps 1+3) **before** the engine was deleted (`602218d3`, step 2),
because deleting the fallback first would have left wireless `--no-typecheck` IR
unrunnable. *Deliberately not landed:* the `no_typecheck` plumbing strip — the
deprecation window is open, so `RunOpts.no_typecheck` and its threads survive until
the value-type bug stream dries up. The `Option`-drop half of step 4 *is* done
(it needed only the unconditional pass, not the window's close).

Verification: 33 test binaries, 0 failures (`type_system` target removed,
`no_typecheck` target added against the pre-series 33); `-D warnings` + clippy
clean; `rg pipeline_mismatch` no code hits; smokes — T0012 on `echo foo | length`,
map-lines/uutils/external pipelines run, the flag warns-and-runs.

Pages touched: the ADR (flipped to `status: active`, `landed: [767a96bf,
602218d3]`, a Landed block, the verdict shape, the loaders/uutils/latent-bug
sections, sequencing annotated done, the order-reversal note);
[[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]] (its parked
deletion inventory now cites `602218d3` as landed);
[[internals/compilation-ladder|compilation-ladder]] (the typed-IR rung is the only
mode source) and [[internals/pipeline-execution|pipeline-execution]] (staging reads
`Vec<Wire>`, no runtime fallback), both re-stamped;
[[internals/type-inference|type-inference]] (sole engine, `unify_mode` a plain
method, `annotate`; anchors `annotate_binds`/`ModeStore` retired) and
[[internals/surface-syntax|surface-syntax]] (head classification is the parser's
`Head`, not the deleted `classify.rs`; anchor `classify`→`Head`), re-stamped;
[[map/core/ir|ir]] (non-optional slots, `Wire::EMPTY` placeholder),
[[map/core/typecheck|typecheck]] (the "two engines" section rewritten to one;
`ty.rs` dropped from `covers_paths`), [[map/core/evaluator|evaluator]],
[[map/core/syntax|syntax]] (`classify.rs` residue gone), [[map/core|core]],
[[map/repl/startup|startup]] (batch under the three-armed verdict),
[[map/repl/loop|loop]] (per-turn `compile_to_verdict`, rc verdict behaviour),
[[map/exarch/shell-eval|shell-eval]] (strict, now on one engine),
[[map/core/prelude|prelude]] and [[design/builtins|builtins]] / [[design/types|types]]
(stale `ty.rs`/`classify.rs`/runtime-engine references corrected),
[[invariants/schemes-leave-closed|schemes-leave-closed]] (`annotate_binds`→`annotate`),
and `index.md` (ADR-4 status `proposed → active`, map stamps bumped). The
one-mode-engine series is complete: all four ADRs `active`.

## [2026-06-04] ingest | unconditional-mode-pass — the deprecation window closes, the flag retires

The fourth step of [[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]
lands (`51419f6c`): `--no-typecheck` is removed and, with it, the entire
fragment/verdict apparatus `767a96bf` built to honour it. **The flag was the sole
consumer**: with value-type errors fatal on every path — exactly as mode errors
already were — the mode-vs-value fragment distinction has no behavioural reader and
the three-armed verdict collapses to the "clean comp / errors" the pre-existing
strict path (`typecheck` / `compile_and_typecheck` / `CompileOutcome`) already
expressed. Net −249 production lines.

- *Routing.* Every evaluated path returns to the strict single entry: REPL turn
  (`exec.rs::execute_input`), batch + `-c` and `--check` (`main.rs::run_batch`), rc
  (`boot.rs::source_config_inner`), and the `source`/`use` and plugin loaders
  (`modules.rs::check_source`, `plugin/load.rs`) — all now `compile_and_typecheck` /
  `typecheck`. A type error of any kind is fatal: the turn/script blocks, the rc
  file is reported and skipped while the boot survives (the parse-error precedent).
- *Flag plumbing deleted.* `RunOpts.no_typecheck` + the clap arg, `Session.no_typecheck`,
  and the `no_typecheck` params threaded through `step`/`execute_input` and
  `load_profiles`/`source_config_file`/`source_config_inner`. `--no-typecheck` is now
  an unknown flag.
- *Apparatus deleted.* `Verdict` + `typecheck_verdict` fold back into `typecheck`
  (`core/src/typecheck.rs`); `CompileVerdict` + `compile_to_verdict` go, leaving
  `compile_and_typecheck` the single compile entry (`core/src/lib.rs`);
  `ErrorFragment` + `TypeErrorKind::fragment()`/`TypeError::fragment()` go
  (`scheme.rs`) — the `CompDiff::Stdin`/`Stdout` variants stay (the unifier's real
  per-channel diff); `format_type_warning_ariadne` and `render_ariadne`'s
  `ReportKind` param go (`diagnostic.rs`), `render_messageless_kind` folds back into
  `render_messageless`. Static T0012 / type-error rendering byte-for-byte unchanged.
- *Smell-review fixes, same commit.* The `infer.rs` tail-stage comment loses a
  tombstone parenthetical naming deleted code; the uutils synthetic wire literal
  becomes a named `Wire::EXTERNAL` constant (`core/src/mode.rs`, beside `Wire::EMPTY`,
  mirroring `PipeSpec::ext`); `InferCtx` gains a sentence on its address-keyed maps'
  single-tree validity (`env.rs`); the duplicated `ValListElem` match in
  `annotate_val`/`annotate_args` extracts to one `annotate_list_elem`.
- *Tests.* `ral/tests/no_typecheck.rs` → `type_errors_block.rs` (renamed, not
  dropped — binary count stays 33): the downgrade tests retired with the behaviour,
  the live-behaviour tests ported to the no-flag world (a value-type error blocks
  the binary, a mode error blocks, an rc type error skips-but-boots). The
  in-process `fragment()` classification tests (`core/tests/typecheck.rs`) and the
  warned-turn-installs-no-scheme tests (`core/tests/session_schemes.rs`) deleted
  with the code they exercised.

Verification: `-D warnings` build + clippy clean; 33 test binaries, 0 failures; `rg`
no code hits for any removed symbol. Smokes: value-type error now *blocks*
(`let x = hello; return $[$x + 1]` → T0010 Error, nonzero); map-lines and external
pipelines run; `--no-typecheck` is an unknown flag; exarch builds, `shell_eval`
untouched.

Pages touched: the ADR (retirement note at the head, a *Closing the window* section,
the two window sections marked retired, the loaders/uutils references corrected,
sequencing steps 3+4 done, a window-close *Verified* paragraph);
[[map/repl/startup|startup]] (batch under `typecheck`'s `Result`),
[[map/repl/loop|loop]] (per-turn + rc `compile_and_typecheck`/`typecheck`),
[[map/core/typecheck|typecheck]] (`typecheck_verdict` bullet removed),
[[map/exarch/shell-eval|shell-eval]] (strict, no dead-flag contrast),
[[internals/type-inference|type-inference]] (unchecked-path list), all re-stamped;
and `index.md` (window-closed). The one-mode-engine series is fully retired down to
its last flag: one judgment, one engine, one verdict.

## [2026-06-04] ingest | LoC-reduction sweep — dead code, duplication, and one-consumer carriers removed

**~320 net LoC removed across commits `44bf8a52..2b999feb`**; build and full test suite green, Windows build cross-checked. No behaviour or architecture changed.

- *Source registry removed.* The `SourceDb` / `SourceFile` / `DbCache` layer in `source.rs` is gone; source positions are now bare `Span` / `FileId` values handed directly to ariadne at render time. [[map/core/diagnostics|diagnostics]] rewritten to reflect the current shape.
- *Surplus capability and handler methods removed.* `HandlerStack::len` / `is_empty` and `GrantStack::len` (the `GrantStack` type remains); the two `From<Vec<Capabilities>>` impls on `GrantStack`; `HandlerArity::Direct` (dead variant).
- *Duplication collapsed.* `AuditSite` merged into `CallSite`; a local `sig` in `capability/load.rs` replaced by `crate::types::sig`; a duplicate Windows `stage_err` replaced by `runner::stage_err`; `fmt_comp_ty` callers migrated to `fmt_comp_ty_ctx`; `push_scope_node` callers migrated to `push`.
- *One-consumer carriers dropped.* `HandlerThunk` (single-variant enum — `HandlerEntry.thunk` is now a bare `Value`); `DeferredFrame` (single-variant enum — the name survives as `type DeferredFrame = PendingFrame<StageJob>`); `LaunchOutcome` (single-field carrier — returns `ByteChannel` directly); `ChildIo` (carrier inlined into `spawn_child`); `redirect_io_error` and `stream_nil` (trivial helpers inlined); `config_base` / `data_base` in `path/config.rs`; `pipeline_thread` in `io.rs`; `BOLD_MAGENTA` in `ansi.rs`; `List::back` / `pop_back`.

Touched map pages re-stamped to `2b999feb`: [[map/core/diagnostics|diagnostics]], [[map/core/shell-state|shell-state]], [[map/core/capabilities|capabilities]], [[map/core/typecheck|typecheck]], [[map/core/evaluator|evaluator]], [[map/core/builtins|builtins]], [[map/core/transport|transport]], [[map/core/io-process|io-process]], [[map/core/syntax|syntax]].

## [2026-06-05] query | the freeze boundary — why `RawCapabilities` and `Capabilities` are two types

A conversation tracing the purpose of `RawCapabilities` and whether the two-stage capability pipeline could be simplified. The structural verdict: **no — the seams are already in the right place**, and three tempting unifications were each declined for a concrete reason:

- *Collapse the two types into one typestate `Capabilities<Stage>`* — declined. Only the resolved form derives `Serialize`/`Deserialize`; a derived `Serialize` over the parameterised struct admits every stage and reopens the IPC wire to the unresolved, sigil-bearing form. The nominal split is what keeps sigils off the wire by construction.
- *Unify the composition `meet` with the runtime fold* — declined; already covered by [[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]] (one combinator at two fidelities; the two folds stay separate). The shared atoms (`meet_prefix_sets_by`, `meet_literal_exec`) are the right DRY line.
- *Retire composition-meet, push per-profile layers and let the runtime fold them* — declined. Exarch needs `join` (`--extend-base` widens; a stack only meets) and `for_invocation` is contracted to return one `Capabilities`.

Filed the durable knowledge as [[design/capability-freeze|capability-freeze]]: the syntactic→resolved boundary, `freeze` as the unique one-way door, the serde-asymmetry rationale, and the *compose-then-freeze* discipline.

The only change the analysis justified was documentation: docstrings on `RawCapabilities` / `Capabilities` (`core/src/types/capability.rs`), the `prefix.rs` reference to a nonexistent `Capabilities::meet`, and [[design/grant|grant]]'s prose all carried tombstones and a dead "toml profile" reference (toml was retired; profiles are `.ral` scripts decoded to a `Value::Map`). Rewritten to describe only what is there now. [[map/core/capabilities|capabilities]] re-stamped to `108cdb67` with the freeze boundary added to its type list.

## [2026-06-05] query | what unification would cost — wire-safety is recoverable, simplicity is not

A follow-up pressing on whether a typestate unification is actually viable. **Corrects the prior entry's claim** that collapsing into `Capabilities<Stage>` "reopens the IPC wire" — verified empirically (throwaway serde probe) that it need not:

- the *default* `#[derive(Serialize)]` over `Capabilities<S>` does admit every stage — serde's `PhantomData` carve-out adds no `S: Serialize` bound;
- but a `Raw` marker that does not implement `Serialize`, plus `#[serde(bound(serialize = "S: Serialize", deserialize = "S: Deserialize<'de>"))]`, excludes the unresolved stage from the wire — and with the `PhantomData` field `#[serde(skip)]`ped the wire bytes are byte-identical.

So the honest ledger of what unification loses: a *naive single type* loses the compile-time theorem (a sigil-bearing bundle could reach the stack or the wire — a security-relevant invariant downgraded to reviewer convention) for ~30 saved lines; a *typestate* keeps the theorem but trades two self-describing names for marker types, a serde-bound incantation, `PhantomData` literals, and stage-ambiguous `default`/`root`/`deny_all` — net no simpler. Either way the two-name design is the cheapest expression of the invariant. [[design/capability-freeze|capability-freeze]] §"Why two types, not one" rewritten to say this.

## [2026-06-05] ingest | collapse the capability stage split into one always-frozen `Capabilities`

**Reverses** the two prior same-day query conclusions.  They declined a typestate `Capabilities<Stage>` on legibility grounds — and were right about that shape.  This change is different in kind: it does not parameterise the type, it **deletes the syntactic stage**, freezing at decode time.  With no `Raw` stage there is no `PhantomData`, no serde bound, no stage-ambiguous `default`/`root` — the typestate's cost never arises, and the compile-time "no sigils on the stack/wire" theorem is preserved because every `Capabilities` is resolved by construction.

What landed:

- `decode_capability_map` (`core/src/capability/decode.rs`) became the single `Value::Map → Capabilities` constructor: it takes a `&FreezeCtx` and runs the freeze pass inline (the relocated `freeze_exec_map` lives here now).  Both callers — `eval_grant` and the profile loader — are in-crate, so "always frozen" is a visibility fact.
- Deleted: `RawCapabilities` and its `freeze`/`freeze_in_env`/`validate_paths`; `path::sigil::validate_xdg_tokens`; the second instantiation of the `deny_all`/`is_restrictive` macro (inlined onto `Capabilities`, macro retired).  `meet`/`join` moved onto `impl Capabilities`.
- Core loaders renamed `load_raw_capabilities_from_*` → `load_capabilities_from_*`, threading a `&FreezeCtx` and returning frozen `Capabilities`; `apply_session_profiles` and exarch's `for_invocation`/`resolve_base`/`load_capabilities_ral` build the ctx once and drop the trailing freeze.

Intended behaviour changes: the **xdg-escape guard is now a per-profile invariant** (fires on any input profile naming an `xdg:` path outside `$HOME`, survive composition or not — fail-closed); `meet`/`join` compose **resolved paths** (strictly more precise); error attribution moves from "invalid grant after composition" to a per-profile decode/freeze error.  Enforcement unchanged — `grant_policy`, `lattice_tests`, `ral/tests/capabilities` green before and after.

Filed [[decisions/260605_capability-stage-collapse|capability-stage-collapse]] (active); rewrote [[design/capability-freeze|capability-freeze]] (thesis inverts — the boundary is a pass inside `decode`, the guard an invariant); updated [[design/grant|grant]]; re-stamped [[map/core/capabilities|capabilities]] and [[map/exarch/policy|policy]] to `96a09da9`; added a forward link from [[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]]; `docs/SPEC.md` §11.9 walks into a `Capabilities` now.

## [2026-06-05] query | why `Capabilities` / `Reduced` / `SandboxProjection` aren't one type

Pressed on whether the capability types could collapse into one (`GrantStack` aside).  The load-bearing answer, sharper than the existing pages stated it: **the runtime reduction of a stack is not an endomorphism on the lattice — it does not factor through a single `Capabilities`.**  `EffectiveGrant::reduce` folds *nothing*; it forwards the whole stack and every verdict re-walks the layers, because the real question ("is *this* action allowed *now*?") is subject-relative (the three-valued exec verdict is a predicate on runtime `argv`), resolver-relative (lexical in the child, lenient in the parent), and canonical (symlink-resolved), none of which survive precomputation into a lattice element.

The complement: `Reduced` and `SandboxProjection` are the *same* reduction rendered for two evaluators of unequal power — ral judges live and losslessly; the OS kernel must be handed an eager, total, argv-erased path list because it has no `argv` at `open()` and cannot call back.  That unmergeability *is* the [[design/two-enforcers|two-enforcer]] fact in type form ([[decisions/260602_exec-authority-partitioned|260602]] §"the two folds stay separate").  `EffectiveGrant` is the thin one — a typestate seal forcing the fold before any verdict ([[decisions/260601_reduced-authority-witness|260601]]) — and honestly inlinable.

Filed as [[design/capability-carriers|capability-carriers]] (intuitive register — rule / judgment / checklist, no code mechanics); cross-linked from [[design/two-enforcers|two-enforcers]], [[internals/capability-enforcement|capability-enforcement]], and [[map/core/capabilities|capabilities]].

## [2026-06-05] ingest | tidy the capability decision/decode helpers; no behaviour change

Pure hygiene over `core/src/capability/` — **the enforcement design, the lattice, and the runtime flow are untouched; this is naming, dedup, and doc register.**  No `decisions/` or `invariants/` page moves, and the [[internals/capability-enforcement|capability-enforcement]] anchors all survive.

What landed:

- `check.rs` — the read/write distinction, carried as a parallel `op: &str` + `get_prefixes` closure through two single-call `check_fs_*_impl` wrappers, is now one `FsOp` (Read | Write) feeding a single `check_fs_op` that `effective.rs` calls directly.  The four hand-rolled `Break::Error(Error::new(.., 1)…)` denials route through the canonical `sig` / `sig_hint` constructors; the two `Ok(())` exec arms fold together and the subcommands branch flattens to one guarded match.
- `decode.rs` — `decode_fs` loses its dead `allow_deny` / `strict` parameters (the manifest decode path is gone; the sole caller passed `(true, true)`); the editor/shell bool-map decoders share one `reject_unknown_keys` rather than duplicating the post-fold re-walk; the dimension decoders drop from `pub(crate)` to private (only `decode_capability_map` calls them).
- Docs across `capability/` brought to the house register — no tombstones (the manifest-capabilities aside, "two flavours", "previously rejected"), no "front door" in the code comments, and the `Reduced` witness invariant stated once rather than four times.
- `lattice_tests::decode_rejects_xdg_var_outside_home` was racing a concurrent `with_xdg_defaults` over the process-global `XDG_DATA_HOME`, so the resolution fell back to the home default and the rejection never fired; both now serialise behind a poison-tolerant env lock.

Re-stamped [[map/core/capabilities|capabilities]] to `f8bbef8c` — the page never named the refactored internals, so its prose stands unchanged.

## [2026-06-05] query | why symlink resolution is deferred to the access, not the freeze

Pressed on whether the grant could be *fully* resolved at decode — symlinks and relative paths included — so the check is a pure prefix-match.  For symlinks the answer is **no, and the reason is the threat model, not convenience: the symlink that defeats a grant is planted at runtime, inside an already-granted region, on the path a body opens — it does not exist at decode, so there is nothing for freeze to resolve.**  Canonicalisation is a property of the access, not the rule; pinning the grant's prefixes to canonical form would defend nothing and would bind the grant to the decode-time symlink topology.

Filed into [[design/capability-freeze|capability-freeze]] §"What freeze does not resolve".  Surfaced one real gap on the way: a *bare relative* grant prefix is not frozen — `freeze_one` only rewrites sigil-bearing entries — so it resolves against the **live** cwd at check time rather than the grant-time cwd.  Tracked for the simplification pass (freeze or reject bare relatives at decode).

## [2026-06-05] ingest | reject bare relative grant paths at decode

Closed the gap the previous query flagged.  **A bare relative grant entry is now a decode error, not a live-cwd anchor.**  Of the two options tracked (freeze vs reject), reject was chosen: ral already treats path resolution as opt-in via sigils (`freeze_leaves_literal_paths_alone`), `cwd:` is the one explicit "relative to here", and a bare relative in a security map is far likelier a typo than intent — so erroring beats guessing.

What landed (`core/src/capability/decode.rs`): a `require_absolute` helper runs after the freeze pass over the fs `read`/`write`/`deny` lists and inside `freeze_exec_map` for every `dirs` key and every path-shaped `literals` key.  A bare command name (no `/`, no sigil) stays exempt — it is a name, not a path.  The violation names the offending entry and points at `cwd:sub` or an absolute path.

Recorded the third freeze-pass action in [[design/capability-freeze|capability-freeze]] (resolve sigils · reject xdg-escape · reject bare relative); `docs/SPEC.md` §11.2.1 aligned.  No `decisions/` page — this is a behavioural tightening foreshadowed by the freeze design, not a new direction.

## [2026-06-05] ingest | collapse the reduced-authority witness to free functions

**The `EffectiveGrant` → `reduce` → `Reduced` typestate is removed; the capability decisions are now free `capability::check_*(&Context, …)` functions.**  The witness sealed an identity, not a fold: `reduce` was identity on data (both types wrapped exactly `&Context`), and the property it guarded — judge from the whole meet-folded stack — already lives in the check bodies, each iterating `ctx.grants`.  The chokepoint is now a module boundary, not a type ([[decisions/260605_witness-collapse|witness-collapse]], superseding [[decisions/260601_reduced-authority-witness|reduced-authority-witness]]).

What landed: `effective.rs` → `decide.rs` (`admits_head`, `sandbox_projection`, editor/shell gates, projection builder); `check.rs` exposes `check_exec_args` / `check_fs_op` over `&Context`; `evaluate_exec` and `canonical_grant_paths` take `&Context`; `Shell::audit_call` keeps its disjoint context/audit borrow split, now handing a `&Context` to the check.  The two-folds split (in-ral vs `SandboxProjection`) and the host-vs-sandbox resolver distinction are untouched — only the ceremony is gone.

Rewrote [[design/capability-carriers|capability-carriers]] to three carriers (rule / live judgment / projection — the live judgment is a function, not a type); re-stamped [[internals/capability-enforcement|capability-enforcement]] (`92abfa7b`, anchors → `check_exec_args` / `check_fs_op` / `sandbox_projection`) and [[map/core/capabilities|capabilities]] (`92abfa7b`).

## [2026-06-05] query | why freeze pins some paths and the access resolves others

Pressed on the threat that deferring symlink resolution actually defends against.  **The danger is a symlink escape: a confined body, writing inside its own granted region, plants `~/sandbox/leak → /etc/passwd` and then opens it — a lexical prefix match would admit the link; only canonicalising the *access* before matching resolves it outside the region and denies it.**  A time-of-check-to-time-of-use race whose use is the `open()`, so resolution must bind to the filesystem at the access, not at decode; the adversarial link does not yet exist when freeze runs.

Sharpened [[design/capability-freeze|capability-freeze]] §"What freeze does not resolve": made the failure mode legible (lexical admits vs canonical denies), named the TOCTOU, and added the dual-defence capstone — freezing sigils pins the rule's authorship (no widening by moving), deferring symlink resolution binds it to the live world (no escape by re-pointing).  *Pin the intent; resolve the world.*

## [2026-06-05] ingest | split the capability decision layer into enforce.rs and sandbox.rs

**The `check`/`decide` file boundary — drawn at audit-or-not — is replaced by `enforce`/`sandbox`, drawn at the real seam: point-of-use gates versus OS-projection synthesis.**  `decide.rs` straddled both (the editor/shell/head gates *and* the `SandboxProjection` builder), so the names told the reader nothing; the new pair names what each file answers.

What landed (`core/src/capability/`):
- `check.rs` → `enforce.rs`, now home to every point-of-use gate — `check_exec_args`, `check_fs_op` (both audit-bearing), `admits_head`, and the editor/shell bool gates relocated from `decide.rs`.
- `decide.rs` → `sandbox.rs`, holding only the OS-renderable projection (`sandbox_projection` / `project` / `reduce_exec`).
- The six hand-rolled `for caps in &ctx.grants { if let Some(field) … }` folds now run through dimension iterators on `GrantStack` — `exec()`, `fs()`, `net()` — each yielding only the opining layers, so the `saw_*` bookkeeping collapses to `.next().is_some()`.  `layer_exec_verdict` takes `&ExecMap` and `LayerExec::NoOpinion` is gone — the iterator already filtered it.

Re-stamped [[internals/capability-enforcement|capability-enforcement]] (`cec1d535`) and [[map/core/capabilities|capabilities]] (`cec1d535`).  No `decisions/` page — a structural rename plus a DRY pass over the stack walk, with no change to the authority model.

## [2026-06-05] query+ingest | the surface/resolved prefix duality is load-bearing — lift it into `path::PrefixSet`

Pressed on whether the `GrantPath { canonical, raw }` pair in `capability/prefix.rs` had outgrown its use and could collapse to one string.  **Verdict: no — the duality is the minimal carrier of "enforce the ceiling on the symlink-resolved form, emit the author's lexical form," and the OS backends prove it.**  Traced every `.canonical` read (all inside `prefix.rs`; only `.raw`/surface escapes) and confirmed `meet_prefix_sets_by` judges overlap with the alias-aware `path_within` regardless of key — so `canonical`'s *only* distinct effect is resolving **non-firmlink** symlinks in the cross-layer meet.  Firmlink pairs (`/tmp`↔`/private/tmp`) are bridged either way; the field earns its keep only for arbitrary symlinks.

That case is real, not theoretical: base ceiling `{/a}`, inner grant `{/a/link}` with `/a/link → /x` outside `/a`.  Canonical meet resolves to `/x ⊄ /a` → empty projection (fail-closed); a lexical meet keeps `/a/link`, emits it, and `bwrap --bind` follows the source symlink (Seatbelt matches lexically per `lex.rs:24-27`) → a **spawned child** reads `/x`.  The in-process `check_fs_op` is unaffected (it canonicalises prefixes at check time), so the exposure is child-only — exactly what the projection gates.  Dropping `canonical` would silently *widen* child authority, so it stays.

Ingest: the pair is path algebra, not a capability concern — its primitives (`canonicalise_lenient`, `meet_prefix_sets_by`, `Resolver`) all live in `crate::path`, and the old module touched `Context` only to fetch a `Resolver`.  Lifted `capability/prefix.rs` → `core/src/path/prefix_set.rs` as `PrefixSet` — `resolve(&Resolver, …)` / `Meet` (intersect on resolved) / `union` (sticky denies) / `surface()` (emit lexical) — hiding the `GrantPath` pair and the leaky `.0`, folding two copies of the sort into one `normalise`.  Behaviour identical; `sandbox.rs` is the lone caller and now reads in path vocabulary.  Re-stamped [[map/core/capabilities|capabilities]] (`64e2e96e`).  No `decisions/` page — relocation + readability, semantics preserved.

## [2026-06-06] ingest | complete the co-inductive unifier with one-sided obligations

A user's rc stopped type-checking after the swallow-removal of `79568d97` made `apply_piped_value` strict — correctly, it turned out: `from-lines | map/each` was a latent *runtime* crash (`from-lines` is a lazy `Step` stream, `map`/`each` are list combinators; the pipe driver feeds the stream element-by-element, handing `map` a `String` where it wants a list).  The rc was fixed to the streaming idiom (a per-line block).  But the investigation surfaced a real robustness bug: a `stream-map` written to take a `Stream` **value** instead of a `Thunk` **overflowed the type-checker's stack** during `--check`.

**Verdict: complete the co-inductive guard, not bound the depth.**  The equi-recursive unifier admits cyclic types with no occurs check and relies on `Pairs` to discharge a re-entered obligation at the fixed point — but it memoized only symmetric `Var`/`Var` root pairs.  The same `Step` type anchored at a comp-var (`from-lines`: `C = F Step(C)`) versus a ty-var (the value-taking combinator: `T = Step(F T)`) re-enters as `T ~= Step(C)` / `F T ~= C` — always variable-vs-concrete-structure, never a var pair — so the guard never fired and the descent unrolled to stack exhaustion.  `apply_ty` survived via its `Visited` stack; only unification lacked the guard.  (Diagnosis and patch shape from a second agent; lldb put the loop in `unify_row_inner`.)

Fix: `Pairs` now also memoizes *one-sided* obligations — a var root against a finite structural key (`TyKey`/`CompTyKey`/`RowKey`) that canonicalizes nested vars through `find` but never expands bindings, so it stays finite over a cyclic root.  Re-entry on the same obligation is the cyclic fixed point; the two anchorings **unify** (regular-tree/bisimulation completion), not reject.  Rows stay inductive.  `PipeMode` gained `Hash` for the keys; the false `apply_ty` comment ("no ral-typeable program lands" in the ty-var value-cycle case) is corrected.  Two regression tests (low-level unifier `T`-vs-`Step(C)`; source-level value-taking `smap`).  Commit `9f5923a6`.  Added [[decisions/260606_unify-one-sided-obligations|unify-one-sided-obligations]]; updated [[internals/type-inference|type-inference]] and re-stamped [[map/core/typecheck|typecheck]].

## [2026-06-06] ingest | the Stream combinators take a value, not a thunk

Follow-on to the unifier fix: the prelude `stream-*` family no longer needs the `Thunk(F Stream)` parameter that earlier kept inference well-typed.  Now that a value-var-anchored cycle unifies with a comp-var-anchored producer, `stream-map`/`each`/`fold`/`take`/`drop`/`to-list` take a `Stream` **value**, `case $s` directly, and recurse through the forced tail `!$p[tail]`.  The cons cell's tail stays a thunk, so laziness is unchanged (`stream-take 5` over an infinite `nats` still terminates).  This retires the `{ return $s }` wrap at call sites — a stream value passes bare (`stream-to-list $s`), a producing combinator's result is forced once (`!{stream-map $f $s}`).  Updated the combinators and `from-lines-list`, the `tests/practical` scripts, the `pipeline`/`variants`/`windows_pipeline` fixtures, and the agent-facing `exarch/data/ral.md`.  Commit `b60ce01e`.  Corrected the forward-looking parenthetical in [[decisions/260606_unify-one-sided-obligations|unify-one-sided-obligations]] (the recursion is direct, not via an inner helper) and re-stamped [[map/core/prelude|prelude]].

## [2026-06-06] ingest | a fresh alias/handler head defines its own modes

`02ee909f` relaxes mode preservation to bind only a *known* head.  `head_pipe_spec`'s unknown case no longer pins a brand-new alias/handler name to the external byte default `F[μ, Bytes]`; it mints a fresh `F[μ, ν]`, so `pin_arm_to_head`/`handler_comp_scheme` let the arm define the head's modes.  A known head whose scheme resolves to a `Return` still constrains the arm — reinterpreting it with incompatible modes stays a positioned `ModeMismatch`.  The byte-channel discipline (`O_left = I_right`, SPEC §4.2.1) is unchanged; it moved from eager definition-time pinning to connection-point checking, so a value-output head piped into a byte consumer is now flagged at the use site, not the definition.  `infer_chain` unions a `?` chain's arms' input and output modes (`union_mode`, as `merge_branches` does for `if`) while keeping the chain's value type a fresh variable, so `tmux a ? tmux b` reads byte-output and installs over a byte head.

Why now: [[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]] made plugin files and rc aliases fatally checked by the single static checker, and eager pinning blocked ordinary value-output aliases (`… | from-lines | { |x| … }`) and `?`-chain aliases from installing.  The relaxation is the correct resolution, not a weakening — the byte discipline still binds, at the edge.  Companion commits `ee7727e4` (honest pipe-mode/record schemes for `_ed-tui`/`_ed-set`) and `998e9916` (drop PWD/OLDPWD when applying an rc env map) are the editor-builtin and config surface that prompted it.  Added [[decisions/260606_alias-head-defines-its-modes|alias-head-defines-its-modes]] (supersedes [[decisions/260603_handler-alias-mode-preservation|handler-alias-mode-preservation]]); re-stamped [[map/core/typecheck|typecheck]] at `998e9916`.

## [2026-06-06] ingest | the cacheless unified module loader

`466b36f5` drops the `use`/`source` result cache.  The cache keyed a bindings map by canonical absolute path with no invalidation, so a long-lived REPL that re-`use`d an edited file served stale bindings; in a shell the memoisation buys nothing — the REPL is human-paced, scripts are separate processes whose caches never share, and an in-run diamond over the same module is rare.  Each `use`/`source` now re-reads and re-evaluates.  The cycle stack and depth bound in `evaluate_source` are RETAINED and now load-bearing: with no cache absorbing a second visit, they are the only thing keeping a self-referential module from diverging.  `use` collapses onto `evaluate_source` (the shared guarded parse+elaborate+evaluate core, also the capability-file loader's) as a scope-projecting wrapper, sharing a `read_and_normalize` helper with `source` and differing only on scope/return and path policy (`use`'s `RAL_PATH` `find_file` fallback vs `source`'s script-relative resolution).  `Modules` lost its `cache` field (and its `WireModules` mirror) and the now-vestigial `Modules::return_to`.  Added [[decisions/260606_cacheless-module-loader|cacheless-module-loader]]; re-stamped [[map/core/builtins|builtins]] at `466b36f5`; corrected `docs/SPEC.md` §8.  Regression tests in `core/tests/module_loader.rs` pin the cacheless re-`use`, the cycle rejection for both verbs, and the Map-vs-scope-leak split.

## [2026-06-07] ingest | pipeline abort closes gates before reaping

A sandboxed grant with captured stdout exposed a core pipeline teardown cycle:
`range 0 5 | limit 80` starts a ral helper for `range`, then the missing later
external (`limit`) aborts launch before the helper's `StageJob` gate is
released.  The helper can hold an inherited anchor-channel fd while blocked on
the gate; waiting on the anchor or stage handles before closing that gate can
therefore wait on the very helper that is waiting on the parent.

What landed: `PipelineBuild::abort` now consumes the build and signals the pgid;
`PipelineResources` owns the partial-launch resources in safe drop order —
deferred jobs, trailing byte pipe, running handles, then group/anchor.  A helper
that sees job EOF before release exits quietly, so the user sees the real launch
error (`limit: command not found`) without a helper-side diagnostic.  Added the
binary-level regression `grant_pipeline_abort_after_missing_later_stage_does_not_hang`
in `ral/tests/pipeline.rs`, covering both small and large value producers under
the OS sandbox.  Re-verified [[internals/pipeline-execution|pipeline-execution]]
and re-stamped [[map/core/evaluator|evaluator]] at `45f69525`.

## [2026-06-08] ingest | exarch edits by line/hash endpoint witnesses

Exarch's edit surface is one sourced ral helper:
`edit path start-line start-hash end-line end-hash new-text`.  The line number
selects the row; the line hash witnesses the content read at that row; a range is
witnessed by its two endpoints.  Single-line edits repeat the same endpoint
pair, and deletion is replacement with the empty string.  Rust supplies only the
irreducible atoms (`line-hash`, `grep-files`, `explore-dir`); the splice, write,
and `surface` patch emission live in `agent.ral`.

Updated [[design/hash-addressed-editing|hash-addressed-editing]] with Can
Bölük's “The Harness Problem” as the direct hashline reference and a comparison
against exarch's narrower one-verb surface.  Re-stamped
[[map/exarch/builtins|builtins]] and [[map/exarch/shell-eval|shell-eval]] at
`39cb98bb`.

## [2026-06-08] ingest | Esc returns to the prompt without escalating to force-exit

exarch's task-level cancel returned to the prompt but kept escalating ral's
termination counter: each Esc forwarded a synthetic SIGINT into the shell's
handler, whose third `fetch_add` forces `libc::_exit(130)` and leaves the
alt-screen / raw / mouse terminal unrestored.  The missing operation was the
counter's *effect* (unwind at the next `check`) with the cancel-scope's
*discipline* (no escalation).

What landed: `process::interrupt` — a non-escalating `SIGNAL_COUNT.store(1)`,
idempotent — beside `clear`; exarch's unix `deliver_interrupt` drives it instead
of forwarding a signal, and the foreground-external-child SIGINT moved into the
platform layer as `process::interrupt_foreground_child`, so `cancel.rs` holds no
raw libc on this path.  The real-signal `chained` handler is untouched, so a
genuine external `kill -INT` still escalates.  Companion observability: a
transient `Kind::Phase` label (rendering / waiting / typechecking / compacting)
beside the spinner and in the headless `events.json`, and `compact` honours a
turn-boundary cancel before its summarize request.

Filed [[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]];
re-stamped [[map/core/io-process|io-process]], [[map/exarch/frontend|frontend]],
and [[map/exarch/session|session]] at `39cb98bb`.  Tests: core
`interrupt_is_idempotent`, exarch `repeated_interrupt_never_force_exits`.

## [2026-06-08] ingest | one debug path, build-gated with no flags

Collapsed debug tracing to a single primitive.  The `al`→`ral` rename had
left a dead `debug_trace!` macro behind the stale `AL_DEBUG` env var, and a
separate `RAL_DEBUG` flag let release builds opt into the `try`-caught-error
echo.  Now `dbg_trace!` (on in debug builds, compiled out in release, no
flag) is the only path; the `try`-error echo shares the `debug_assertions`
gate; `AL_DEBUG`/`RAL_DEBUG` are gone from the macro, `eval_try`, the binary's
ENVIRONMENT help, and `docs/SPEC.md`.

A companion fix: because debug builds trace unconditionally, a consumer of a
debug ral child's stderr deadlocks unless it drains concurrently — the
per-child wait/drain traces outran a pipe buffer.  `run_with_timeout` in
`ral/tests/pipeline.rs` now reads both pipes on reader threads (collapsing it
with its draining twin `run_args_with_timeout`), fixing
`many_sequential_pipelines_no_leak`.

Filed [[decisions/260608_one-debug-path|one-debug-path]]; re-stamped
[[map/core/diagnostics|diagnostics]] (added the `dbg_trace!` pointer) and
[[map/exarch/shell-eval|shell-eval]] at `e99d86d3`.  Tests: exarch
`many_sequential_pipelines_no_leak` and the full `ral` pipeline suite.

## [2026-06-08] ingest | the edit witness is `h` plus six hex

Fixed an infinite edit loop in exarch's witnessed line-range editing.  A
`line_hash` of all hex digits (≈6% of lines) is also all *decimal* digits, so
when the agent copied a witness out of `view` and typed it back as a bare `edit`
argument, bare-word elaboration (`Val::from_word`) classified it as `Val::Int`
while the hash `edit` recomputes is a `String`; `equal` has no Int×String arm, so
a correct witness was rejected — `"line … has hash 152347, not 152347"`, the two
sides printing identically.  One real run logged 202 identical failing edits.

`line_hash` now prefixes its six hex with the letter `h`, so the witness can
never parse as a number and round-trips as a `String` at every call site; the
comparison logic is untouched.  This also covers the leading-zero digest that a
string-coercing `edit` could not — its integer reading drops the zero before
`edit` ever runs.

Filed [[decisions/260608_witness-hash-h-prefix|witness-hash-h-prefix]] (records
the rejected alternatives: Int×String in `equal`, `to-string` in `edit`,
type-directed literal elaboration); updated
[[design/hash-addressed-editing|hash-addressed-editing]].  Tests: new
`edit_accepts_numeric_witness_hash` in `exarch/src/shell_eval.rs` (all-digit and
leading-zero fixtures, red before the prefix, green after); updated the
`view_tags_lines_with_hash` six-hex assertion to the `h`-tag; full exarch suite
green.

## [2026-06-08] ingest | a step ceiling on the round-trip loop

`Session::apply` gains a hard `MAX_STEPS = 250` ceiling on provider round-trips.
The interactive frontend has Esc to halt a model that never stops emitting tool
calls; headless and autonomous sub-agent runs have nothing, so a benchmark turn
could loop until the token budget or wall-clock ran out.  At the top of the loop,
once the step count would exceed the ceiling, `apply` returns a new terminal
`TurnOutcome::Capped` — surfaced through `note_error` and a `step_cap`
`StopReason` (so the headless JSON `stop_reason` distinguishes a capped run from a
completed one).  `Capped` matches no `nudge` rule, so the driver stops rather than
re-driving into another 250 steps; the mid-protocol log is wound back by
`run_turn`'s existing `ReadyForUser` exit.  The same ceiling bounds sub-agents,
whose reply collapses to `(child stopped: step cap reached)`.

Re-stamped [[map/exarch/session|session]].  Tests: full exarch suite (120) green;
`apply` has no provider mock, so the guard is covered by build + read, not a unit
test.

## [2026-06-08] ingest | tool-result caps fixed; the `--caps` tier flag removed

The model-facing output caps collapse from a two-tier preset to fixed module
constants in `digest.rs`.  The four tool-result sections (stdout/stderr/value/
audit) share `TOOL_RESULT_CAP` (10 KiB, which `head_tail` halves into a ~5000-char
head and tail); `FFF_CAP`, `OPAQUE_CAP`, `AGENT_REPLY_CAP`, and `COMPACT_THRESHOLD`
carry the remaining budgets.  With exactly one possible cap set, the `OutputCaps`
struct, the `CapsTier` enum, and the `--caps small|large` flag amounted to a
runtime carrier for compile-time constants behind a flag whose advertised effect
(whole-file reads inline on the large tier) no longer held — all three are gone,
along with the `output_caps` field threaded through `Session` and its
`output_caps()` accessor.  Uniform tool-result caps also retire `SectionKind`: the
section tag selected nothing once every section shared one cap.

Re-stamped [[map/exarch/session|session]].  Tests: full exarch suite (121) green.

## [2026-06-08] ingest | proposed: a `#\'` near-miss is named at the open, not chased to its symptom

A proposed ADR, no code landed yet — filed ahead of implementation while the
load-bearing reading of "do not change the grammar" is confirmed with the human.

The raw-string delimiter `#'` mistyped as `#\'` opens a comment (the backslash
breaks the delimiter) and silently swallows the line; the failure resurfaces as a
distant [L0001] *unterminated* at a later lone `'`, never at the stray backslash.
The proposal splits the diagnostic on lexical position: in **value position**
(`next_token`'s `#` arm, e.g. `echo #\'…`) a near-miss is a hard [L0004] error
anchored at the open; in **line-leading position** (`scan_separator`'s comment
skip) the run stays a comment but its span is recorded, and a later dangling quote
redirects [L0001]'s secondary label back at it. One new `LexErrorKind::
RawDelimiterEscaped`, one `hash_escape: Option<Span>` on `UnterminatedString`
cleared on every successful string close, one shared near-miss predicate. The one
observable behaviour change — `echo #\'…\'# > f` from silent no-op to error — also
rewrites the Strings section of `exarch/data/ral.md`.

Filed [[decisions/260608_raw-delimiter-near-miss|raw-delimiter-near-miss]]
(*proposed*). No map page re-stamped — the change is not yet implemented.

## [2026-06-10] ingest | the pure pipe equation and the pipeline-edge refactor

A four-parcel refactor of `core/src/evaluator/pipeline/` landed, each parcel the
same move: where two definitions must agree, keep one; where an illegal state is
representable, narrow the type; where a special case guards a race, delete the
special case.  (1) The collector's control variants now carry `Escape`, not
`Break`, so a protocol-layer error can never masquerade as control flow or mask
the first genuine stage failure.  (2) The streaming value edge is gone —
[[decisions/260609_pure-pipe-equation|pure-pipe-equation]]: `x | f = f !{x}`
unconditionally, `pipeline/stream.rs` and the checker's `stream_probe` deleted,
Step consumed by explicit prelude eliminators; two adjacent wire bugs fixed
alongside (consumed-stage output mode; helper-side producer force).  (3) Every
interior edge is allocated once in `open_stage_routes` and its ends moved into
the adjacent stages' routes; a `StageRoute` is consumed whole at spawn, so
leaked or doubly-wired ends are unrepresentable, and a value-carrying stage
never direct-spawns (data-last application needs the helper).  (4) The pgid
anchor is unconditional and child-side `setpgid` failures abort the spawn
loudly.  Modules renamed to the phase names the module doc always used:
`resolve.rs` / `launch.rs` / `stage.rs`; `HelperProtocol` / `HelperStageHandle`
shed the redundant `Ral` prefix.  [[internals/pipeline-execution|pipeline-execution]]
and [[map/core/evaluator|evaluator]] re-stamped; SPEC §20.4, TUTORIAL, and
IMPLEMENTATION updated to the explicit-eliminator surface.

## [2026-06-10] ingest | LoC-audit engine parcels: child-eval unification, evaluator/runtime split, host-embedding API

The `260610_loc_audit_implementation_plan.md` engine parcels landed. Filed three
decisions: [[decisions/260610_child-eval-unification|child-eval-unification]]
(P2 — one re-exec'd-child runner `run_child_eval` + `ChildKind` for the sandbox
and the pipeline stage, replacing two pack→run→report state machines),
[[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]] (P3 —
`command/`, `pipeline/`, `command_call.rs`, `transport.rs` moved to
`core/src/runtime/`; `evaluator/` is the CBPV machine), and
[[decisions/260610_host-embedding-api|host-embedding-api]] (P4 — `BakedPrelude` +
`boot_shell` in `core/src/host.rs`, postcard moved into core). The resolve
parcels landed alongside: the stage-dispatch judgment frozen in resolve as
`StageLaunch`; `Boundary`/`Transport` deleted; the value-edge force consolidated
into one `force_pipe_value` (the runtime mirror of
[[decisions/260609_pure-pipe-equation|pure-pipe-equation]]). [[map/core|core]] and
[[map/core/evaluator|evaluator]] re-stamped. P5 (test table-driving) was dropped;
the generated tree-sitter `parser.c` was untracked.

## [2026-06-10] lint | Full-wiki drift pass after the LoC-audit parcels

Mechanical map lint over every `generated_at_commit`, semantic anchor check
over every `internals/` page, accuracy-and-insight pass over the durable
layers, all against `2df6db85`. Fifteen `map/` pages re-stamped (three
rewritten: [[map/core/runtime|runtime]], [[map/core/capabilities|capabilities]],
[[map/exarch/policy|policy]]); [[internals/pipeline-execution|pipeline-execution]]
rewritten around the resolve-time `StageLaunch` freeze, the single value-edge
judgment, the shared child-eval frame pair, and the `n ≥ 2` anchor; six other
internals pages corrected or re-verified. Durable-layer fixes:
[[invariants/single-binary|single-binary]] now names the real multicall
sentinels; [[design/capability-freeze|capability-freeze]] and
[[design/pipelines|pipelines]] repointed off deleted paths. New ADR:
[[decisions/260610_value-edge-locality|value-edge-locality]]. The lint also
surfaced three stale doc comments in *source* (`types/capability.rs` citing the
deleted `sandbox/ipc/wire.rs`; archeology headers in
`sandbox/windows_restricted_token.rs` and `syntax/ast.rs`), fixed in the same
commit. No contradictions or broken wikilinks found; every durable page has
inbound links.

## [2026-06-11] ingest | exarch edit verb narrows to a single content hash

The `edit` helper in `agent.ral` dropped its range-and-endpoint shape
(`edit path start-line start-hash end-line end-hash new-text`) for
`edit path hash new-text`: it replaces the one line whose `line_hash` matches,
fails unless the hash picks exactly one line, and lets `new-text` carry newlines
(one line becomes several) or be empty (deletion). The line number is gone from
the address — uniqueness of the witnessed content is now the contract, and
multi-line change is composed from one edit per line, each witness staying valid
because the address is content, not position. The persona and language guides
(`system.md`, `ral.md`) were rewritten to teach one-edit-per-line, and the
`edit` doc atom in `agent_builtins.rs` updated. Rewrote
[[design/hash-addressed-editing|hash-addressed-editing]] around hash-only
addressing and re-stamped [[map/exarch/builtins|builtins]]. The `h`-prefix
witness decision [[decisions/260608_witness-hash-h-prefix|witness-hash-h-prefix]]
still holds: `edit` recomputes a `String` hash, so a bare numeric witness would
still fail its `equal` without the prefix. Tests in `shell_eval.rs` updated to
the three-argument call; the repeated-text test now pins the ambiguity rejection
rather than line-number disambiguation.

## [2026-06-11] ingest | exarch edit becomes an atomic batch of hashes

Same day, `edit` went from one line per call to a batch: `edit path edits`, where
`edits` is a list of `[hash, new-text]` pairs. The motive was the cost of
re-reading — a per-call edit re-reads and rewrites the whole file each time, and
two adjacent per-call edits from one read can have the second invalidate the
first's witness. The batch dissolves both: every hash resolves against a single
read *before* anything is written, then all named lines splice in one pass, so the
edits never interfere (adjacent lines included) and the file is rewritten once.
The batch is atomic — a stale hash, an ambiguous hash, or two pairs naming the
same line all fail writing nothing. A single edit is a batch of one. Implemented
in `agent.ral` over `range`/`filter`/`flat-map` (all iterative or TCO-safe, so
large files are unaffected); one `` `patch `` per edit still reaches the rail.
Updated the `edit` doc atom, the persona and language guides (`system.md`,
`ral.md`) to teach one-batch-one-read, [[design/hash-addressed-editing|hash-addressed-editing]]
and [[map/exarch/builtins|builtins]] (re-stamped). Added the
`edit_batch_is_atomic_and_non_interfering` test (poisoned batch leaves the file
untouched; a clean replace/delete/expand batch over adjacent lines applies in one
pass); the other edit tests moved to the batch call shape.

## [2026-06-11] ingest | the edit witness becomes a ±3 context hash, and grep-files moves to the prelude

The line witness now folds in context: `window-hash rows i` is `line-hash` of the
±3 neighbours' `line-hash`es, prefixed by the target's offset within the window.
This distinguishes two identical lines whose surroundings differ — a blank, a
brace, a repeated header — so they are addressable without a line number; only a
line buried in a run of identical lines (whole neighbourhood repeating) stays
ambiguous. The offset prefix is load-bearing for short files: when the file is
≤ ~6 lines the window clamps to the whole of it for several lines, giving them
identical content, and the offset keeps their witnesses distinct (the bug the
content-only first cut had). The batch design makes this safe — every hash
resolves against one snapshot before any write, so a context-dependent witness
can't go stale mid-batch even for adjacent edits; staleness is bounded to a ±3
neighbourhood across batches, which is the re-read we want.

To share the windowing in one place and stop reaching into the Rust grep, the
monolithic `grep-files` atom was split: Rust keeps the fast ignore-aware ripgrep
walk as `search-files <pattern>` → `[{file, line, text}]` (no witness), and
`grep-files` is now an `agent.ral` prelude helper that stamps each hit with
`window-hash`, reading each matched file once. `view` was rewritten from a
streaming `map-lines | cat -n` pipeline to materialise lines and tag each with its
`window-hash`; `edit` resolves against `window-hash`. Rust atoms are now
`{ line-hash, search-files, explore-dir }`; the whole read/edit/grep witness layer
is ral over them. Reworked [[design/hash-addressed-editing|hash-addressed-editing]]
(now *context-hash line editing*) and re-stamped [[map/exarch/builtins|builtins]];
the `h`-prefix decision still holds (`window-hash` ends in a `line-hash`, so it is
always `h`+hex). The `edit_by_hash_rejects_repeated_and_edits_unique` test became
`edit_window_hash_addresses_repeated_lines` (two same-text lines in different
context are now each editable; a deep identical-run line is still rejected); the
numeric-witness regression recomputes the all-digit case against `window-hash`.

## [2026-06-11] ingest | hide the raw search atom as `_search-files`

`search-files` renamed to `_search-files`: the leading underscore hides it from
`help` (and module exports) while leaving it callable, the convention
`core/src/builtins/misc.rs` already uses to keep internal plumbing off the
agent's surface. It is `grep-files`'s engine, not a tool — the agent has
`grep-files` for witnessed search-to-edit and `rg` for pure search, so a third,
witness-less search verb would only be a footgun (search, then fail to edit for
lack of a `hash`). The agent-reachable error path — a bad regex through
`grep-files` — is labelled `grep-files`, since the hidden atom is never called
directly. Removed the `search-files` mention from `ral.md`; updated
[[map/exarch/builtins|builtins]] and [[design/hash-addressed-editing|hash-addressed-editing]].

## [2026-06-11] ingest | authenticate the confinement marker (S1/S2/S8 → A8)

Closed the deep-review's highest-impact security finding: `RAL_SANDBOX_ACTIVE`,
an inheritable public-name env var, single-handedly suppressed the OS layer —
the sole enforcer of `net` and of bundled-coreutils filesystem access — on its
mere presence (S1). The transport gate (`runtime/transport::dispatch`) now trusts
the marker only when `marker::authenticated()` holds: a genuine confined child
adopts a per-re-exec capability token (`sandbox/marker.rs`: 32 bytes of OS
entropy, `/dev/urandom` / `ProcessPrng`) that its parent minted in `run_confined`
and shipped inside the `ChildEvalRequest` IPC frame — a channel a wrapper cannot
write to. `run_confined` also strips the inherited marker from the child env so a
forged value cannot trip `assert_not_already_confined`. `Context::resolver_for_check`
switches to lexical resolution under the *authenticated* marker only, so a forged
value cannot weaken the in-process fs gate. The bundled-coreutils fs floor (S2)
follows: every fs/net-restricting grant body now necessarily runs in the confined
child, where a bundled `cat`/`diff`/… runs under the inherited OS profile exactly
as an external command does — verified end-to-end on macOS (Seatbelt denies a
secret read, allows a granted one; nested `!{…}` dispatches local without a
one-shot re-spawn). Added the S8 adversarial test `core/tests/sandbox_forged_marker.rs`
(fails pre-fix, passes after) and rewrote `sandbox_nested_dispatch_local.rs` to
simulate *genuine* confinement via the `adopt_token_for_test` seam. New decision
[[decisions/260611_authenticated-confinement-marker|authenticated-confinement-marker]];
re-stamped [[internals/capability-enforcement|capability-enforcement]] and
[[map/core/capabilities|capabilities]]. Residual: a nested grant tightening fs
inside an existing Seatbelt child cannot re-tighten the OS profile (one-shot) — a
limitation shared with external commands.

## [2026-06-11] ingest | wire hops are exhaustive, field-complete maps (A7 → R3/R8/R4)

Stated the **exhaustive-map rule** in [[map/core/transport|transport]]: every
wire↔runtime hop is an exhaustive, field-complete map — no hop defaults a field
the wire carries, no kind round-trips through a string with a catch-all. The rule
makes a divergence between confined and local evaluation a build failure rather
than a silent runtime drift. Three hydration seams brought under it:
`install_shell_mobile` now installs a complete `HandlerFrame` through the new
`HandlerStack::push_frame` (R3 — a wire-hydrated alias keeps `removable_by_unalias`,
so `unalias` inside a helper/confined block mirrors local); `WireExecNode.kind`
rides as the serde enum `ExecNodeKind` rather than a string with a defaulting
decode arm, so a new variant fails the build (R8); and `SerialValue::Float`
serialises by IEEE-754 bits, total and exact where JSON coerced NaN/±∞ to `null`
and rejected the decode (R4). Regression tests: `serial::non_finite_floats_round_trip_by_bits`,
`child_eval::wire_exec_node_kind_survives_a_json_round_trip`,
`child_eval::alias_stays_removable_across_the_mobile_wire`, plus the end-to-end
`ral/tests/pipeline.rs::non_finite_float_crosses_a_process_staged_value_edge`.

## [2026-06-12] ingest | exarch panic-recovery for the persistent shell (A4 → X8-panic)

A caught worker panic (`bus::pump`'s `catch_unwind`) left exarch's persistent
`Shell` corrupted: the panicking tool call's skipped restores leaked a grant
frame, hijacked IO, a stale location, the fired watchdog scope, env/cwd, or a
deleted handler into the next turn. New decision
[[decisions/260612_exarch-panic-recovery|exarch-panic-recovery]] records the
chosen story (A4 option (a): poison-on-panic, reconcile, not RAII-everywhere).
Two halves: the per-call IO frame now installs through `shell_eval`'s `IoGuard`
whose `Drop` restores it on unwind; the rest of the dynamic context is rebuilt
from a `durable: Mobile` snapshot the worker refreshes in `Session::run_shell`
right before each eval and `run_turn` reads after `pump` → `Ok(None)`, assigning
`shell.mobile.context = durable.context.clone()` (completed bindings/cwd survive,
the panicking call rolls back). Exposed `ral_core::types::SurfaceSink`. Test:
`session::tests::worker_panic_preserves_completed_bindings_and_clean_context`.
Re-touched [[map/exarch/shell-eval|shell-eval]] and [[map/exarch/session|session]].

## [2026-06-12] ingest | per-root-turn cancellation token (A11 → X5/X8)

Replaced exarch's process-global cancel `AtomicBool` (cleared at the top of
*every* `apply`) with a `Token` minted once per root turn and threaded `apply`
→ dispatch → tools → child sessions; a sub-agent shares the parent's token, so
one Esc cancels the tree. Minting is the reset, so a sub-agent's `apply` no
longer erases a just-pressed Esc (X5). The token's flag is published into a
lock-free `AtomicPtr` slot for the signal handler (a handler must not lock), and
`is_set` reads the same slot so the provider's token-less mid-stream cancel race
observes the same state. `Session::clear` now re-`install`s the chained handler
that `boot_shell` clobbers, so SIGINT after `/clear` still raises cancel (X8).
New decision [[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]];
re-touched [[map/exarch/frontend|frontend]]. `Tool::dispatch` carries the token
(8th arg, allowed). Tests: `cancel::tests::{fresh_mint_does_not_inherit_prior_cancel,
child_token_shares_parent_cancellation, reinstall_after_handler_clobber_restores_the_chain}`,
`session::tests::subagent_apply_honours_a_shared_cancelled_token`. Windows
cross-target (`cargo check --target x86_64-pc-windows-gnu -p exarch`) clean.

## [2026-06-12] ingest | transcript admission invariant for exarch (A12 → X1/X2/X3/X6/X7)

`Session::apply` composed the strict `event.rs` protocol machine on false
assumptions in five places; stated the **transcript-admission invariant** as a
new page [[invariants/transcript-admission|transcript-admission]]: every
committed message serialises to a request every supported provider accepts,
enforced at the `apply` commit boundary. Fixes: a new `admit_assistant` repairs
a non-object tool-call `fn_arguments` to `{}` (X2) and substitutes a stub for an
otherwise-empty assistant message (X7) before `append_assistant`; a `MaxTokens`
reply with captured tool calls now dispatches them and continues the loop rather
than returning `Truncated` into a stranded `AwaitingToolResults` (X6);
auto-compaction moved to the top of `apply` where `can_compact()` actually holds
(it was dead code mid-loop in `AwaitingAssistantAfterToolResults`, X1); and
`provider::parse_4xx_status` now matches a JSON-body `"code": <n>` 4xx as well
as the `status: <n>` token (X3), so an OpenRouter `{"code":400}` classifies as
`Api` not opaque `Other`. The scripted `Reply::empty()` now mirrors the live
zero-part `MessageContent` shape. Re-touched [[map/exarch/session|session]].
Tests: harness `session_apply::{compaction_fires_at_the_threshold,
truncated_with_tool_calls_dispatches_and_continues,
empty_reply_commits_a_stub_not_empty_content,
malformed_tool_arguments_are_normalised_to_object}` plus
`provider::tests::{from_genai_classifies_json_body_4xx_as_api,
parse_4xx_status_excludes_json_429}`.

## [2026-06-12] ingest | exarch usage / stream / char-boundary fixes (X4, X8, X9, X10)

A cluster of smaller exarch corrections. **X4**: the TUI `ctx N%` gauge took
`u.input + cache_creation + cache_read`, but genai's `prompt_tokens` (= `u.input`)
already folds the cache counts in — ~2x on a cache-heavy session, on the one
gauge that signals when to `/compact`; it now reads `u.input` directly. **X9**:
one usage renderer — `Usage::parts` (`UsageParts`) is the single content/layout
source the plain `Display` and the TUI's styled `usage_text` both consume, so the
chrome and the logs cannot diverge; the humaniser is the shared
`humanize_tokens`. Seed-blank filtering moved into `cli::load_seed` (collapses an
empty/whitespace seed to `None`), so the headless and TUI frontends no longer
each re-filter. **X10**: the streaming match is now exhaustive — the reasoning,
thought-signature, and tool-call chunks are dropped by named arms (captured in
the `End` frame), so a new genai stream variant fails the build instead of
vanishing; and `summarize` surfaces a summary that itself hit the 1024-token
budget as `Truncated`, so `Session::compact` keeps the un-summarised history
rather than committing a half summary. **X8 char-boundary**: `parse_4xx_status` /
`parse_retry_after` slice the lowercased copy they search rather than indexing
the original with an offset taken from it (a length-changing lowercase like `İ`
would land mid-character and panic). Re-touched [[map/exarch/provider|provider]]
and [[map/exarch/frontend|frontend]]. Tests:
`provider::tests::{parse_4xx_status_survives_length_changing_lowercase,
parse_retry_after_survives_length_changing_lowercase,
usage_parts_are_the_shared_render_source}`; harness
`session_apply::truncated_summary_preserves_history`. Windows cross-target clean.

## [2026-06-12] lint | deep-review audit: wiki drift swept

Audit of the #260611 deep-review remediation (84 commits) confirmed the code
fixes landed; the residue was wiki prose the review's per-section assessments
had flagged and the remediation never touched. Corrected four claims to match
the code: [[invariants/ir-pure-cbpv|ir-pure-cbpv]] now states that spreads
(`ValListElem::Spread`) and interpolation (`CompKind::Interpolation`) stay in
the IR as runtime-resolved constructors rather than unfolding;
[[design/builtins|builtins]] no longer names a `BuiltinTypeRule::Reducer`
variant (the enum is `Scheme | Sig`; `fold-lines` is a `Scheme` whose modes
come from `reducer_spec`); [[design/codecs|codecs]] now says `from-lines`
materialises whole-buffer behind its stream-shaped interface and only
`fold-lines` streams; [[internals/surface-syntax|surface-syntax]] describes
head classification as a three-stage refinement (parser shape → elaborator
scope resolution → evaluation-time binding/handler/PATH dispatch) instead of
"decided once". Companion code-doc fix: `typecheck/unify.rs` module doc no
longer advertises Unit↔String / Unit↔Bytes coercions (only Record↔Map exists).

## [2026-06-13] migrate | docs/IMPLEMENTATION.md retired into the wiki

The public implementation-notes companion was deleted and its deferral references
in `SPEC.md` / `RATIONALE.md` / `exarch/README.md` rewritten to stand alone
(mechanics are now unpublished; the spec stays normative). Subagents checked its
mechanism sections against the code first — several had gone stale — so only
current facts were carried into [[internals/pipeline-execution|pipeline-execution]]
(Ctrl-Z stop/resume, the load-bearing `tcsetpgrp`-before-`SIGCONT` handoff),
[[map/core/builtins|builtins]] (the worker-thread eval path), [[map/core/elaboration|elaboration]]
(the real Tarjan let-grouping over `group_stmts`, not the doc's stale Kosaraju /
`emit_assignment_group`), and [[map/core/evaluator|evaluator]] (`assign_pattern`
mismatch hints).

## [2026-06-13] ingest | terminal-foreground handoff gated on ownership, not interactivity

A `run-claude.ral`-style script launching `^claude` raised `[R0001]` SIGTTOU: a
non-interactive top-level external now leads its own group (since the timeout
tree-kill change), but the `tcsetpgrp` handoff was gated on `interactive`, so the
script's interactive child was stranded in a background pgroup. Fixed by gating
the handoff on a new cached `TerminalState::startup_foreground`
(`tcgetpgrp(stdin) == getpgrp()`) across both the standalone path
(`for_standalone`, `ForegroundGuard::try_acquire`) and the pipeline path
(`resolve_terminal_plan`); parking on stop decoupled to `want_fg && interactive`.
New [[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]];
touched [[map/core/io-process|io-process]], [[map/core/runtime|runtime]],
[[internals/pipeline-execution|pipeline-execution]].

## [2026-06-13] ingest | terminal foreground restore masks SIGTTOU

A real `claude.ral` launch exposed the release half of the foreground contract:
after a script foregrounds `^claude`, ral is itself a background process group
until it gives the tty back. `ForegroundGuard` now blocks SIGTTOU only around
that restore, while spawned children still get `reset_child_signals`; updated
[[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]],
[[map/core/io-process|io-process]], and the index.

## [2026-06-13] ingest | a turn-boundary prompt queue for the TUI

The TUI REPL now queues a prompt submitted while a turn is in flight and
dispatches it as the next turn's prompt the moment the turn ends, coalescing
the queue oldest-first; the waiting messages render in a strip above the input.
Boundary dispatch keeps the agent/frontend channel one-way rather than
injecting mid-turn. Added [[decisions/260613_prompt-queue|prompt-queue]],
updated [[map/exarch/frontend|frontend]] and the index.

## [2026-06-13] ingest | provider config as a ral script

A design conversation on exarch's unwieldy provider invocation — many positional
parameters, and OpenAI OAuth plus changeable sampling knobs about to widen it
further — reached a proposed direction: the provider/model/tuning surface becomes
a ral script evaluating to a record, read from a project-local `.exarch/config.ral`
(tuning only, run under a no-authority grant and deny-listed against the agent)
layered over the XDG config that is the sole home for auth, `endpoint`, and
defaults. Credentials stay a Rust `Credential` sum type resolved at genai's
per-request `AuthResolver`, so a refreshable OAuth token fits the existing seam.
Added [[decisions/260613_provider-config-ral-script|provider-config-ral-script]]
and the index entry.

## [2026-06-14] query | provider-config ADR revised around a switchable session

A follow-up design conversation traced transcript portability: the stored history
is genai-neutral except for `ContentPart::ReasoningContent`, which the Anthropic
adapter already drops on the wire, so excluding reasoning from the resend set
(`render_messages`) is free and makes the history portable across adapters. That
unlocks a switchable session — one active profile, switch to any other whose
credential is already resolved, same transcript, cache reset expected. Revised
[[decisions/260613_provider-config-ral-script|provider-config-ral-script]] to a
record of named profiles with this switching model, and updated the index entry.

## [2026-06-14] ingest | provider-config ADR: TUI picker, login entry points, eager credential store

Folded the interaction design into the proposal. The TUI has no overlay layer —
it is a flat stack of strips — so the profile picker is a readiness-marked strip
(modal in behaviour, flat in rendering; no shadow), opened by `/model` and
autopopulated by profiles and their credential state rather than an env scan.
Login is one OAuth flow with two entry points (a `login` subcommand for headless
and the picker for interactive), and because the env is scrubbed after startup,
every ready credential is resolved into an in-memory store up front. Revised
[[decisions/260613_provider-config-ral-script|provider-config-ral-script]] and
the index entry.

## [2026-06-14] ingest | provider-config ADR pivots to auto-discovery + live models

The design conversation pivoted away from declared named profiles. New model:
famous providers auto-populate from their conventional env key and fetch their
model list live (genai `all_model_names`, lazy + cached + manual fallback); a
hand-written XDG `config.ral` covers only unusual providers (endpoint + wire
protocol); the active model and tuning are picker-written runtime state in a cwd
`.exarch` (non-authority, deny-listed from the agent), loaded on startup. `/model`
becomes a searchable strip picker with a secondary tuning picker. Reasoning is
never stripped (genai handles it per adapter; DeepSeek/Kimi require the echo).
The named-profiles implementation on branch `provider-config-profiles` is
superseded; reusable patterns (genai client construction, swappable provider,
deny-list, no-authority eval) carry forward. Rewrote
[[decisions/260613_provider-config-ral-script|provider-config-ral-script]] and
the index entry.

## [2026-06-14] ingest | model-selection state moves to the per-project XDG dir

Slice 1 first persisted the model selection to a cwd `.exarch` file (deny-listed
from the agent). Relocated it to `$XDG_STATE_HOME/exarch/<project-slug>/state.json`
— the *same* per-project directory the session logs already use — by extracting a
shared `bootstrap::project_dir(cwd)` (reusing `project_slug`) from `log_run_dir`.
Per-project memory is kept (keyed by the cwd slug) without scattering a dotfile
into the working tree, and because the path is outside cwd the sandboxed agent
cannot reach it — the fs deny-list entry is gone, the protection is structural.
Updated [[decisions/260613_provider-config-ral-script|provider-config-ral-script]]
and the index. Code landed on main (`exarch/src/{bootstrap,state,lib,tui,policy}.rs`).

## [2026-06-14] ingest | hot-loop cancellation implemented

A `grep-files` over the repo hung ~10 minutes deaf to the tool watchdog: its
per-file `filter` ran a value-bodied predicate that never reached an evaluator
poll point. Implemented the long-proposed
[[decisions/260504_hot-path-cancellation|hot-path-cancellation]] plan — `each` /
`map` / `filter` / `fold` / `sort-list-by` now poll `process::check` per element
and `range` polls every 1024 steps — and corrected its analysis (combinators do
*not* poll incidentally via the trampoline; a value-bodied callback escapes every
boundary). Moved the decision proposed → active. Separately fixed the `grep-files`
prelude helper's files×hits quadratic. Code on main
(`core/src/builtins/{collections.rs,..}`, `exarch/data/agent.ral`).

## [2026-06-14] lint | proposed decisions reviewed against the code

Audited the three `proposed` ADRs against current source.
[[decisions/260430_typed-state-flow-wrappers|typed-state-flow-wrappers]] →
*superseded*: the env-key and `current_dir`/PWD concerns it raised are closed by
[[decisions/260531_env-is-dynamic-only|env-is-dynamic-only]] and an audit of the
`std::env::current_dir` call sites, and the `Borrowed`/`Returned`/`Bound`/`Ambient`
wrappers were never built (no type exists in `core/src`). The other two stay
*proposed*, both still genuine open directions:
[[decisions/260603_stateful-handlers|stateful-handlers]] is wholly unimplemented
(no frame state, no surface syntax, no tests), and
[[decisions/260522_repl-architecture|repl-architecture]] keeps its `rustyline`
stream-console substrate — noted there that exarch's TUI now exercises the
Ratatui + Crossterm stack the long-term workbench would use.

## [2026-06-14] ingest | prompt split gets a reusable-script guide

Split exarch's baked prompt into leaner roles: `system.md` now carries basic
agent rules, `ral.md` remains the language card, and the new
`script-style.md` holds reusable-session-script guidance. Updated
[[map/exarch|exarch]] and the index to name the new prompt layer.

## [2026-06-14] ingest | wide review → structural bug prevention

A 33-reviewer pass over core/ral/exarch, every finding adversarially
re-verified against the real code, confirmed 73 defects — the serious ones
collapse into nine recurring shapes. Filed
[[decisions/260614_structural-bug-prevention|structural-bug-prevention]]
(proposed): make each shape unconstructable with a type, lint only as a
backstop at the sanctioned call site. The safe point-fixes landed (49
findings across the front-end, evaluator, runtime, sandbox, REPL, and
exarch); the type-level rewrites are staged follow-ups.

## [2026-06-15] ingest | core's representation stays private to the host

A core review made `ExecPolicy::Subcommands` a `BTreeSet` (the set it always
was, so `meet`/`join` idempotence holds by construction) and added
`admit_label`/`is_denied`, so [[map/exarch|exarch]]'s prompt stops matching
`ExecPolicy`'s variants. Filed
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
and noted the set refinement on
[[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]].

## [2026-06-15] ingest | poll, the non-blocking dual of await

`poll :: ∀α. Handle α → F <pending | ready: {value, …}>` joins the handle
eliminators as the non-blocking dual of `await`; `await`, `poll`, and `race` now
share one settle kernel (`ensure_live` + `try_settle`), and the prelude gains the
`is-done` predicate. Added `poll` to the concurrency family on
[[design/builtins|builtins]], re-ingested [[map/core/builtins|map: builtins]], and
re-stamped [[map/core/typecheck|typecheck]] / [[map/core/prelude|prelude]];
`docs/SPEC.md` §13.3 and §13's eliminator list now name `poll`.

## [2026-06-15] ingest | poll made total over a finished block

A confirm-and-fix pass turned `poll` total: `` `ready `` | `` `failed `` (the await
record minus `value`) | `` `pending ``, never blocking or re-raising, while
`await`/`race` still do. Also fixed a real bug — `try_settle` swallowed a panicked
worker's `Disconnected` receiver, so `poll` reported `` `pending `` forever and
`race` spun; it now settles as the panic failure `await` reports. The failure
buffers are peeked, not drained, and `is-done` became total. Filed
[[decisions/260615_poll-total-failed-arm|poll-total-failed-arm]], updated
[[design/builtins|builtins]] and re-ingested [[map/core/builtins|map: builtins]] /
[[map/core/typecheck|typecheck]] / [[map/core/prelude|prelude]]; rewrote
`docs/SPEC.md` §13.3.

## [2026-06-15] ingest | the handle settle — poll total, await unwrapped

A surface pass refined the concurrency eliminators to one canonical settle:
`{stdout, stderr, outcome: <ok: α | err: ErrRecord>}`. `poll → <pending | settled>`
reports it as data (replacing the sibling `` `ready ``/`` `failed `` arms — the
done/ok split is now orthogonal); `await`/`race` unwrap to `{value, stdout, stderr}`
and re-raise `err`, dropping the always-0 `status` field. Buffers drain once into a
cached `CompletedHandle`; `poll`'s `` `err `` reuses `try`'s `error_record`; `is-done`
is total. Rewrote [[decisions/260615_poll-total-failed-arm|the settle decision]],
[[design/builtins|builtins]], and the [[map/core/builtins|builtins]] /
[[map/core/typecheck|typecheck]] / [[map/core/prelude|prelude]] maps; `docs/SPEC.md`
§13.3 and the exarch `ral.md` prompt now teach the settle shape.

## [2026-06-16] ingest | `/clear` after Esc clears the stale interrupt

`/clear` could panic while rebooting the shell if the previous turn was cancelled:
ral's process interrupt flag was still set, so the embedded exarch agent library
loaded under `boot_shell` aborted as "interrupted". `Session::clear` now clears
that stale interrupt before booting, re-chains the cancel handler before any
fallible log rewrite, and the TUI surfaces clear/redraw errors instead of
discarding them. Re-touched [[map/exarch/session|session]] and
[[map/exarch/frontend|frontend]]; test:
`cancel::tests::clear_discards_stale_ral_interrupt_before_reboot`.

## [2026-06-16] ingest | exarch shell boot owns cancel ceremony

Follow-up to the `/clear` crash fix: `bootstrap::boot_shell` is now the only
session-shell constructor that clears stale ral interrupts before loading the
embedded agent library and re-chains exarch cancel after ral handler install.
`Session::clear` and startup no longer repeat signal ceremony; `boot_root_shell`
only adds scratch/session environment. Re-touched
[[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]],
[[map/exarch|exarch]], [[map/exarch/session|session]], and
[[map/exarch/frontend|frontend]]. Tests:
`cancel::tests::{boot_shell_restores_the_chain_after_handler_clobber,
clear_discards_stale_ral_interrupt_before_reboot}`.

## [2026-06-16] ingest | prompt queue drains at tool boundaries

Queued TUI prompts now steer a running root turn at the next safe tool boundary:
pending tool ids receive real or skipped results first, then the prompt is
committed before the next assistant step. Filed
[[decisions/260616_tool-boundary-steering|tool-boundary-steering]], superseded
[[decisions/260613_prompt-queue|prompt-queue]]'s turn-boundary-only rule and
the in-turn batching slice of
[[decisions/260523_background-tool-calls|background-tool-calls]], then re-touched
[[map/exarch|exarch]], [[map/exarch/session|session]],
[[map/exarch/frontend|frontend]], and [[map/exarch/tools|tools]].

## [2026-06-16] ingest | `!` should eliminate blocks, not functions

A design conversation reached a force-typing decision: the surface `!` should
require a value-producing thunk `U(F α)`, so `!$body` types `body` as a nullary
block and a function-bodied argument fails at the call site rather than silently
returning unrun. Filed
[[decisions/260616_force-eliminates-blocks|force-eliminates-blocks]] (proposed) —
the naive runtime arm is unsound because `step_force` is shared with the
elaborator's `App(Force(Variable), args)` call head, so the two force sites must
first be distinguished.

## [2026-06-16] ingest | verify-before-finish nudge gates expect-action completion

Under `--expect-action`, a clean completion that *acted* now earns one one-shot,
budget-free verify nudge — re-read the output against the task's stated
requirements, a clean exit being evidence the command ran, not that the answer is
correct — as the symmetric complement to the existing idle nudge for tool-less
completions. Re-touched [[map/exarch/session|session]] (the `expect_action`
completion gate). Tests: `nudge::tests::verify_nudges_once_then_accepts`.

## [2026-06-16] query | why records and maps are distinct

Filed a conversation back as [[design/records-and-maps|records-and-maps]]: the
shared `Value::Map` carrier, the `Map<α>` / `Record(Row)` split as the
homogeneous-runtime-key vs heterogeneous-static-label duality, the key space
(bare/quoted/tag → record, deref → map), the `$r[label]` vs `$m[$k]` indexing
rule, the forgetful one-way Record→Map coercion (`unify_map_record`), and the
why-not-collapse argument grounded in `try`/`await`/`audit` records vs
`env`/`from-json` maps. Cross-linked from [[design/types|types]]. Written as a
worked, example-driven tutorial rather than a terse one-claim page (at the
maintainer's request), but kept in the house register — thesis-forward,
PL vocabulary, bullets.

## [2026-06-16] ingest | proposed: unify the two turn evaluators

Filed [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] (status
proposed): lift one top-level turn into `ral_core` as `eval_turn(shell, src,
frame)`, with the frame the missing type over the IO sinks, foreground
`CancelScope`, `Capabilities`, and lifecycle callbacks on which `execute_input`
(REPL) and `run_shell` (exarch) diverge. Records the scope bug — exarch's swap
of `local.cancel` for the per-call watchdog collaterally kills `spawn`/`watch`
workers because `spawn_thread` parents them under `local.cancel` — and the fix:
a durable root scope distinct from the swappable foreground. Recommends
collapsing Ctrl-C and the timeout onto `foreground_scope.cancel()`; leaves the
kill-all gesture and a per-worker death-clock open. Finds the cancel→drain
order factors cleanly inside `RunningChild::observe` (typestate) but is implicit
in exarch's `run_shell` and must become an explicit frame step. Indexed.

## [2026-06-16] ingest | proposed: concurrency primitives, detached vs structured

Filed [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]] (proposed), sibling to [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]: the handle is the evidence of detachment — `spawn`/`watch`/`&` parent at the root, pipeline stages are foreground-bounded, `par` joins detached workers in-expression; `await` unifies onto `race`'s cancel-aware loop, `forget` is recommended for deletion, and a capped lifetime ceiling reaps abandoned detached workers. Reconciled both pages after review: `par` detached throughout, the kill-all gesture resolved (Ctrl-`\` cancels the root, Ctrl-C warns), no introspection primitive for detached handles, and the timeout/ceiling triggers recast as deadlines-as-data behind one shared timer/reaper service.

## [2026-06-16] ingest | review: unify-turn-evaluation corrections

Revised [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] after
review: the foreground scope is always a child of the durable root, pipeline
scope parenting is a required cutover step rather than a current
`RunningPipeline` fact, Unix interactive SIGINT is relay-shaped today, and
`TurnOutcome` now has static and runtime arms with one computed eval status.
Added the root worker registry requirement for Ctrl-C survivor warnings and
updated [[index|index]].

## [2026-06-16] ingest | cause-bearing foreground cancellation

Updated [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] after
the Ctrl-C/Esc design review: scope cancellation now carries
`CancelCause` (`Interrupt`, `Deadline`, `RootAbort`) so Ctrl-C and exarch Esc
share foreground-scope routing without inheriting timeout's SIGTERM→SIGKILL
policy. The [[index|index]] summary now names the cause-bearing cancel shape and
the worker registry / timer service that make it observable.

## [2026-06-16] ingest | refine detached concurrency sequel

Revised [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
after review: `watch` is REPL-only because detached output needs a durable host
sink, `par` stays prelude code over root `spawn` workers rather than gaining a
nursery, pipeline child scope is cutover work rather than a current fact, and
the REPL `JobTable` is separate from `&`/`spawn` handles. Updated
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] so
cause-bearing cancellation includes explicit handle cancellation.

## [2026-06-16] ingest | choose detached worker ceiling

Recorded the exarch detached-worker ceiling in
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]:
ordinary `spawn` gets a ten minute frame-owned lifetime, not a per-spawn knob.
Long-running agent jobs are left as a future explicit promotion/disown-style
mechanism with separate host management, and [[index|index]] now names that
boundary.

## [2026-06-16] ingest | extend detached worker ceiling

Revised [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
after choosing a more patient exarch default: ordinary detached `spawn` now has
a one hour frame-owned lifetime ceiling. The separate long-running-job escape
hatch remains future host-managed work rather than a per-spawn timeout knob, and
[[index|index]] reflects the new bound.

## [2026-06-16] ingest | propose exarch binding leases

Added [[decisions/260616_exarch-binding-reaping|exarch-binding-reaping]]:
exarch may reap stale top-level scratch bindings through core-owned accessors,
using idle age (`last_used_at`) rather than creation age. The ADR keeps Ctrl-`\`
as root cancellation, `/clear` as full shell reset, and records tombstones for
helpful undefined-name diagnostics.

## [2026-06-16] ingest | set binding idle lease

Refined [[decisions/260616_exarch-binding-reaping|exarch-binding-reaping]] with
the first death policy: an unpinned exarch scratch binding dies after one day
since last use or 256 unused ral calls, with a one-boundary grace for fresh or
just-used names. Tombstones last seven days or 1024 ral-tool epochs.

## [2026-06-16] ingest | fix binding lease seams

Revised [[decisions/260616_exarch-binding-reaping|exarch-binding-reaping]] around
Plan C's split: leases live on `Shell::local`, epochs are host-supplied data,
and generation is complete only when every top-level write routes through a
lease-aware shell operation. The ADR now nails the genesis/fork boundary,
unchecked-name read tracking, committed-tool-result reaping site, and
[[index|index]] summary.

## [2026-06-16] ingest | close prune snapshot leak

Clarified [[decisions/260616_exarch-binding-reaping|exarch-binding-reaping]]:
because pruning removes names from the live `Env`, exarch must refresh the
durable `Mobile` snapshot after a committed prune. Without that refresh, the
snapshot could retain large values and resurrect pruned names after an unrelated
worker panic.

## [2026-06-17] ingest | name the turn-local state

Filed [[decisions/260617_turn-local-state|turn-local-state]]: `eval_turn`'s
`FrameGuard` saves and restores nine scattered `Shell` fields by hand, leaking
the membership of "what a turn installs and must restore". The ADR names that
bundle a `TurnLocal` type — `io`+`surface`, the foreground `cancel`, and the
four turn-scoped `Location` cursor fields — so the guard becomes one
`mem::replace`. The crux is splitting `Location` (turn-scoped cursor vs durable
`db` registry, the latter read by both hosts after the turn returns), which
splits `Shell` into `mobile`/`turn`/`durable` and makes the foreground/durable-
root descendant invariant a constructor. Sequel to
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]].

## [2026-06-16] ingest | propose bundled tools as exec images

Filed [[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]]:
bundled coreutils/diffutils/ripgrep commands should be represented as command
images. Clean terminal calls remain inline for Windows latency; every
redirect/capture/env/cwd/pipeline case runs as a ral self-reexec child, leaving
the pipeline helper for evaluator jobs and keeping value-edge bundled heads on
`HelperEval`.

## [2026-06-17] ingest | reshape turn-local state

Rewrote [[decisions/260617_turn-local-state|turn-local-state]] after review:
the proposal now splits `Shell` by lifetime (`mobile` / `turn` / `session` /
`local`) instead of adding a smaller overlay. The ADR folds in the full
non-`db` location cursor, same-thread IO inherit/return semantics, unwind-safe
turn restoration, and visibility boundaries for opaque root/foreground handles
and host-facing accessors.

## [2026-06-17] ingest | place builtins in session state

Refined [[decisions/260617_turn-local-state|turn-local-state]]: the builtin
dispatch table is session/host state, not mobile state. IPC mobiles carry ral
scope/control/context and user handler frames, while receivers preserve their
own booted builtin table; first-class builtin callables cross only as synthesized
ral name-dispatch thunks.

## [2026-06-17] migrate | implement turn-local state split

Landed [[decisions/260617_turn-local-state|turn-local-state]] (now *active*).
`Shell` is four fields by lifetime — `mobile` (public embedding seam) plus
`pub(crate)` `turn` / `session` / `local`. `TurnState` { io, surface, cancel,
loc } is installed by a one-field `TurnGuard` (deleting `SavedIo` and the
field-by-field `FrameGuard`); `Location` split into the turn-local
`LocationCursor` (`turn.loc`) and the session `SourceDb` (`session.sources`);
the builtin table moved onto `session.builtins`, so the wire `Context` and the
sandbox/IPC builtins-preservation steps fell away. `DurableRoot` and
`ForegroundScope` are opaque newtypes, so a turn's foreground is always a
root descendant by construction. Hosts driven through a narrow accessor module;
`mobile` stays public. Whole workspace builds, clippy clean, full suite green.

## [2026-06-17] ingest | refine concurrency detachment ADR

Refined [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
after review: `par` now names the ordinary-error orphan case, root/foreground
text uses the current `session.root` / `turn.cancel` split, and Ctrl-C survival
claims are scoped to foreground-scope interrupts rather than process-global
signals. The `forget` deletion argument now distinguishes dropping a binding
from explicitly discarding a handle's observation channel.

## [2026-06-17] implement | concurrency detachment + the death-clock

Implemented [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]],
flipping it to *active*. `forget` deleted (builtin, `HandleState::Forgotten`,
every arm; `detach_handle` only ever cancels now). `await` recast onto a
`wait_first_settled` loop shared with `race`, so a foreground deadline or
interrupt unwinds the wait while the root-scoped worker survives — the
bare-`recv` hang and the collateral kill gone together. Detached-worker policy
became a frame axis: `DetachedPolicy { lifetime, watch }` rides `TurnFrame` into
`TurnState`, flowing down through `inherit_from` and into workers through
`spawn_thread`; the REPL supplies the interactive policy (no ceiling, `watch`
admitted), exarch the agent policy (one-hour ceiling, `watch` denied). `watch`
now refuses under a frame whose streams are capture buffers.

The death-clock rides a new shared `process::reaper` ("deadlines as data") — one
process-global daemon over a `(when, scope)` heap, the single timer/reaper
service [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] named
but left unbuilt. `spawn` arms its worker as a kept, fire-and-forget entry under
exarch. The reaper gained a disarmable `Deadline` guard, so exarch's 30 s
foreground wall moved off its per-call watchdog *thread* onto the same service,
disarming when a turn finishes early. Both ADRs updated to mark the reaper,
death-clock, and wall migration done. Whole workspace builds, clippy clean,
full suite green.

## [2026-06-17] propose | watch is the REPL's builtin, not a vetoed core one

Proposed [[decisions/260617_watch-repl-builtin|watch-repl-builtin]], a revision
of the `watch`-admission mechanism in
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]].
The principle: a builtin a host cannot run should be *absent* from that host,
not *present-but-vetoed at runtime*. `watch` would move out of `CORE_BUILTINS`
and register only in the REPL via the same `register_builtins` mechanism exarch
uses for its agent tools; its line-framed spawn machinery stays private in core,
so only the `BuiltinEntry` registration moves. With `watch` admission gone from
the frame, `DetachedPolicy` collapses to the per-host lifetime ceiling and
`WatchAdmission` plus the runtime gate are removed — exarch then genuinely lacks
`watch`, a compile-time unknown-name diagnostic rather than a runtime lie.
Status *proposed*; the concurrency-detachment ADR is left *active* and only
linked.

## [2026-06-17] open | long-running work is born, not promoted

Opened [[decisions/260617_long-running-work|long-running-work]], the second
explicit mechanism
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
deferred: how exarch runs work that must outlive the one-hour detached-worker
death-clock. Settled and recorded firmly: such work is *born* durable, not
promoted from an ordinary `spawn` (promotion would need a side registry of
`Deadline` guards just to disarm a ceiling a born-durable worker never arms, and
intent is known at launch); it is a distinct exarch-registered verb reusing the
core-mechanism / host-affordance split from
[[decisions/260617_watch-repl-builtin|watch-repl-builtin]] (not the REPL, whose
`disown` already names POSIX job control); and it births into a *listable*
durable-job registry, pinned against
[[decisions/260616_exarch-binding-reaping|exarch-binding-reaping]] so the model
rediscovers jobs by id after compaction loses the binding name. Left *open* — the
headline question — is which lifetime regime(s) to support: Regime 1 in-process
durable (real `Handle`, small) versus Regime 2 survives-exit (`setsid` process,
no handle). The page recommends Regime 1 now and Regime 2 as separate later work.

## [2026-06-17] query | how a long-running server behaves under capture

Filed back [[internals/output-capture-and-detachment|output-capture-and-detachment]],
the operational narrative an analysis of exarch's slowness-on-servers surfaced.
Capture drains each child's pipe to EOF (`Sink::pump` → `io::copy`, joined by
`WaitedChild::drain`), so a never-closing pipe stalls a foreground command to the
30 s wall, then `PgidPolicy::NewLeader` lets the reaper SIGKILL the whole group.
`spawn` is the escape — root-parented (survives the turn), per-handle buffer
capped at 16 MiB (`SINK_BUFFER_CAP`, dropped past the cap but kept draining so a
chatty server never wedges), one-hour death-clock. Narrates, not re-decides,
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
and [[decisions/260617_watch-repl-builtin|watch-repl-builtin]] (no live `watch`
under exarch → fire-and-`cancel`, redirect for logs).

## [2026-06-17] propose | a wakeup schedules the agent, not a worker

Proposed [[decisions/260617_scheduled-wakeups|scheduled-wakeups]], a cron-like
wakeup for exarch as a long-running agent. A wakeup is a timer that injects a
synthetic user *turn* into the bus `PromptQueue`, re-engaging the agent loop
without a human — it schedules the *agent*, the line that separates it from
[[decisions/260617_long-running-work|long-running-work]] (a born-durable *worker*
yielding a value; here the payload is a prompt, no ral code runs). Settled by the
human: ephemeral and per-session (lives on the `Session`, gone on `/clear`, not
inherited by `fork`). Settled by the code: **no cron** — `process::reaper`'s
substrate is monotonic `Instant`, and `host.rs` shells to `date(1)` to keep
`chrono`/`time` out of the deps, so the trigger vocabulary is `Duration`
(`every`/`after`), not a calendar string. A sibling daemon to the reaper (recurs,
produces a message, host-owned per [[decisions/260617_watch-repl-builtin|watch-repl-builtin]]),
turn-boundary delivery (not [[decisions/260616_tool-boundary-steering|steering]]),
overlap-skip, a listable registry pinned against
[[decisions/260616_exarch-binding-reaping|binding-reaping]]. The one new
mechanism: the idle wait becomes a select over `{input, wake}`. Status
*proposed*; persistence and a durable cron left out of scope.

## [2026-06-17] revise | scheduled-wakeups adopts cron, reusing jiff + the reaper

Revised [[decisions/260617_scheduled-wakeups|scheduled-wakeups]] in place (still
*proposed*, never landed): the *when* is now a **cron expression**, not a bespoke
monotonic-`Duration` DSL. Two facts flipped it. Headless makes a resident exarch
long-lived, so calendar schedules ("weekdays at 09:00") are the common case, not
an edge case; and models are far more fluent in cron than in any mini-language we
could invent — the custom DSL was the actual reinvention. The dependency
objection collapsed on inspection: `jiff` (timezone/DST-aware) is *already
compiled in* as a hard transitive dep via the bundled `date` (`uu_date` →
`jiff-icu` → `jiff`), so cron evaluation reuses it at zero marginal cost.
Reuse-maximal shape: `jiff` evaluates next-occurrence; the five-field grammar is
parsed in-tree (not a `chrono`-based cron crate, which would add a second
datetime tree); the reaper's one action generalises from cancel-a-scope to
`Cancel | Run` so the wakeup rides the existing timer daemon; the `PromptQueue`
and `nudge` path deliver. `after <dur>` survives as the one-shot relative delay
cron cannot express. Everything else stands.

## [2026-06-17] revise | scheduled-wakeups: the delivery seam unifies cron and async workers

Folded into [[decisions/260617_scheduled-wakeups|scheduled-wakeups]] the answer to
"could reaper + cron + future async-agent delivery be one mechanism?" — yes, but
along the *delivery* seam, not a single queue. Decomposing "something happens to
the loop later" into `(trigger, effect, target)` shows the reaper (time → cancel a
scope), a cron wakeup (time → post a message), and a finishing async worker (event
→ post a message) differ on trigger but cron and the worker share effect+target.
So `PromptQueue` (a `VecDeque<String>`) generalises into a typed per-session
**inbox** (source + drain-boundary tags, the inbound twin of the outbound `Kind`
stream); the idle wait selects over `{input, inbox}`; the reaper's `Run` is one
producer, a settling worker another. Built once — cron now, async push later
(turning `spawn → poll` into a pushed *"job finished"* message). Two category
errors kept apart by type: an event-trigger is not a zero-delay timer entry, and a
cancellation (control plane) is not an `InboxMsg` (data plane), per
[[decisions/260614_structural-bug-prevention|structural-bug-prevention]]; a shared
channel is not a uniform drain policy. Flagged the inbox decomposition as a
candidate `design/` page predating the async-worker work it anticipates.

## [2026-06-17] ingest | async agents are a tool delivered through the inbox

Filed [[decisions/260617_async-agent-tool|async-agent-tool]] (*proposed*) from a
design conversation. The want: a sub-agent that talks to the model and does
*unrelated* work off the parent's critical path — the one shape in-turn
concurrency cannot express (the parent turn ending before the child does). The
conclusion deliberately rejects the language route I first reached for: it is not
the born-durable ral verb of [[decisions/260617_long-running-work|long-running-work]]
but an ordinary exarch **tool** whose detached child escapes the dispatch
`thread::scope`, captures an `Arc<Provider>` (the one structural change, already
mapped by [[decisions/260523_background-tool-calls|background-tool-calls]]), and
posts its reply to the per-session **inbox** — the "worker settling" event-trigger
[[decisions/260617_scheduled-wakeups|scheduled-wakeups]] anticipated. Because the
harness owns delivery there is no model-facing handle and no listing-by-id, so the
compaction problem that forces long-running-work to be listable never arises.
Root parenting, the `detached_ceiling`, and muted-to-`SessionLog` output come
unchanged from [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]].
Synchronous stays the default (in-context, no lifetime to manage); async is opt-in.

## [2026-06-17] ingest | watch-repl-builtin lands; DetachedPolicy collapses to a ceiling

Implemented [[decisions/260617_watch-repl-builtin|watch-repl-builtin]] (now
*active*): `watch` left `CORE_BUILTINS` for `core::builtins::WATCH_BUILTIN`, a
public one-entry slice over the still-private `builtin_watch`/`scheme::watch`.
The `ral` host registers it in `register_host_surface` and installs it in both
the REPL session and the batch path (durable stdout in every ral mode); exarch
installs only its agent tools, so `watch` is absent there — out of its builtin
table, `help`, and system prompt. With admission gone from the frame,
`DetachedPolicy { lifetime, watch }` and `WatchAdmission` collapsed to a bare
`detached_ceiling: Option<Duration>` on `TurnFrame`/`TurnState`, and the runtime
gate in `builtin_watch` is deleted. **Corrected the proposal's premise:** ral is
a shell, so naming an absent `watch` is not a compile-time `Static` diagnostic
but an ordinary unknown command (external-command exec → *command not found* at
runtime) — fixed in the ADR, the revised
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
"What shipped", `index.md`, and `docs/SPEC.md` §13.5.

## [2026-06-17] revise | async agent becomes sync/async modes

Revised [[decisions/260617_async-agent-tool|async-agent-tool]] after reviewing the
orchestrator use case. The `agent` tool is now bimodal: `sync` is the dependency
edge that returns the child answer in the current `tool_result`, while `async` is
the orchestration edge that returns an opaque `AgentId` receipt and later pushes a
typed `AgentResult` through the inbox. The revision also fixes the implementation
plan: async needs request-local provider cancellation, an ephemeral listable
registry with `agent_cancel(id)`, explicit reaper arming rather than inherited
`detached_ceiling`, and `/clear` generation rejection for stale deliveries.

## [2026-06-17] revise | steering stays at batch boundaries

Restored same-batch agent overlap while keeping steering protocol-safe.
[[decisions/260616_tool-boundary-steering|tool-boundary-steering]] now states the
current rule: dispatch stages the whole assistant tool-call batch, runs siblings
concurrently where their tool permits it, appends all results, then drains a
queued prompt before the next provider request. Updated
[[decisions/260523_background-tool-calls|background-tool-calls]] to mark the
two-pass staging phase active, and clarified
[[decisions/260617_async-agent-tool|async-agent-tool]] that async is still about a
child outliving the parent turn, not merely same-batch overlap.

## [2026-06-17] ingest | sandbox IPC timeout is parent-owned

Recorded `sandbox-ipc-cancel` (later superseded and removed by
[[decisions/260617_sandbox-external-children|sandbox-external-children]]) after the
exarch session whose `cargo test` crossed the 30 s shell-tool wall and ignored
Esc. The root bug was not simply the Seatbelt `signal children` denial; it was a
parent blocked on synchronous sandbox IPC with no out-of-band watcher for the
foreground `CancelScope`. Updated [[internals/capability-enforcement|capability
enforcement]], [[map/core/capabilities|core capabilities]], and
[[map/exarch/shell-eval|exarch shell-eval]] for the parent-side watcher and the
`Deadline`-only timeout classification.

## [2026-06-17] ingest | test ctors must serve the multicall re-exec flags

`exarch`'s `timeout_kills_sandboxed_subprocess_tree` failed (not skipped):
the exarch test `#[ctor]`s served helper re-execs but never ran
`ral_core::sandbox::early_init`, so `SANDBOX_SELF` was unpinned and the confined
transport was permanently `Unavailable`. Hoisted the test-ctor sandbox tail into
`early_init_or_exit_for_test_ctor` (one place for the success→0/error→1 mapping),
called by `core/tests/common` and exarch's new `serve_test_pre_main`. This is an
operational consequence of [[invariants/single-binary|single-binary]] (the test
binary *is* the multicall executable a child re-execs), recorded as "where" prose
in [[map/core/capabilities|core capabilities]] and [[map/exarch|exarch]] — not a
new invariant.

## [2026-06-17] ingest | grant sandboxes external children

Recorded [[decisions/260617_sandbox-external-children|sandbox-external-children]]
from the re-exec design conversation. The proposed cut keeps sandboxing as the
reason ral exists while moving the ordinary `grant` process boundary to
external-command dispatch: ral-owned filesystem effects use `check_fs_op`,
child-owned effects launch under the effective sandbox projection, bundled tools
become exec images when process semantics are required, pipeline helpers remain
the principled re-exec mode, and endpoint-shaped/advisory network policy is
replaced by offline child confinement or fail-closed unsupported backends.

## [2026-06-18] ingest | cancellation page + restore Ctrl-`\` root abort

Wrote [[internals/cancellation|cancellation]] threading the whole stop-work flow:
the escalating `SIGNAL_COUNT` floor, the cause-bearing `CancelScope` tree, the
signal-safe slots, the `process::check` poll points, the cause-directed external-
child teardown, and the per-host gestures (REPL relay/foreground-cancel/SIGQUIT,
exarch's chained handler and per-turn token). Two code fixes landed with it: an
audit found `boot::setup_signals` re-bound SIGQUIT to `SIG_IGN` right after
`jobs::setup_signals` installed `sigquit_handler`, leaving Ctrl-`\` dead in the
REPL against [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]'s
shipped intent — the override is removed; and `core/src/process/signal.rs` docs
naming a non-existent `RunningPipeline::Drop` are corrected to credit the
foreground/worker scopes and `PipelineGroup::Drop`. [[index|index]] lists the new
page.

## [2026-06-18] ingest | TUI re-encoded as a graphic

Filed [[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]]
from the design conversation on supercharging the TUI. The proposed cut
re-projects the scrollback as an information graphic in Bertin's vocabulary:
the decorative `❖` rail becomes a marginal index (shape→block kind,
hue→agent, value→magnitude), the tab bar becomes a reorderable
agents×steps matrix, `rule_line` gains a `ctx%` value-ramp and a phase
Gantt ribbon, collapsed blocks carry size bars and diff-density grain, and
disclosure becomes graded reduction. The vertical-time log stays the
default projection; the matrix and a codebase map are alternate
projections of the same `Block` buffer. [[index|index]] lists the new page.

## [2026-06-18] migrate | Re-baseline provenance stamps after squash

The `Initial public release` squash rewrote history, so every frontmatter
`*_at_commit` stamp pointing at a pre-release commit no longer resolved and
the drift-lint (`git log <stamp>..HEAD`) errored. Added
`scripts/wiki-restamp.py`, which re-stamps only the dead stamps to the
repository root (`7ba500b`, 2026-06-17) — the honest post-release baseline,
since a squash leaves the working tree unchanged — and leaves the eight
pages with still-resolving stamps untouched. 67 fields across ~38 pages
moved; `--check` now reports zero dead stamps and doubles as a CI guard for
the next rewrite. Inline prose hashes in `decisions/` and this log were left
as-is.

## [2026-06-18] lint | Semantic re-verify of internals/ and related/ after squash

Followed the stamp re-baseline with a real semantic pass over the pages it
reset to root. Confirmed every anchor on the eight flagged `internals/` pages
still resolves in the source, and that no `decisions/` page supersedes the
five `related/` pages' `against` design pages (all eight design pages exist,
none superseded). Re-stamped the twelve confirmed pages to `a590f4f`. One
genuine drift: [[internals/a-turn-end-to-end|a-turn-end-to-end]] still
described the pre-unification two-evaluator spine; re-ingested it to the
single `eval_turn` / `TurnFrame` / `TurnOutcome` model
([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]), noting the
`ral` batch path as the one un-unified entry. The live-stamped `internals/`
pages (cancellation, capability-enforcement, pipeline-execution) were already
current and left untouched.

## [2026-06-18] ingest | tui-transcript-as-graphic, Phases 0–2

Landed the first three parcels of
[[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]]:
the per-`Block` substrate (`AgentSlot`, `magnitude`, `RailShape` chrome
discriminant on `tui/block.rs`), the data-encoding marginal rail (Move 1 —
`tui/rail.rs` encoding shape→kind, hue→agent, value→magnitude, lifted into
`Block::render`), and `rule_line`'s value-ramp `ctx%` bar + Gantt phase
ribbon (Move 3 — spinner dropped, `phase` state moved off `App` onto
`Viewport`). Decision status `proposed → active`; re-stamped
[[map/exarch/frontend|frontend]] to `d8dbd81`. Phases 3–8 remain proposed.

## [2026-06-18] ingest | exarch TUI key algebra collapsed

Collapsed the TUI key surface to quit / close-overlay / active-turn interrupt:
Ctrl-C and Ctrl-D quit only at idle, overlays close, and active-turn Ctrl-C/Esc
drive the per-root-turn token. Removed the TUI kill-all binding and re-aligned
[[internals/cancellation|cancellation]], [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]],
and [[map/exarch/frontend|frontend]] with the smaller key algebra.

## [2026-06-18] ingest | run-turn-host-loop supersedes host-seam-turn-observer

Re-cut the daemon-task hang fix. [[decisions/260618_host-seam-turn-observer|host-seam-turn-observer]]
had the diagnosis right — a detached `spawn` worker holds a clone of the turn's
event `Sender` via the cloned `SurfaceSink`, so exarch's disconnect-gated `drive`
never returns — but its fix (recast `SurfaceSink` to `Rc`, `!Send`) makes `Shell`
`!Send`, which fails to compile against exarch's `pump`/`Session` move
(`session.rs:421`, the `Send`-bounded scoped worker that owns the `Shell`). Verified
the collision against the source, plus the rest of the mechanism (`bus.rs` `drive`,
`inherit.rs:205/214` surface clone, `shell_eval.rs` frame build, the `surface`
builtin at `misc.rs:530`), and that exarch already runs a tokio multi-thread runtime
with `tokio::select!`/`spawn_blocking` (`provider.rs:591/621`).

New decision [[decisions/260618_run-turn-host-loop|run-turn-host-loop]] (proposed):
one synchronous, runtime-agnostic core entry `run_turn(src, &TurnRequest, &dyn
EventSink) -> TurnReport`; the host owns the loop. Completion becomes the turn
task's join future, not the channel's disconnect — so a detached worker holding a
sender clone can no longer hang the turn, `Shell` stays `Send`, and `pump`/channel-
`drive`/`Emitter`-transport are deleted rather than worked around. tokio never
enters `ral_core` (the seam is a sync `EventSink` taking `Value`); the `surface`
builtin is unchanged, only its carrier moves from a stored cloned closure to the
borrowed turn sink. Completes [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]
(REPL + exarch + the `main.rs` batch path become request suppliers) by removing a
thread rather than adding a type. host-seam-turn-observer set to `superseded`.

## [2026-06-18] ingest | run-turn-host-loop review tightened

Revised [[decisions/260618_run-turn-host-loop|run-turn-host-loop]] after review against the current source: the surface carrier is now a concrete turn-local `SurfaceSink` rather than an unplaceable borrow, exarch's first loop carrier preserves the existing borrowed `Session`/`Provider` shape with a scoped worker plus one-shot, and the event bus remains presentation rather than liveness.
Also tightened deferred surface replay, backpressure, lifecycle/IO request fields, the test plan, and the [[index|index]] summary so the proposed implementation no longer relies on `spawn_blocking` ownership or dropped channels.

## [2026-06-18] ingest | turn entry API boundary recorded

Added [[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]] as the companion to [[decisions/260618_run-turn-host-loop|run-turn-host-loop]]. It decides the collapse boundary: public host policy/report types remain (`TurnRequest`, `TurnIo`, `SurfaceSink`, lifecycle, `TurnReport`), while `TurnFrame`, `IoFrame`, core `TurnOutcome`, and public `eval_turn` disappear rather than surviving as a second API.
Updated the run-turn ADR and [[index|index]] to point at the collapse decision.

## [2026-06-18] ingest | after-turn-api simplification draft

Added draft ADR [[decisions/260618_after-turn-api-simplifications|after-turn-api-simplifications]] for the cleanup unlocked after the run-turn API cutover. The draft orders the next simplifications around visibility reduction first: close old core exports, migrate tests to `run_turn`, shrink the exarch ral adapter, share TUI/headless turn driving, narrow host accessors, and only then consider naming cleanup.
Updated [[index|index]] with the open draft and kept the guardrails explicit: `Event`/`Kind` stay, bytes and surface stay separate, `set_stdout` stays until live-printer setup has a replacement, and tokio stays out of core.

## [2026-06-18] ingest | after-turn diagnosis folded in

Folded the architectural diagnosis into [[decisions/260618_after-turn-api-simplifications|after-turn-api-simplifications]]: the common error is turn-local facts leaking into long-lived or host-owned machinery (completion via presentation transport, surface as persistent cloned state, materialised frames as host API). The draft now frames the follow-on cleanup as boundary enforcement rather than a broader redesign.
Updated [[index|index]] to carry the same root-error summary.

## [2026-06-19] ingest | surface carries documents draft

Added proposed ADR [[decisions/260619_surface-carries-documents|surface-carries-documents]]: `surface` should carry a *render document* — an ordered stack of Bertin *marks* the kit composes in ral — interpreted by one generic renderer, replacing the closed `` `patch ``/`` `wrote ``/`` `task ``/`` `meter `` tag set that exarch decodes into a closed `Kind` enum with a bespoke renderer at five Rust sites each. Open card set, closed mark set: the renderer stays total and reflow/disclosure/aggregation/`events.json` survive. Extends [[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]] from chrome to content — the kit declares data and its level of measurement (quantitative → size/value/grain, nominal → hue/shape) and exarch owns the variable binding, so magnitude cannot land on hue. Five marks (`text`/`measure`/`fields`/`diff`/`raw`) plus a `card` container; `raw` keeps the original "just print bytes" instinct as one scoped mark. Core untouched (carries raw `Value` per [[decisions/260618_run-turn-host-loop|run-turn-host-loop]]; detached replay free); `TaskStatus` and the four bespoke `line` builders retire, `provider_error` folds into the shared `fields` renderer. Updated [[index|index]].

## [2026-06-19] ingest | surface I/O event ADR tightened

Revised [[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]] after source review. The exec hook now sits after command resolution at the external/bundled completion doors, write/read events carry path/mode/outcome rather than file-size counts, and exarch keeps a raw `IoEvent` beside the rendered card for `events.json`.
Updated [[index|index]] with the proposed ADR and its enforcement shape: bulk helper I/O sinks below the ral line, while the remaining doors are clippy-checked and outcome-fused.

## [2026-06-19] ingest | terminal lease plan tightened

Revised [[decisions/260619_terminal-lease|terminal-lease]] after source review of the SIGTTIN failure. The plan now parks one unforgeable lease in session state, separates `TerminalAccess` from `TurnStdin`, keeps `JobControl` until only process-group role remains, and treats `_ed-tui` as an explicit host loan rather than a capture exception.
Updated [[index|index]] with the narrowed implementation parcels.

## [2026-06-19] ingest | terminal lease public seam split

Re-evaluated [[decisions/260619_terminal-lease|terminal-lease]] after the follow-up review. The plan now distinguishes the host-facing `RequestedTerminalAccess` from internal `TerminalAccess::ExplicitLoan`, and records that the parked session lease has no raw public getter: foreground handoff code gets a borrow only through an authorised turn.
Updated [[index|index]] to reflect the public/internal terminal-access split.

## [2026-06-20] ingest | terminal lease landed; loan elevation door closed; foreground-ownership superseded

The lease implementation has landed, so [[decisions/260619_terminal-lease|terminal-lease]]
is marked **active**. The `_ed-tui` loan was tightened to match §4:
`begin_terminal_loan` now refuses the `Denied → ExplicitLoan` elevation, raising
only an already-`Leased` turn (`if matches!(prev, TerminalAccess::Leased) { … }`),
so the loan can only *raise* an authorised turn and can no longer mint authority
from `Denied` — closing the deviation the ADR recorded. The loan stays a manual
begin/end token rather than a `Drop`-based RAII guard (a `Drop` impl cannot hold
the `&mut Shell` it needs while the editor body also borrows it); the manual-restore
consequence is acknowledged and intentionally retained. With the lease in place,
[[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]] is
**superseded**: its `startup_foreground` predicate is the lease's mint condition.
Updated [[index|index]]: terminal-lease → active, terminal-foreground-ownership → superseded.

## [2026-06-20] ingest | same-thread body plan tightened

Revised [[decisions/260620_same-thread-body-shares-the-session|same-thread-body-shares-the-session]] after source review. The implementation plan now preserves lambda `$?` entry semantics explicitly, ties pipeline-stage copying to `child_eval` rather than `spawn_thread`, includes `exit_hints` in the `SessionState` inventory, and makes the flow-matrix tests concrete.

## [2026-06-20] ingest | binding reaper plan simplified

Revised [[decisions/260616_exarch-binding-reaping|exarch-binding-reaping]] after design review. The first implementation now uses only ral-tool-call epochs, baseline pins, live-handle pins, static turn read/write sets, and generation-guarded idle pruning; tombstones, retained-size eviction, wall-clock expiry, explicit pin syntax, and callable-specific TTL are deferred.
Updated [[index|index]] with the smaller v1 contract.

## [2026-06-21] ingest | sync/async agent paths studied; bus-lifetime decision filed

Studied the sync/async `agent` asymmetry and unified the incidental duplication in code: one `run_child` + `to_outcome` reduction, `Kind::SubagentDone` now carries `outcome: AgentOutcome`, and the settle breadcrumb is a single `AgentOutcome::breadcrumb` path across [[map/exarch/tools|tools]], [[map/exarch/frontend|frontend]], and headless. Filed [[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]] proposing the remaining, essential change — lifting the per-turn bus to session lifetime so a background child streams a live tab — which answers [[decisions/260617_async-agent-tool|async-agent-tool]]'s deferred "lift the bus" open question and rests on [[decisions/260618_run-turn-host-loop|run-turn-host-loop]]'s completion-as-control-flow-fact invariant. Updated [[index|index]].

## [2026-06-21] ingest | session-lifetime event bus landed

Implemented [[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]] (status → active): a `SessionBus` owns the channel and inbox, borrowed by `pump`/`run_turn`; the TUI mints a session-lived bus so a detached async `agent` streams a live tab, headless keeps a per-turn bus (muted), `drain_pass` latches `done` so a turn ends under a background flood, the idle wait gains the bus as a third source, and `/clear` retires live tabs through the linger window. Re-stamped [[map/exarch/frontend|frontend]], [[map/exarch/session|session]], and [[map/exarch/tools|tools]] (the `PromptQueue`→`Inbox` and sync-only-`agent` descriptions were stale) and updated [[index|index]].

## [2026-06-22] ingest | transcript-as-graphic rework re-ingested into frontend

Re-stamped [[map/exarch/frontend|frontend]] to `@1baac6d` after the [[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]] rework's remaining phases landed (now 0–7). The TUI section now states the two-voice model — human band vs agent field encoded on orthogonal foreground/background planes, machine text washed into a recessed panel and the human prompt fenced as a raised band — the per-tab agent hue (now read from `Viewport::agent`, not per-block), the in-flight reply rendered as a growing magnitude seat, surfaced general cards framed as bounded objects, the synchronized-update bracket and head-anchored tail that steady the frame under streaming tool calls, and the scroll offset mapped onto ratatui's scrollbar range; the `user.log` paragraph notes the `/export` copy now living beside the tee writer as one I/O door. Updated [[index|index]].

## [2026-06-22] ingest | cards re-ingested at HEAD (32 commits stale)

Re-stamped [[map/exarch/cards|cards]] to `@1baac6d` after the
[[decisions/260619_surface-carries-documents|surface-carries-documents]] model
drifted 32 commits. The kit-side prose was the stale part: `edit`, `grep-files`,
and `window-hash` are now Rust host builtins ([[map/exarch/io-surface|io-surface]]),
so `agent.ral` carries only `view`/`view-around`, and the `diff` mark decodes the
`hunks`-list shape `edit` emits (the flat single-hunk form is gone). Also folded
in derived disclosure / aggregation as built (`CardOrigin`, `render_card_framed`,
`absorb_patch`) and de-archeologised the decode prose. `cards` is not listed in
[[index|index]], so index.md was left untouched.

## [2026-06-22] ingest | io-surface re-ingested at HEAD (23 commits stale)

Re-stamped [[map/exarch/io-surface|io-surface]] to `@1baac6d` after the
[[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]] area
drifted 23 commits. The rendering prose was the stale part: a Bertin pass replaced
the `<`/`>` read/write glyphs with dim `Read:`/`Write:` words and lifted
`Role::Path` to cyan, exec args now render as plain ink (not `code` spans), and the
TUI coalesces an interleaved I/O burst into one grouped card per kind via
`io_group_card` (the exec group dropping its per-command status tail). Corrected
the Enforcement reason-tag taxonomy to the actual tags — `[io-door:surface:<slug>]`
/ `[io-door:silent:<slug>]` / `[io-door:test]`. Fixed the orphan defect in
[[index|index]]: both [[map/exarch/io-surface|io-surface]] and
[[map/exarch/cards|cards]], which existed but were missing from the `map/` → exarch
catalog, now have entries stamped `@1baac6d`.

## [2026-06-22] ingest | loop re-ingested at HEAD (21 commits stale)

Re-stamped [[map/repl/loop|loop]] to `@1baac6d` after the run-turn cutover and
the structural-frontend series drifted it 21 commits. The "One input" prose was
the stale part: a REPL turn no longer calls `compile_and_typecheck` /
`eval_top_level` itself — `execute_input` builds a `TurnRequest` and enters core
through the framed `run_source_turn` door, matching one flat `TurnReport`, with
prompt thunks, rc startup, and plugin hooks entering through `run_value_turn`
under `Denied` ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]],
[[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]],
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]],
[[decisions/260617_turn-local-state|turn-local-state]]). Added the selectable
`Frontend` (minimal / readline / structural, `--surface` and rc `surface:`),
linking the structural surface out to [[map/repl/frontend|frontend]], and folded
the SIGQUIT root-abort, the `set_stdout` printer wiring, and the `surface:` rc
key into the page. Updated [[index|index]].

## [2026-06-22] ingest | shell-state re-ingested at HEAD (19 commits stale)

Re-stamped [[map/core/shell-state|shell-state]] to `@1baac6d` after the run-turn
cutover and the terminal-lease / same-thread-body series drifted it 19 commits.
The `Shell` is now split by lifetime into four fields — `Mobile`, `TurnState`,
`SessionState`, `LocalState` — so the page records where each datum lives: the
`surface` sink and `TerminalAccess` are turn-local, the `terminal_lease` and
durable cancel root are session-durable, and audit / REPL scratch are local
([[decisions/260617_turn-local-state|turn-local-state]],
[[decisions/260619_terminal-lease|terminal-lease]]). Added that a same-thread
thunk body runs in the caller's session by swapping only `Mobile`
([[decisions/260620_same-thread-body-shares-the-session|same-thread-body-shares-the-session]]),
that handler and alias arms are lambdas with a fixed `HandlerArity`
([[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]),
and the host-embedding accessors (`host.rs`, `TerminalLoan`). Updated
[[index|index]].

## [2026-06-22] ingest | exarch hub re-ingested at HEAD (15 commits stale)

Re-stamped [[map/exarch|exarch]] to `@1baac6d`, refocusing the hub on the
binary's front door: pre-`main` dispatch normalised to `Option<u8>` and shared
by `main` and every test ctor, the `login`/`logout`/`accounts` subcommands now
that several ChatGPT accounts are each a selectable provider, bootstrap's
machine probing through the renamed `ral_core::driver::boot_shell` (with
`ral_core::host` owning the probes), and the scratch / run-log dirs. Rewrote the
system-prompt section around the assembly order (persona, `Grant`, `Host`,
`Ral`, `Script style`, `Headless`), reframed around definitions rather than
bindings, the surface render-document model
([[decisions/260619_surface-carries-documents|surface-carries-documents]]),
lambda-only handlers
([[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]),
and the `--allow-schedule` scheduled-wakeups affordance
([[decisions/260617_scheduled-wakeups|scheduled-wakeups]]). Updated
[[index|index]].

## [2026-06-22] ingest | runtime re-ingested at HEAD (14 commits stale)

Re-stamped [[map/core/runtime|runtime]] to `@1baac6d`. Confinement moved off
`transport::dispatch` — a grant body now always evaluates locally
([[decisions/260610_value-edge-locality|value-edge-locality]]) and the OS
sandbox is entered per-command at external dispatch in `command::build_command`
([[decisions/260617_sandbox-external-children|sandbox-external-children]]).
Bundled coreutils/ripgrep heads run as `--ral-bundled-tool` exec images
whenever process semantics are required, keeping the inline `uumain` placement
only for a clean terminal and serialising the uucore exit-code cell across
threads ([[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]]);
redirect reads/writes and exec completions now surface at runtime I/O doors
(`command/io_event.rs`), with the event shapes and card rendering in
[[map/exarch/io-surface|io-surface]]
([[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]).
Corrected the module map (`command.rs` owns the External arm beside the
`command_call.rs` dispatcher; `reexec_child_shell` lives in `subprocess.rs` and
is driven by `child_eval.rs`) and recorded lambda-only handlers/aliases
([[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]).
Updated [[index|index]].

## [2026-06-22] ingest | startup re-ingested at HEAD (11 commits stale)

Re-stamped [[map/repl/startup|startup]] to `@1baac6d`. Batch execution no longer
calls `eval_top_level`: `run_batch` now enters core through the framed
`Shell::run_source_turn` door, scoring its two-armed `TurnReport`
([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]],
[[decisions/260618_run-turn-host-loop|run-turn-host-loop]],
[[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]]), and the
foreground handoff is gated on a held `TerminalLease` via the request's
`RequestedTerminalAccess` ([[decisions/260619_terminal-lease|terminal-lease]]).
The interactive frontend is chosen by `--surface` (not `RAL_SURFACE`), the argv
terminator's value-flag set is derived from clap, the prelude/Shell-embedding API
moved `host` → `driver` (machine probing split into `core::host`), and the
pre-clap chain gained confined-child tails (`--ral-sandbox-exec`,
`--ral-bundled-tool`) normalised to `u8`. Updated [[index|index]].

## [2026-06-22] ingest | capabilities re-ingested at HEAD (10 commits stale)

Re-stamped [[map/core/capabilities|capabilities]] to `@1baac6d`. The OS sandbox's
platform reality is sharpened: macOS-only re-exec items (`--ral-sandbox-exec`,
`verify_unswapped`) are now `cfg(target_os = "macos")`-gated, and Windows fails
closed at `maybe_enter_process_sandbox` — a requested policy it cannot enforce
errors rather than running unconfined
([[decisions/260617_sandbox-external-children|sandbox-external-children]]). Added
the `diag.rs` kernel-denial hint, where only `file-*` denials yield a path to
grant (ipc/mach/network operands reproduce verbatim but never fill the
path-to-grant slot), and noted that the layer's `fs`/process constructors are
clippy-enforced I/O doors whose shapes render through
[[map/exarch/io-surface|io-surface]]. The XDG resolver
([[decisions/260601_xdg-resolver-consolidation|xdg-resolver-consolidation]]) and
the decision-fold structure are unchanged. Updated [[index|index]].

## [2026-06-22] ingest | shell-eval re-ingested at HEAD (10 commits stale)

Re-stamped [[map/exarch/shell-eval|shell-eval]] to `@1baac6d` after the turn-door
cutover ([[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]],
[[decisions/260618_run-turn-host-loop|run-turn-host-loop]]): `run_shell` is now a
request supplier that enters core through the single source-text framed door
`Shell::run_source_turn`, the only way into evaluation. Added the two new
`TurnRequest` fields the migration carries — `terminal:
RequestedTerminalAccess::Denied` paired with `stdin: TurnStdin::Empty`, so a tool
turn holds no terminal lease and its foreground handoff is unrepresentable
([[decisions/260619_terminal-lease|terminal-lease]]) — and reframed the surface
sink around the `` `card `` render document core now carries
([[decisions/260619_surface-carries-documents|surface-carries-documents]]),
linking the mark set out to [[map/exarch/cards|cards]] and the io-door shapes to
[[map/exarch/io-surface|io-surface]]. Dropped the dead `exarch/src/sandbox_diag.rs`
/ `sandbox_diag/` `covers_paths` and the page's exarch-owned `sandbox_diag`
paragraph: that diagnostic moved into `core::sandbox::diag`, now owned by
[[map/core/capabilities|capabilities]]; the page links out to it. Updated
[[index|index]].

## [2026-06-22] ingest | provider re-ingested at HEAD (8 commits stale)

Re-stamped [[map/exarch/provider|provider]] to `@1baac6d`. Rewrote the provider
model around the three-armed `ProviderId` — auto-discovered famous providers, an
unusual-provider `config.ral` with a custom endpoint + wire protocol
([[decisions/260613_provider-config-ral-script|provider-config-ral-script]]), and
each signed-in ChatGPT account as its own selectable OAuth identity — and named
the two unmetered axes (opencode Go's flat rate vs an OAuth login). Recorded the
per-event idle timeout that now bounds both the streaming loop and the
non-streaming summary, the request-local cancellation seam of
[[decisions/260617_async-agent-tool|async-agent-tool]] (registry/inbox linked out
to [[map/exarch/tools|tools]] / [[map/exarch/session|session]]), the unified
`humanize_tokens` formatter, and that error classification is now structural off
genai's typed variants, carrying the parsed JSON body to the boundary for the
[[map/exarch/cards|cards]] renderer. Updated [[index|index]].

## [2026-06-22] ingest | evaluator re-ingested at HEAD (8 commits stale)

Re-stamped [[map/core/evaluator|evaluator]] to `@1baac6d`. The boundary is now
two crate-private verbs, not three public ones: `eval_top_level` is `pub(crate)`
and reached only through the framed `Shell::run_source_turn` door
([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]), and `apply`
dropped from a listed boundary verb to a `pub(crate)` reduction host. Recorded
that a same-thread thunk body — block force or lambda apply — now evaluates in
the caller's session via the shared `with_thunk_body`, swapping only a rescoped
mobile ([[decisions/260620_same-thread-body-shares-the-session|same-thread-body-shares-the-session]],
the `Value` split kept by [[decisions/260616_force-eliminates-blocks|force-eliminates-blocks]]),
that list destructuring without a `...rest` tail now rejects a longer value, and
that `within` handlers and aliases must be fixed-arity lambdas validated at the
install boundary ([[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]).
Updated [[index|index]].

## [2026-06-22] ingest | IO/process/stream re-ingested at HEAD (8 commits stale)

Re-stamped [[map/core/io-process|io-process]] to `@1baac6d`. The foreground
handoff is now gated on a held `TerminalLease` value (new `core/src/process/lease.rs`,
lent to `ForegroundGuard::try_acquire`) rather than the inferred `startup_foreground`
predicate, and the former `JobControl` is the placement-only `LaunchRole`
([[decisions/260619_terminal-lease|terminal-lease]]); recorded the new no-input
`Source::Empty`. Folded in the single reaper daemon whose action generalised to
`Cancel | Run`, so death-clock deadlines, scheduled wakeups, and detached-worker
ceilings share one timer ([[decisions/260617_scheduled-wakeups|scheduled-wakeups]],
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-primitives]]),
and noted that redirect reads/writes emit byte-level I/O doors at this layer,
rendered by [[map/exarch/io-surface|io-surface]]. Updated [[index|index]].

## [2026-06-22] ingest | core hub re-ingested at HEAD (8 commits stale)

Re-stamped [[map/core|core]] to `@1baac6d`. Reframed the crate root around its
two narrow seams: the compile-to-typed-IR ladder (`compile` /
`compile_and_typecheck`), and the framed turn doors `Shell::run_source_turn` /
`run_value_turn` as the only entry into evaluation — the reduction primitive
behind them is crate-private, and completion is the call returning, not a
channel closing ([[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]],
[[decisions/260618_run-turn-host-loop|run-turn-host-loop]]). Recorded the new
`host` (machine probing) / `driver` (Shell embedding, `boot_shell`,
`BakedPrelude`, the single prelude-bake site) split
([[decisions/260610_host-embedding-api|host-embedding-api]],
[[decisions/260610_evaluator-runtime-split|evaluator-runtime-split]]). Updated [[index|index]].

## [2026-06-22] ingest | repl hub re-ingested at HEAD (8 commits stale)

Re-stamped [[map/repl|repl]] to `@1baac6d`. Reframed the `ral` binary's hub
around its thesis: argv dispatch into a turn through core's framed door,
driving one of three selectable frontends — minimal, readline (default), and
structural, a default-on ratatui projection of live program state selected by
`--surface` ([[decisions/260522_repl-architecture|repl-architecture]]). Recorded
that every evaluation, batch or interactive, now enters core through the same
framed turn door as one synchronous policy-carrying call
([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]); the structural
surface's internals (typed spine, reactive worksheet, handles matrix, vi-mode,
shared fuzzy completion + Tab menu) stay pointed at [[map/repl/frontend|frontend]].
Updated [[index|index]].

## [2026-06-22] ingest | structural surface gains the plugin surface and shell line-edit

Re-stamped [[map/repl/frontend|frontend]] to `@1baac6d` for six commits of
refinement to the structural surface ([[decisions/260620_repl-as-structural-surface|repl-as-structural-surface]]).
Recorded that it now drives the same in-editor plugin surface as the rustyline
backend off the shared `PluginRuntime` — fish-style ghost text, highlight cell
overlays, and fzf/zoxide keybindings, with the runtime itself living in
[[map/repl/plugins|plugins]] — and that `shell_line_edit` remaps Ctrl-U to
readline's kill-to-line-start in emacs and vi-Insert mode. The shared fuzzy
completion engine and the native-cursor-every-mode editing
([[decisions/260522_repl-architecture|repl-architecture]]) were already on the
page; confirmed both against the code. Updated [[index|index]].

## [2026-06-22] ingest | plugin hooks gain source-mapped faults and a buffer-change breaker

Re-stamped [[map/repl/plugins|plugins]] to `@1baac6d` for six commits of drift.
Recorded that framed hooks (keybinding, buffer-change, prompt) now evaluate
through the value turn door ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]),
that a hook fault is source-mapped against the plugin file's own text and
rendered inside `call_plugin_hook` while its registry is still live, and that a
buffer-change hook is braked by a per-plugin `HookHealth` circuit breaker —
three faults in a row or one overrun of the keystroke budget disables it for the
session. Reframed the page around the runtime that drives the in-editor plugin
surface both frontends render, pointing the rendering at
[[map/repl/frontend|frontend]] and the host boundary at
[[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]].
Updated [[index|index]].

## [2026-06-22] ingest | edit becomes a Rust atom building a similar whole-file diff card

Re-stamped [[map/exarch/builtins|builtins]] to `@1baac6d` for four commits of
drift. `edit`, `grep-files`, and `window-hash` are now Rust host builtins reading
below the redirect frame, so the bulk helper I/O surfaces once at the I/O doors
([[map/exarch/io-surface|io-surface]]) rather than as per-file read/write cards;
`agent.ral` keeps only `_rows`/`view`/`view-around`. `edit` now builds one
whole-file line-level `diff` card grouped into hunks by the `similar` crate and
hands it to the core `surface` builtin ([[map/exarch/cards|cards]]), the dormant
tag-set helpers gone ([[decisions/260619_surface-carries-documents|surface-carries-documents]],
[[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]],
[[decisions/260608_witness-hash-h-prefix|witness-hash-h-prefix]]). Updated
[[index|index]].

## [2026-06-22] ingest | the checker retains per-stage pipeline value types and types handler/alias arms as lambdas

Re-stamped [[map/core/typecheck|typecheck]] to `@1baac6d` for two commits of
drift. The annotation pass now writes a resolved value type per pipeline stage
onto the `Pipeline` node alongside its ground wires — the data flowing between
stages, kept for the structural REPL's typed spine, with no new inference
([[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]]). A handler
or alias arm is a fixed-arity lambda whose calling convention is fixed by the
surface form rather than sniffed from the value, so `alias_arm_scheme`'s `param`
is non-optional; the static layer still types a non-`Lam` thunk by its bare body,
leaving the runtime install as the sole gate on shape
([[invariants/fixed-arity|fixed-arity]],
[[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]).
Updated [[index|index]].

## [2026-06-22] ingest | one shared depth cap now bounds every recursive production in the syntax stage

Re-stamped [[map/core/syntax|syntax]] to `@1baac6d` for three commits of drift.
Pattern and unary-operator recursion now pass through the shared
`NESTING_DEPTH_LIMIT`: the parser's three mutually-recursive sub-grammars each
descend through one guarded chokepoint (`parse_primary`, `parse_expr_atom`,
`parse_pattern`), so adversarial nesting rejects cleanly rather than overflowing
the stack. The parser also gives a friendlier error when a digit is glued to a
comparison operator inside `$[…]` (`$[2>3]`, lexed as the redirect `2>`), and the
lexer's `\<newline>` continuation no longer stretches an adjacent literal's span.
Updated [[index|index]].

## [2026-06-22] ingest | transport carries a per-name handler's convention by construction

Re-stamped [[map/core/transport|transport]] to `@1baac6d` for three commits of
drift. The one genuine change to what the page asserts: a wire-hydrated per-name
handler entry now takes its calling convention as `HandlerArity::Unary` by
construction rather than re-sniffing it from the thunk's runtime shape — the
values already cleared install-time arity validation on the sender, so hydration
trusts that and does not re-check
([[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]).
The serde mirror's other churn was doc-comment renames and the codec gained only
`#[allow]` markers on its post-mortem `dump_frame` door — below this page's
altitude, no prose change. Updated [[index|index]].

## [2026-06-22] ingest | the pipeline node carries a typed spine

Re-stamped [[map/core/ir|ir]] to `@1baac6d` for one commit of drift. The
`CompKind::Pipeline` node gained `stage_types: Vec<Ty>` — one inferred value type
per stage, parallel to `stages`/`wires` — emitted as a `Unit` placeholder by the
elaborator and overwritten by the annotation pass, retained for the structural
REPL's typed spine ([[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]]).
Updated [[index|index]].

## [2026-06-22] ingest | fg resume is gated on a held terminal lease

Re-stamped [[map/repl/jobs|jobs]] to `@1baac6d` for one behaviour commit (the
other was a `cargo fmt` sweep). On Unix, `fg` now hands the controlling terminal
to a parked group only when the turn holds a `TerminalLease`
([[decisions/260619_terminal-lease|terminal-lease]]) — `wait_foreground` asks the
session for the borrow rather than re-deriving a `startup_foreground` predicate,
and a non-interactive resume with no lease skips the tty handoff. Updated
[[index|index]].

## [2026-06-22] ingest | elaborator emits a Unit placeholder per pipeline stage value type

Re-ingested [[map/core/elaboration|elaboration]] for `3cd6d84`: the elaborator now seeds the `Pipeline` node's `stage_types` with a `Ty::Unit` per stage, parallel to the existing `Wire::EMPTY` mode-wire placeholders, which the checker's annotation pass overwrites with each stage's resolved value type. Extends [[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]] — the same elaborator-placeholder / checker-overwrite split now carries the per-stage value types the structural REPL's typed spine needs.

## [2026-06-22] ingest | diagnostics exposes the type-error underline ingredients

Re-ingested [[map/core/diagnostics|diagnostics]] after `1a4cdb9` promoted `byte_to_char` and `label_message_for_kind` to `pub`, so an external renderer (the structural [[map/repl/frontend|frontend]]) can draw an in-place type-error underline that agrees word-for-word and column-for-column with the post-Enter ariadne report. The companion `0637d08` was a test-only ANSI-stripping change, no behaviour.

## [2026-06-22] ingest | ral-sh exec sites become silent io-doors

The login-shell bridge's two re-exec call sites (`exec_ral`, `exec_posix_sh`) now carry `[io-door:silent:respawn-…]` reasoned allows under the workspace door discipline; behaviour is unchanged. Re-stamped [[map/ral-sh|ral-sh]] to `1baac6d` and corrected its dispatch description to the four-rule first-match-wins matrix.

## [2026-06-22] ingest | re-verify evaluator-machine at HEAD

Re-ingested [[internals/evaluator-machine|evaluator-machine]] against the run-turn cutover and the Shell lifetime split: its `eval_top_level` / `apply` verbs are now `pub(crate)`, reached only through framed turn doors ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]), and the old two-way Mobile/Local split is now the four-way `Mobile` / `TurnState` / `SessionState` / `LocalState` partition ([[decisions/260617_turn-local-state|turn-local-state]]). Added the same-thread β-step that runs a thunk body in the caller's session via `with_thunk_body` rather than a copy ([[decisions/260620_same-thread-body-shares-the-session|same-thread-body-shares-the-session]]); bumped the verified stamp to `1baac6d`.

## [2026-06-22] ingest | a turn runs through the framed door, not the eval_top_level spine

Re-verified [[internals/a-turn-end-to-end|a-turn-end-to-end]] against the run-turn cutover: the single `run_turn` entry is now two synchronous doors, `Shell::run_source_turn`/`run_value_turn` returning a `TurnReport`, sharing one `build_turn`/`run_framed` scaffold while `eval_top_level` and the old spine go crate-private ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]], [[decisions/260618_run-turn-host-loop|run-turn-host-loop]], [[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]]). Dropped the vanished `run_turn`/`run_compiled` anchors, added `run_source_turn`/`run_value_turn`/`run_framed`, and bumped the verified stamp to `1baac6d` / 2026-06-22.

## [2026-06-22] ingest | exarch policy: verify + re-stamp, deny-paths fix

Re-ingested [[map/exarch/policy|policy]] at `1baac6d`: `policy.rs`/`policy/` are substantively unchanged (a test-only clippy `#[allow]` + rustfmt), so this is a faithful verify. Corrected the `fs.deny_paths` claim to lexical-only (core expands canonical/firmlink), dropped the duplicated `prompt.rs`/`data/` prompt-assembly (now owned by [[map/exarch|exarch]]), and trimmed `covers_paths` accordingly.

## [2026-06-22] ingest | builtins: bundled tools span three families, heads are exec images

Re-ingested [[map/core/builtins|builtins]] to HEAD: bundled tools are now coreutils + diffutils + ripgrep unified under `uutils_invoke`/`is_uutils_tool`, and the bundled heads are resolved command images rather than in-process builtins, with the `ral --ral-bundled-tool` dispatch delegated to [[map/core/runtime|runtime]] per [[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]].

## [2026-06-22] ingest | pipeline-execution: foreground handoff now gated on the held terminal lease

Re-verified [[internals/pipeline-execution|pipeline-execution]] to HEAD: the `startup_foreground` predicate has vanished as the foreground gate — the launch-time and resume-time `ForegroundGuard` are now acquired only against a held `TerminalLease` (`try_acquire(target, &TerminalLease)`), so the terminal plan and the guard ask one authority ([[decisions/260619_terminal-lease|terminal-lease]]). Confirmed the value-edge judgment ([[decisions/260610_value-edge-locality|value-edge-locality]]) and the shared `run_child_eval` frame pair ([[decisions/260610_child-eval-unification|child-eval-unification]]) still hold; dropped `startup_foreground`, added `TerminalLease`/`terminal_lease` to anchors, and bumped the stamp to `1baac6d`.

## [2026-06-22] ingest | capability-enforcement re-verified at HEAD

Re-stamped [[internals/capability-enforcement|capability-enforcement]] to 1baac6d after confirming all eight anchors survive and the grant-body-evaluates-locally / per-command OS sandbox flow of [[decisions/260617_sandbox-external-children|sandbox-external-children]] holds, with Windows fail-closed. Added the missing [[design/two-enforcers|two-enforcers]] link; the authenticated-confinement marker was removed with the reexec machinery and is correctly absent rather than tombstoned.

## [2026-06-22] ingest | compilation-ladder re-verified to HEAD

Re-verified [[internals/compilation-ladder|compilation-ladder]] against the host/driver split, relocating the prelude bake's encode/decode pin (`bake_prelude_to_out_dir`/`BakedPrelude`) from `host.rs` to `core/src/driver.rs` per [[decisions/260610_host-embedding-api|host-embedding-api]] and recording `annotate`'s new per-stage `stage_types` verdict alongside the ground `Wire`s ([[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]]). Added the run-turn cutover note that the evaluator is reached only through the framed turn doors ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]); all anchors confirmed and the verified-stamp bumped to 1baac6d.

## [2026-06-22] ingest | surface recognised as multi-purpose; spawn-notify decision filed

Studied ral `spawn`'s pull-only delivery and the model's three failure modes (poll, forget to `await`, assume an absent notification). Traced the seam: the one `surface` `EventSink` (`AgentSink`, `exarch/src/shell_eval.rs`), the `Emitter`'s `session_lived` flag and silent `let _ = tx.send` (so the withheld channel guards presentation *placement*, not liveness), and the session-lived inbox the async `agent` already posts to. Filed [[decisions/260622_surface-carries-control|surface-carries-control]] (proposed): recognises `surface` as the language→host typed-`Value` channel of which presentation is one class, and adds a non-rendering `` `spawn-started `` *control* event — surfaced **in-turn** and carrying a live `Value::Handle` — so exarch registers the handle and arms an inbox-posting waiter, letting `spawn` **notify** (`SpawnResult` → `Turn::Spawn`, generation-gated like `AgentResult`) instead of being **polled**. The design keeps one channel and gives the detached worker none — its closure is unchanged — upholding the detachment-holds-only-root/handle-resources invariant by construction; an earlier draft that handed the worker the `Emitter` is recorded as the rejected unsafe variant. Extends [[decisions/260619_surface-carries-documents|surface-carries-documents]] and [[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]] (*render* vs *control*, one step beyond *operation* vs *appearance*), refines [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]'s poll idiom and the poll-instructing inline-timeout text, and partly answers [[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]'s background-surfacing open question (control class only). Flagged the one shell-free handle settle and its two-observer cache handoff as the sole concurrency risk. Updated [[index|index]].

## [2026-06-22] ingest | spawn-notify counter-proposal filed: surface/notify split by lifetime

Reviewed [[decisions/260622_surface-carries-control|surface-carries-control]] and filed [[decisions/260622_sync-surface-async-notify|sync-surface-async-notify]] (proposed) as a cheaper alternative. The pivot of the review: the sibling routes a *post-turn* need (`spawn` should notify) through the *in-turn* `surface` channel, and pays for the lifetime mismatch in machinery — a non-rendering `` `spawn-started `` control class, an exarch waiter thread, a new shell-free core settle, and the atomic two-observer cache handoff it names as its sole risk. The counter-proposal recognises that the one property a value's tag can never carry is **lifetime**, so it is the one axis that earns a structural split: two channels, `surface` (sync, in-turn, holds the [[decisions/260619_terminal-lease|terminal-lease]]) and `notify` (async, post-turn, boundary-rendered as a fresh turn), each a typed-`Value` channel dispatched *by class*. `spawn`'s detached worker emits a structural `` `spawn-done `` through a session-lived `notify` at its existing `tx.send` settle point (`core/src/builtins/concurrency.rs:145`); exarch's `InboxNotify` pushes it onto the inbox the async `agent` already uses (`exarch/src/tools/agent.rs:287`), landing next turn as `[spawn 'build' finished]`. This deletes the waiter, the shell-free settle, and the two-observer handoff (deliver-once becomes a boundary-time `joined`-flag check — the worker *emits*, never is *observed*), and is safe by construction: the worker holds `notify` (session-owned, the same class the async `agent` worker already holds), never the foreground `Emitter`, and `notify` cannot splice into sealed scrollback because it only ever lands at a boundary. The two ADRs stay side by side as `proposed` for the author to choose. Recorded the layering correction (the `spawn-done` decode belongs in the tui boundary render, not `marked_turn`) and that the async value is data, not a live handle, so `events.json` needs no special-casing. Updated [[index|index]].

## [2026-06-22] ingest | reply tool landed; tools map refreshed

Implemented [[decisions/260622_agent-reply-tool|agent-reply-tool]]: a sub-agent returns the argument of an explicit, hard-terminating `reply` tool (no prose scrape), gated by a new `replies()` axis mirroring `spawns()`. Re-ingested [[map/exarch/tools|tools]], which had drifted to a `shell` tool and `Staged::Done/Spawned` join phase: it now describes the live registry (`ral`, the spawn family, `reply`, the schedule family, `fff`), the synchronous-dispatch contract, and the mirror-image `ToolSet` gate. Updated [[index|index]].
