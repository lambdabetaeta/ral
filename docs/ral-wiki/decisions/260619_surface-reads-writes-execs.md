---
status: accepted
---

# Surface every redirect and exec image; sink library I/O below the ral line

**Every ral redirect read (`<`), redirect write (`>` family), and external or
bundled exec image the model launches should appear on the rail — surfaced at
the runtime doors where the operation actually happens, not at kit call sites,
so coverage is total rather than dependent on which helper was used.** Core emits
a structural *I/O event* — the operation and its operands (path, mode, argv,
status, outcome), never a card and never a promised file-size count; exarch binds
it to a card from the existing mark grammar, exactly as it already binds a kit
`surface` ([[decisions/260619_surface-carries-documents|surface-carries-documents]]).
For this to stay honest there is one invariant: **a ral redirect means the
model's own I/O and nothing else.** So the library's bulk plumbing —
`grep-files`'s per-file reads, and the witness hashing it shares with `edit` —
sinks below the ral line into exarch builtins, where the reads happen in Rust and
never reach the redirect frame. The payoff is one surface per logical operation
with **no suppression flag**: grep is a single "searched *here* for *this*" card
because its reads are no longer ral reads, and `<`/`>`/external exec become a
clean, fully-surfaced model-I/O channel. That coverage is an *invariant*, not a
convention: a `disallowed-methods` lint bans every filesystem and process
constructor outside the handful of doors, and each door *fuses* the surface into
the operation — so nothing opens, writes, or spawns without surfacing (see
**Enforcement**).

**Implemented** as built — the chosen fork is `edit` → builtin (see §3), so no
suppression flag exists anywhere; the subsystem map is
[[map/exarch/io-surface|io-surface]]. The diagnosis below records the
pre-implementation state that motivated it: at proposal time only `edit` (one
`diff` card per change) and the tasks kit (`task`/`meter`, `kit/tasks.ral`)
reached the rail; `wrote-card` was defined but never called; there was no read
card and no exec-image card; redirect reads, redirect writes, and external execs
were invisible.

## The diagnosis

Surfacing is currently a **deliberate kit act**: the `surface` builtin
(`core/src/builtins/misc.rs:530`) forwards one `Value` to the turn-local `EventSink`
(`core/src/types/shell/mod.rs:156`); exarch's `AgentSink::emit` (`shell_eval.rs:59`)
runs `value_to_card` and emits `Kind::Card`. A thing surfaces only because a ral
function *chose* to call `surface` — `edit` does, a bare `< file` does not. So "show
every read/write/exec image" cannot be met by adding more kit wrappers: a wrapper only
sees the calls routed through it, and the model issues raw `<`, `>`, and bare external
commands directly. **Total coverage forces surfacing at the runtime, below the kit.**

There are two operation classes, and each has one runtime door:

- **Redirects.** Every `<`, `>`, `>>`, `>|`, `2>`, `&>` funnels through one combinator
  pair — `within_redirect_frame` / `with_redirects` (`core/src/evaluator/redirect.rs`)
  — over `open_file` (`core/src/runtime/command/redirect.rs:223`) for fd 1/2 file
  targets and `install_stdin_redirect` (`:578`) for `< file` on fd 0. The mode set is
  closed and small: `RedirectMode::{Read, StreamWrite, Append, Write}`
  (`core/src/syntax/ast.rs:467`). One hook here is *every* redirect-based read and
  write, exhaustively; the event names the path, mode, and terminal outcome, not a
  guessed byte count.
- **Exec images.** Surface command syntax reaches `CompKind::Exec`, but the hook is
  *not* `eval_call_parts` (`core/src/evaluator/call.rs`): at that point the head may
  still resolve to a ral closure, builtin, or handler. Resolution happens in
  `run_call` (`core/src/runtime/command_call.rs`); only `Resolution::External` reaches
  `run_external` / `command::run`, and bundled tools either complete through that
  spawned `ExecImage::BundledTool` path or through the inline
  `uutils::run_uutils_in_process` door
  ([[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]]).
  Hook those completion doors, after resolution. Then `view 50 100 < foo.rs` is one
  read card and no exec card; a bare external `cargo test` is one exec card; and
  `cat < a` legitimately surfaces two model operations, the read redirect and the
  external exec.

### Why not the capability gate

The tempting "universal" hook is the capability audit funnel `audit_call`
(`core/src/types/shell/checks.rs:53`), through which `check_fs_read` (`:75`),
`check_fs_write` (`:82`), and `check_exec_args` (`:60`) all pass. It is the wrong
granularity in both directions: it **over-fires** on operations that are not data I/O —
`source`/`use` loading ral code (`builtins/modules.rs:149`), `exists`/metadata
predicates (`builtins/predicates.rs:91`), `list-dir` (`builtins/fs.rs:49`) — and it
**under-fires** on bundled coreutils, whose internal `fs::read` (`builtins/uutils.rs:308`)
never calls `check_fs_read` (it is gated at exec/sandbox time). The redirect-frame and
external/bundled exec doors, by contrast, name exactly the operations the rail records.

### The one residual

`source` / `use` read ral files via `read_to_string` (`modules.rs:149`), outside both
chokepoints. That is code-loading, visible as its own statement, not turn-time data I/O
— left unsurfaced, and noted here so the boundary is on the record.

## The decision

### 1 — Core emits a structural I/O event; exarch binds it to a card and keeps the event

At the redirect doors and the external/bundled exec completion doors, core pushes onto
the host channel a plain `Value` describing *what it did*:

```
{io: "read",  path: "src/foo.rs"}
{io: "write", path: "out.json", mode: "write", outcome: "committed"}  # mode ∈ write|append|stream
{io: "exec",  argv: ["cargo", "test"], outcome: "ok", status: 0}
```

This is **not** a card, and it does not name a mark. Core already names this vocabulary
— `FsOp::{Read,Write}` and exec live in its capability layer — so reporting its own
activity adds no new concept to core and does not breach
[[decisions/260618_run-turn-host-loop|run-turn-host-loop]]'s rule that core carry raw
`Value`, nor [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
(the event is a public `Value`, not core's private representation). exarch decodes that
shape into an `IoEvent`, composes a `Card` from the existing marks, and emits a bus event
that carries both: the TUI renders the card through the generic card interpreter, while
`events.json` serialises the raw event beside the rendered mark tree. If parcel 0 reuses
the existing `surface` `EventSink`, `AgentSink::emit` dispatches `io` values through this
adapter and ordinary `` `card `` values through `value_to_card`; if it uses a dedicated
telemetry seam, the same `IoEvent -> Card` adapter is still the boundary. The division
stays exactly as [[decisions/260619_surface-carries-documents|surface-carries-documents]]
drew it — **core names the operation, exarch names its appearance.**

### 2 — One surface per logical operation, so bulk I/O sinks below the ral line

The redirect frame cannot tell the model's `view 50 100 < foo.rs` from `grep-files`'s
internal `from-string < $f` (`agent.ral:176`) — both install a read frame. The clean
resolution is **not** a suppression flag but an invariant: *if a ral redirect always
means the model's I/O, library plumbing must not be a ral redirect.* So the bulk-I/O
helpers move below the ral line:

- **`grep-files` becomes an exarch builtin.** The ral wrapper is already careful: it
  reads each *matched file* once, reusing the rows across that file's consecutive hits
  (`agent.ral:173`), so its reads are linear in matched files, not hits (`:163` documents
  this, it is not a complaint). The waste is narrower — `_search-files`
  (`agent_builtins.rs:91`) already held those files' bytes when it matched them, so the
  ral fold reads the matched subset a *second* time solely to compute `window-hash`.
  Folding the witness stamp into the search's own pass removes that second read — and
  *also* the two-decoder dance it forces: `_search-files` decodes lossily to match, so the
  ral fold re-reads strictly and must fail on a file that decoded one way but not the
  other (`agent.ral:169`–`178`); in one pass a witness is stamped when the matched file
  decodes validly (the only files `edit` can touch) and flagged otherwise — the error path
  disappears. So the builtin is *simpler*, not merely faster. The decisive reason, though,
  is architectural rather than either: the builtin reads in Rust,
  so its reads never reach the redirect frame, and it emits **one** I/O-style surface —
  the cwd scope and the pattern — instead of one read card per matched file. The plumbing
  is relocated, not silenced.

- **`window-hash` becomes a builtin too** (`agent.ral:20` → Rust), because it is the
  coupling. It is shared by `grep-files` (`:184`) *and* `edit` (`:102`); moving only
  grep's copy to Rust would duplicate the witness algorithm across the language boundary
  — the WET seam this codebase forbids. Promoting it is small and consistent: it is a
  thin fold over `line-hash`, which is *already* an exarch builtin
  (`agent_builtins.rs:73`). One implementation, called from Rust grep and from ral
  `edit` alike.

The principle generalises: **any helper that performs bulk or atomic file I/O as a
single logical act belongs below the ral line**, where it is one surface by
construction. After grep and the witness hash move, the only ral helper still doing
internal redirects is `edit` (`< $path` at `:99`, `> $path` at `:135`) — the boundary
case below.

### 3 — `edit`: the boundary case

`edit` is one logical operation (apply a batch to a file) that already emits the richest
surface there is — the `diff`. Its internal single read and single write are *sub-steps*
of that operation, not operations in their own right, and a read card + write card
beside the diff is pure redundancy. Two ways to honour the invariant:

- **Recommended — `edit` follows grep below the line.** Fold the read → resolve → atomic
  rebuild → write into a builtin that surfaces the `diff` from Rust. Then *no* ral helper
  does internal I/O, the invariant "ral `<`/`>`/exec = the model's I/O" is total, and no
  suppression mechanism exists anywhere. **Cost, stated plainly:** `edit`'s
  read/witness/rebuild is a showcase ral program; moving it to Rust trades that
  transparency for the clean invariant. Given `_search-files`, `line-hash`, and (now)
  `window-hash` already live in Rust, `edit`'s witness machinery is on the same side of
  the boundary as its dependencies — so this is consolidation, not new hiding.

- **Alternative — keep `edit` in ral behind a `quiet` scope.** A dynamic scope that
  suppresses I/O-event emission for its extent while leaving explicit `surface` calls
  live; `edit` wraps its read+write, still emits its diff. Keeps the algorithm readable
  in `agent.ral`. The cost is that one ral helper now needs a marker the model's code
  must never use, and a future bulk helper that forgets it leaks plumbing onto the rail —
  the failure mode the "below the line" rule removes entirely.

This is the one decision the proposal leaves open for the author; everything else
follows from it.

### The cards — composed from the existing marks, zero new mark vocabulary

Per [[decisions/260619_surface-carries-documents|surface-carries-documents]], a card is a
stack of the five marks (`text`/`measure`/`fields`/`diff`/`raw`) with nominal roles
(`path`/`code`/`ok`/`warn`/`bad`/`muted`/`strong`). The new surfaces add no mark:

- **read** (`<`): a `text` mark — a `muted` `<` glyph and a `path`-roled path.
- **write** (`>` family): a `text` mark — a `>` glyph, a `path`-roled path, the mode,
  and an outcome role (`ok` for committed, `bad` for failed, `warn` for aborted). The
  old dormant `wrote-card` becomes a narrower write-card constructor; runtime redirects
  do not carry written bytes or previews.
- **exec**: a `text` mark — the program as a `path`/`code` span, the args as `code`,
  and the exit outcome/status as a nominal role (`ok`/`bad`/`warn`). Bundled `cp`/`cat`
  surface here; their internal byte-shuffling rides the visible call.
- **grep**: a `text` mark — the cwd scope and the pattern — emitted once by the builtin.

## Why this shape

- **Coverage is a property of the runtime, not of kit discipline.** Hooking the two
  evaluator chokepoints means a read/write/exec surfaces no matter which helper — or no
  helper — issued it. A wrapper-based scheme can only cover what is routed through it.
- **The invariant beats the flag.** "ral redirect = model I/O" is enforced by *where the
  code lives*, not by a `quiet` marker every future author must remember. Plumbing that
  cannot be a ral redirect cannot leak onto the rail.
- **grep moves the right way for the right reason.** It is faster in Rust (no re-read),
  it removes N reads from the rail, and the raw search it wraps is already a builtin —
  the ral layer was only stamping witnesses, which the search can do in the pass it
  already makes.
- **The boundary is already where this puts it.** `_search-files`, `line-hash`,
  `explore-dir` are Rust; `view`/`edit`/`grep-files` are the ral library on top
  (`agent_builtins.rs:33` sources `AGENT_SOURCE`). Witness hashing is a hot, shared,
  low-level primitive — it sits more naturally with its kin in Rust than as a ral closure
  edit and grep both reach for.
- **Two consumers, one description.** As with cards, the I/O event serialises
  structurally into `events.json` and styles into the TUI from one shape.

## What moves, what stays

- **Moves below the line (exarch builtins):** `grep-files` (folding the witness stamp
  into `_search-files`'s pass) and `window-hash`. `edit` too, under the recommended fork.
- **Gains:** exarch's decoder gains three `io` arms and the read/write/grep/exec card
  composers; the dormant `wrote-card` shape is narrowed into a write-event card. The
  system prompt (`exarch/data/ral.md`, `system.md`) drops `window-hash`/`grep-files` as ral definitions
  and presents them as builtins.
- **Stays untouched:** `view` (its `<` is the model's read at the call site — surfaces
  once, correctly), the mark grammar and renderer, the capability gate, and — modulo the
  I/O-event emission — core's evaluator structure.
- **Stays in ral:** the agent library's orchestration (`view`, and `edit` under the
  alternative fork).

## Enforcement — every door is accounted for

That "all I/O surfaces" holds is not left to discipline; it is the conjunction of two
mechanically-checked facts, in the style the `clippy.toml` header already sets for
canonicalisation, cwd, and child-wait.

**1 — All I/O goes through a known door (clippy).** Extend `disallowed-methods` to the
filesystem and process *constructors*; the handle is the capability, so banning
construction suffices and keeps the list to the inherent associated fns clippy resolves
cleanly — `std::fs::File::{open,create,create_new}`, `std::fs::OpenOptions::open`, the
one-shot `std::fs::{read,read_to_string,write,read_dir,metadata,symlink_metadata,
read_link,remove_file,remove_dir_all,create_dir_all,rename,copy,set_permissions}`,
`std::process::Command::new`, and `std::os::unix::process::CommandExt::exec`. Every call
site is then either a door or a lint failure; CI runs
`cargo clippy --all-targets -- -D clippy::disallowed_methods`, so a stray open is a build
break, not a review note.

**2 — Each door is accounted for, surfacing or silent (fusion + reason).** The
allowlisted doors split in two, and the mandatory reason field records which:

- **Surfacing doors fuse the surface into the operation.** `open_file` /
  `install_stdin_redirect` (`runtime/command/redirect.rs`) record redirect read/write
  attempts; the redirect frame emits their terminal outcome when the frame settles, with
  atomic `>` marked committed only after `commit_atomics` succeeds and aborted when the
  body failed before commit. The external completion path (`command::run`) and the inline
  bundled-tool path (`uutils::run_uutils_in_process`) emit exec events when status or
  spawn failure is known. The search builtin (`agent_builtins.rs`, §2) emits grep once
  for the logical search. There is no open-, write-, or spawn-without-surface API,
  because there is no open, write, or spawn *except* the door.
- **Reasoned-silent fs sites** do filesystem work that is not the model's data I/O and
  raise no card by design: canonicalisation (`path/canon.rs`), `which` probes
  (`path/which.rs`), module loading (`builtins/modules.rs` — the `source`/`use` residual
  named in the diagnosis), and stat predicates (`exists`/`list-dir`,
  `builtins/{predicates,fs}.rs`). Each carries the same `#[allow]` with a reason stating
  *why* it is silent, so silence is a written decision, not an omission.

The invariant is thus stronger and more honest than "everything surfaces": **every
fs/process site is either a surfacing door or a reasoned-silent one, and the lint
guarantees there are no others.** A meta-test pins it — the fs/process
`#[allow(clippy::disallowed_methods)]` sites must equal the reviewed door set, so a new
door cannot slip in unsurfaced and unreasoned, in the structural-test style of
[[decisions/260614_structural-bug-prevention|structural-bug-prevention]].

**The type half (optional).** clippy fixes *where*; to make a non-surfaced handle
unrepresentable as well, a surfacing door can return a `Surfaced<File>` whose only
constructor runs the emission, with the raw `std::fs::File` never escaping the io module —
the witness-marker discipline of
[[decisions/260601_reduced-authority-witness|reduced-authority-witness]]. Redundant under
single-door confinement, but it carries the invariant in the type where a refactor cannot
silently drop it.

**What the lint cannot reach.** It guarantees a surface is *emitted*, not that its path,
mode, argv, status, or outcome is *correct* (the test plan's burden), and it cannot see
inside dependencies —
`ignore`/`grep` (under the grep door), `tempfile` (under the write door's atomic commit),
bundled `uutils` (under the exec door). Those syscalls are confined by the OS sandbox
([[decisions/260617_sandbox-external-children|sandbox-external-children]]), not the lint —
the same boundary that alone can answer what *spawned children* did.

## Alternatives considered

- **Surface from the capability audit trail (`audit_call`).** Rejected: wrong
  granularity — over-fires on `source`/`use`/`exists`/`list-dir`, under-fires on bundled
  coreutils' internal reads. It records capability *checks*, not the model's *operations*.
- **Kit wrappers only (`read`/`write`/`run` + a grep card), no runtime hook.** Rejected:
  cannot capture *every* op — a raw `< file` or bare external command the model writes
  directly bypasses every wrapper. Fails the requirement outright.
- **Keep all plumbing in ral, suppress with a `quiet` scope everywhere.** Rejected as the
  general mechanism: it leaves library reads as ral reads and depends on every present and
  future bulk helper marking itself; a forgotten marker is a silent rail leak. Retained
  only as the narrow fallback for `edit` (fork above).
- **Auto-detect library frames in the evaluator** (suppress redirects running inside a
  boot-sourced closure). Rejected: needs closures tagged with definition provenance and
  still cannot distinguish a model's own inline `each { … < f }` loop from library
  plumbing. More magic, more fragile, than relocating the plumbing.
- **Core grows a card/IO taxonomy it renders.** Rejected: reverses
  [[decisions/260618_run-turn-host-loop|run-turn-host-loop]]. Core names the operation
  (which it already does); exarch names the card.

## Consequences

- The rail becomes a faithful provenance trace: every redirect target the model read,
  every redirect target it wrote, and every external or bundled exec image it ran — one
  card per operation. grep is one card, not one per file.
- No suppression flag exists (recommended fork) — correctness by where code lives, not by
  a marker.
- "All I/O surfaces" is build-time–checked — a `disallowed-methods` denylist (I/O only at
  known doors) plus chokepoint fusion (each door surfaces) — not convention; every silent
  fs site carries a written reason, and a meta-test keeps the door set closed.
- grep is faster (single-pass witness) and `window-hash` is DRY (one Rust impl, edit and
  grep share it).
- `agent.ral` shrinks; the witness/search machinery consolidates in Rust beside
  `line-hash`/`_search-files`. Under the recommended fork, `edit`'s ral algorithm is the
  price.
- New rail volume: surfacing every op is louder than today's edit-only rail. The agent
  kit's posture is to want it; if it proves noisy, a turn-level toggle can gate it without
  changing the chokepoints.

## Implementation plan

Documentation of intended work, not a commitment to build now. Six parcels; each
compiles and tests alone.

```
0  I/O event      core emits {io:read|write|exec, …} Value at redirect + external/bundled completion doors; pick carrier and Kind::Io shape
1  Card binding   exarch IoEvent -> Card adapter; read/write/exec composers; events.json records raw event + card
2  grep → builtin  fold witness stamp into _search-files's pass; builtin emits one grep surface
3  window-hash     promote agent.ral:20 → exarch builtin; edit (:102) + grep call it; drop the ral def
4  edit (fork)     recommended: edit → builtin surfacing its diff;  alt: quiet scope around its <,>
5  Enforcement    disallowed-methods on fs/process constructors; #[allow]+reason per door (surfacing | reasoned-silent); outcome-fused emit; meta-test the allowlist; CI -D
```

## Test plan

- **Coverage.** A raw `from-string < a`, a `to-string "x" > b`, and a bare external
  `cmd` — issued directly, through no helper — produce read, write, and exec cards
  respectively. `view 1 2 < a` produces the read card and no exec card, because `view`
  is a ral helper, not an exec image. `cat < a` produces two cards: one read redirect
  and one external exec.
- **grep is one surface.** `grep-files pat` over a tree of N matching files emits one card
  (scope + pattern), not N reads; the witnesses it stamps still resolve in a subsequent
  `edit` (the round-trip the move must preserve).
- **window-hash parity.** The builtin's hash equals the retired ral `window-hash` for the
  same rows/index (port `shell_eval.rs:481` `edit_window_hash_addresses_repeated_lines`),
  and `edit` still addresses a repeated line by it.
- **edit is one surface.** Under either fork, an `edit` emits its `diff` and *no* separate
  read/write card.
- **Card shape & Bertin.** read/write/exec cards compose only existing marks; path,
  mode, argv, status, and outcome are nominal text roles. No file-size or byte-count
  measure is surfaced.
- **events.json.** Each io event serialises structurally beside the card exarch rendered
  from it.
- **Residual.** `source`/`use` produce no card (documented boundary).
- **Totality.** Each surfacing door emits a terminal outcome on success and failure:
  atomic writes say committed only after commit, aborted when the body failed before
  commit, and failed when open/commit/spawn fails; under the type-half, a `File` cannot
  be obtained without the door.
- **Closed door set.** The meta-test — the fs/process `#[allow(clippy::disallowed_methods)]`
  sites equal the reviewed allowlist, so a new unaccounted constructor call fails CI.

## See also

[[decisions/260619_surface-carries-documents|surface-carries-documents]] (the card/mark
grammar these new surfaces compose from — they add no mark),
[[decisions/260618_run-turn-host-loop|run-turn-host-loop]] (core carries raw `Value` and
names no card vocabulary — why core emits an *operation*, exarch the card),
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]] (the io
event is a public `Value`, not core's internal repr),
[[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]] (bundled
`cp`/`cat` surface as exec, before the Host/bundled split),
[[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]] (the Bertin
bindings the cards reuse), [[map/exarch/cards|cards]] (the surface subsystem as built),
[[map/exarch/shell-eval|shell-eval]] (the `AgentSink`/`value_to_card` seam these events
enter), [[decisions/260614_structural-bug-prevention|structural-bug-prevention]] and
[[decisions/260601_reduced-authority-witness|reduced-authority-witness]] (the lint- and
witness-discipline the Enforcement section extends),
[[decisions/260617_sandbox-external-children|sandbox-external-children]] (the OS boundary
for what the lint cannot reach), [[map/exarch|map: exarch]].
