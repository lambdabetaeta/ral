---
generated_at_commit: 61c288f
generated_at_date: 2026-10-21
covers_paths: [core/src/types/audit/observation.rs, exarch/src/tui/group.rs, core/src/types/audit/fan_out.rs, core/src/evaluator/call.rs, core/src/runtime/command_call.rs, core/src/path/walk.rs, core/src/guard/shell.rs, core/src/runtime/redirect.rs, core/src/runtime/redirect/stage.rs, core/src/runtime/command/detach.rs, core/src/runtime/pipeline/collect.rs, core/src/runtime/redirect/scope.rs, core/src/runtime/command.rs, core/src/runtime/command/stdio.rs, core/src/types/shell/mod.rs, core/src/types/mooring.rs, exarch/src/card.rs, exarch/src/card/diff.rs, exarch/src/card/value.rs, exarch/src/card/decode.rs, exarch/src/card/encode.rs, exarch/src/card/observation.rs, exarch/src/card/done.rs, exarch/src/card/testkit.rs, exarch/src/agent/desk.rs, exarch/src/shell_eval.rs, exarch/src/bus.rs, exarch/src/bus/post.rs, exarch/src/bus/inbox.rs, exarch/src/bus/signal.rs, exarch/src/bus/channel.rs, exarch/src/bus/emitter.rs, exarch/src/bus/sink.rs, exarch/src/record/commit.rs, exarch/src/headless.rs, exarch/src/shell_eval/builtins.rs, core/src/path/tree/walk.rs, clippy.toml, lint/site.rs, core/tests/sites.rs]
---

# Map: exarch / io surface

The transcript calls shell workers **errands**: `errand #7 started · building`
at birth (`card/observation.rs`) and
`errand #7 (building) finished after 3 mins 12 secs (exit 101): build failed`
at completion (`card/done.rs`). Birth and completion share the worker's id
and title, the title exarch's policy requires of every `spawn`; the run time
is read off the worker's own clock, and the outcome is its real ending — a
status with the error's message, an `exit`, a stop's cause, or a panic. The
model's wake-up notice (`surface_notice`, `bus/post.rs`) is that completion
line followed by a sentence naming how the bound handle and registry inspect it
(filtered by `Avatar::arrive` if the worker was already claimed during the turn):
`` `await` on its bound handle replays its output and returns its value; `workers` lists every worker.``


Every redirect read (`<`), every redirect write (`>` family), every external
or bundled exec image the model launches, and every denied head admission
surfaces on the rail — **one structural observation per logical operation**,
the rail then coalescing a burst into one card per kind. Coverage is a
property of the **runtime**, not of kit discipline: the hooks sit at the
syscall sites where the operation actually happens, so a read/write/exec
surfaces no matter
which helper — or no helper — issued it. Core emits a structural
**`Observation`** (`core/src/types/audit/observation.rs`) — the one vocabulary
shared with the [[design/audit|audit trail]], `--audit`'s JSON, and the wire;
exarch binds it to a card from the existing [[map/exarch/cards|mark grammar]],
exactly as it already binds a kit `` `card ``
([[decisions/260619_surface-carries-documents|surface-carries-documents]]). The
division is the one [[decisions/260618_run-turn-host-loop|run-turn-host-loop]]
draws — **core names the operation, exarch names its appearance** — so core
grows no card vocabulary and leaks no representation
([[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak]]). The
governing invariant: **a ral redirect means the model's own I/O and nothing
else**, held not by a flag but by *where code lives* (below). See the decision,
[[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]].

## The syscall sites — core emits its own activity

Three operation classes, each hooked at the sites that realise it. Every site
builds one `Observation` (`types/audit/observation.rs`) and hands it to
`Shell::observe` (`types/audit/fan_out.rs`), the single fan-out point; `Shell::observation`
stamps the call site and principal every site fetches. It judges nothing: the
observation goes to the run's `Mooring::surface` (`types/mooring.rs`) and onto
the open [[design/audit|audit trail]], each already inert when its consumer is
absent. **Which observations matter is the host's call**, made once in
`Landed::of` (`card/observation.rs`), the one refinement of an observation to
what the rail draws (so renderers are total), applied at `decode_surface` — so
a builtin command and an allowed capability check are reported by core and
dropped by exarch, never drawn and never journalled.

The one filter core keeps is its own, and it is the decision rather than a flag:
a capability *denial* joins the trail whenever one is open, an *allowed* check
never does, and no `grant` has a dimension to change either — language
semantics, not presentation ([[design/audit|audit]]). And only a head admission
(`command_call.rs`) surfaces a *structured* denial — the `fs` and full-argv
checks in
`guard/enforce.rs` are entered from `guard/shell.rs`, whose
callers in `builtins/` and exarch's own sites carry no `Mooring`, so their
denials reach the trail alone. A refused *external* command still surfaces
either way, as a failed command observation carrying the denial message.

With nobody listening — no sink installed and no trail open — a site builds no
observation at all, so it pays for neither the `epoch_us()` syscall nor the
`script` and `principal` clones. With a host attached it pays them per observation —
an external's dispatch, a redirect, a birth — and never per builtin
application, which is no observation at all.

- **Redirects** open through `open_file` and `install_stdin_redirect`
  (`runtime/redirect.rs`). A **read** (`< file`, fd 0) emits eagerly
  when the file opens — `Observed::Read { path }`, no outcome — so it precedes
  the body it feeds. A **write** (`>`/`>>`/`>~`, fd 1/2) opens as an
  `OpenedWrite` — `Atomic`, staged beside a regular file, or `Stream` — that
  `RedirectState` (`runtime/redirect/scope.rs`) holds as a `WriteIntent` and
  reports when the frame settles (`settle`), with the outcome the site alone
  can know: an atomic `>` is `committed` (body ok, commit succeeded),
  `aborted` (the body did not reach the commit) or `failed` (commit failed);
  a stream — `>>`, `>~`, any `2>` — committed each byte as it landed and is
  `committed` whatever the body did. An open that fails is reported `failed`
  there, the one write that never reaches settle.
  Every entry a redirect makes — read, write, refusal, settled write — carries
  the redirect's own site (`RedirectState` holds the node's span and runs each
  step under `Shell::at_site`), not what its body dispatched last.

  One interpreter opens the targets in a fixed order — stdin, stdout, stderr — so
  the trail reads the same for a block and an external.

  Mode is `write` / `append` / `stream`. No byte count — path, mode, outcome,
  plus the content snapshots, each whole or `` `none `` and never a prefix:
  `old-bytes` taken at the open, `new-bytes` at settle (what ral wrote, read
  through the staging handle for an atomic `>` and the derived reader for a
  stream, never by name), read only within 64 KiB (`PREVIEW_CAP`), of a
  regular file, under a grant that admits the read. A
  write that did not land carries neither. Exarch diffs the two into the
  write's `Change`; the [[design/audit|audit trail]] keeps them whole.
- **Commands** are hooked *after* resolution, at the completion sites, never
  at the call site (where the head may still resolve to a closure or
  builtin). Every external or detached command is one
  `Observed::Command(Command { argv, status, origin, .. })`: `argv` is the shown name
  first, then its arguments; `origin` is `` `external `` or `` `detached ``.
  The external / bundled path — one site for both, since a bundled tool is a
  `ral --ral-bundled-tool` child like any host executable
  ([[decisions/260731_bundled-tools-always-reexec|bundled-tools-always-reexec]])
  — emits from `call_external` (`runtime/command_call.rs`), which wraps the whole
  dispatch and so covers a spawn failure too, since that never reaches
  `wait()` (the card derives ok/bad from the status directly; a spawn failure
  carries the synthesized 127/126/… code). `detach` (`runtime/command/detach.rs`)
  is the second site, and the one that surfaces at the spawn rather than the
  wait: a surrendered process is never waited for, so its observation carries
  `` origin: `detached `` and status `0` meaning *exec'd*, not *succeeded*. A
  direct external *pipeline stage* is the third: the collector never enters
  `call_external`, so its settlement mints the fact itself
  (`runtime/pipeline/collect.rs`). All three assemble their argv through the
  one constructor, `Command::new` — the rule that index 0
  is the shown name lives there and nowhere else. A **builtin** application
  is no command observation at all: its frame (`BuiltinEntry::framed`) stamps nothing
  and tees nothing, and whatever it did to the world arrives from the operation it
  did it through — `edit-hash` as the `` `write `` its `atomic_write`
  commits, `spawn` as its `` `worker `` ([[design/audit|audit]]).

Capability checks are different again. An *allowed* check stays off the rail:
it is the wrong granularity — it over-fires on `use`/`exists`/
`list-dir` and under-fires on bundled coreutils' internal reads. A *denied*
check is the exception: it is the highest-signal line in a provenance record,
so a head admission reaches the rail whether or not a trail is open — see
[[design/audit|audit]].

A pipeline stage's own sites reach the rail too, not only its trail. A
ral-written stage runs in a thread that ships `active_policy()`, `None`
unless the parent is collecting, so with no `audit { }` in force it opens no
trail and its fragment comes back empty. A direct external stage has no trail
of its own: the collector builds its one command fact unconditionally, the
collect phase being pure over its events — deciding emission there would
judge an interest it cannot see, having no `Mooring`. Either
way the parent reports each merged observation rather than folding it
straight into the trail, so a stage's writes and commands reach the host
exactly as anything run locally does, and the fold's one entry asks the sink
and the trail each on its own terms. They arrive at stage settle, not at
their original instant, but each observation's own `start`/`end` still carry
its true timestamp.

## The observation — a Value, not a card

Every observation carries a common envelope — an optional `site`, `start`,
`end`, `principal` — around one `what`, a variant whose **tag is the
kind**. There is no `kind` field: the tag is the only place the kind is
recorded, so nothing can disagree with it, and a reader dispatches with `case`
over a row the typechecker closes.

```
what: `read    [path]
      `write   [path, mode:`write|`append|`stream, outcome:`committed|`aborted|`failed, new-bytes, old-bytes]   # each snapshot `some or `none
      `command [argv:[prog, …args], status, origin:`external|`detached, stdout, stderr, error]   # error `some or `none
      `grep    [scope, pattern]                        # emitted by the grep builtin
      `check   [resource:`exec|`fs|`deputy, decision:`denied|`flagged, fields]   # decision is the resource's own
      `worker  [id, title]
      `act     [verb, subject, payload, refused]       # authored host-side by the desk
```

`argv` replaces a separate `cmd`/`args` split, and carries no returned
`value`: a process-local or executable value has no honest projection, so the
argv, status, and bytes are the whole of what a reader gets. A `` `check ``'s
`resource` and `decision` sit beside its `fields` map — a denial is never
encoded as a status, and never spliced through a nested value. Core already
names this vocabulary in its capability layer, so reporting its own activity
adds no concept. `Observation::to_value` is the one projection — the same map
shape the [[design/audit|audit trail]] and `--audit`'s JSON use. On the
[[map/core/shell-state|sink]], which a kit `` `card `` shares, it rides tagged
as `` `observed <record> `` (`Observation::to_surface`), so every surfaced
value is a variant and a host dispatches on the tag alone; the trail and the
recorded `Display::Observation` hold only observations and keep the bare
record.

## Binding to a card — exarch

`decode_surface` ([[map/exarch/shell-eval|shell-eval]]) is the shared surface
decoder: an `` `observed `` value decodes through
`Observation::from_surface` into `Surface::Observation`, the raw observation
alone — no card built yet, since the decoder's own codomain carries the
structured value and nothing a printer merely wants a copy of. The card is
bound by `observation_card` only at draw time — from whichever printer's fold
reads the recorded `Display::Observation` — never by the seam
(`shell_eval/surface.rs`'s `record`) that records it: the
observation crosses the seam as its raw wire form alone (`Datum::encode` of the `Observation`),
and the card is rebuilt fresh wherever it is drawn. The other surface shapes
(edit, notice, card, done) have their own arms; a value matching none drops,
the same graceful degradation as before.

`observation_card` composes from the existing marks ([[map/exarch/cards|cards]]).
The operation is a *nominal category*, so it is carried by a word, not a
mirror-orientation glyph: read is a `muted` `read` verb + a `path` span; write
is no card at all — `decode_surface` turns it into a `Change`, its diff taken
between core's two snapshots, none unless both are known text
([[decisions/261006_a-file-change-is-one-fact|a-file-change-is-one-fact]]),
and a change with no diff reads `write <path> <outcome>`: `committed` uses the
`ok` role, `aborted` uses `warn`, and `failed` uses `bad`; a command keeps the conventional `$` prompt, the program as
`path`, each arg as plain ink, and a `→ status` tail roled `ok`/`bad` off the
observation's own `status`; grep is the pattern as `code` `in` the cwd scope
as `path`; a capability check reads `check <resource> <decision> <fields…>`,
the decision roled `Role::Bad` when denied (the only decision the rail ever
surfaces) and its trailing fields — core's own `resource`-specific map,
`name`/`resolved`/`args` for `exec`, `op`/`path` for `fs`, `prefix` for
`deputy` — rendered
as `key=value` pairs in the map's own order, whatever is present, nothing
inferred. `Role::Path` carries a real hue, so the subject of every row stands
as figure against the muted label and the body prose.

The record carries raw facts — one `Display::Observation` per observation,
one `Display::Change` per write or edit — and the **grouping is the frontend's**, derived
online by [[map/exarch/frontend|the mirror]] from three tail rules. Core
surfaces each effect as its own observation, so a burst would otherwise read
as `read…`, `$…`, `read…`, `$…` — noisy clutter at the rail. The rules:

- **An effect joins the most recent call.** `Landed::Effect` walks back from
  the mirror's tail past whatever landed since to the nearest group holding a
  call, and folds onto it (`Scrollback::absorb`); with no such group it
  renders alone, unframed. A redirect writes at the *seam*, mid-call, so
  `read a · write b · read c` would otherwise strand `read c` behind a
  barrier, where the run's tally could not count it. Walking back, every
  effect of one call reaches the mirror's picture of that call whatever landed
  between the two.
- **A change joins the run of changes.** A mutation, not a foldable
  observation: it joins the always-visible `▎` run standing at the tail or
  opens one, which *closes* the group, so the next call opens a new one.
- **Dedupe and list at render.** `group::Call` holds its effects as the
  facts themselves and drops a repeat — a read by path, an exec by argv, a
  grep by `(scope, pattern)` — then renders each non-empty effect bucket
  (reads, greps) and each exec as a `card::Listing`, a head and items split
  between items, never within, in a fixed Read → Exec → Grep order, the
  order-independence being the point: a reader does not care how a burst
  interleaved. Listings and `Tally` read one partition of those facts
  (`Call::buckets`), so the tally is the three bucket lengths rather than a
  second sort that has to be kept in agreement.

A capability check or worker birth joins no call — rare and high-signal enough
to earn its own line. A denial stands as its own card; a **worker birth** is
not a card at all but a rail notice wearing the `↗` of the fleet act that made
it, mirroring the `↘` its settlement arrives on — the departure and the return
of the same worker read as a pair of announcements around the run. A
lone surface and the grouped rows both read a `Listing`, so they cannot drift;
the one departure is that an exec row **drops the `→ status` tail** — a run
reads as the *set* of commands run, and per-command status survives in the
structured observation. The run's dial reaches every effect row: at `Summary` a
row keeps the items that fit plus a muted `… +N more`, at `Full` every item
hangs under the first (as a diff shows its peek, then every hunk). The render
path is shared with a deliberately `surface`d `Display::Card` (`render_card`),
so width-reflow and the rest are free.

## One surface per operation — bulk plumbing below the ral line

The redirect frame cannot tell the model's `view-hash 50 100 < foo.rs` from a
library helper's internal read — both install a read frame. The resolution is
**not** a suppression flag but the invariant *if a ral redirect always means the
model's I/O, library plumbing must not be a ral redirect*. So the bulk-I/O
helpers moved below the line into [[map/exarch/builtins|builtins]]
(`shell_eval/builtins.rs`), where their reads happen in Rust and never reach the
frame:

- **`view-hash`** reads the whole file in Rust (its adaptive-context witnesses
  depend on file-wide uniqueness) and constructs its own single
  `Observed::Read { path }`, via core's public constructor rather than a
  hand-built map — one logical read, one surface, matching the shape the
  redirect frame would have pushed.
- **`grep-files`** does one `fs::read` per matched file (the `search_tree` walk
  reads the bytes the search already needs) and constructs **one**
  `Observed::Grep { scope: path, pattern }` for the whole logical search — not
  one read card per file.
- **`edit-hash`** / **`edit-replace`**
  ([[design/hash-addressed-editing|hash-addressed editing]]) read, resolve,
  atomically rebuild, and write entirely in Rust through core's atomic write
  site (`Shell::atomic_write`) — the read is silent (a sub-step of one logical
  operation) and the site observes nothing, so the editor owns its whole
  surface and speaks it as one `` `card [`diff …] ``. It diffs its own two
  texts, both already resident, so unlike a committed `>` it is under no
  pre-image cap and reads as a diff whatever the file's size; an edit that
  rebuilt the file unchanged surfaces nothing. With the editors below the line,
  **no** ral helper does internal I/O and no suppression mechanism exists
  anywhere.

The residual on the record: `use` reads ral *code* via `read_to_string`
outside the redirect frame — code-loading, visible as its own statement, not
turn-time data I/O — and surfaces nothing by design.

## Machine log

There is no independent operational trace: the record log
([[map/exarch/frontend|frontend]]) carries an observation's total wire form
as a display commit so a resumed scrollback can rebuild its card, but never
the rendered mark tree itself — a rendering is not a fact.

## One file opens everything

Every filesystem operation on a model-named path happens in
`core/src/path/walk.rs`. `Shell::locate` walks the name from the root, one
component at a time through directory handles, never letting the kernel follow
a symlink — each link is spliced into the name and re-walked — and the grant
judges the *object* it lands on. The `Located` it returns performs every open,
stat, listing, staging, rename and unlink relative to that directory's handle
with `FollowSymlinks::No`, so what was judged is what gets opened.

That is why the site set is short enough to read: `redirect.rs` drives the
recipe but contains no open, and `builtins/fs.rs`, `builtins/modules.rs` and
exarch's readers, editors and grep all reach the filesystem through a
`Located` rather than by re-walking a string the in-process guard already judged. The one
exception is the discard device, which has no object to locate — on Windows
it is the device-namespace name `\\.\NUL`, no entry in any directory. Any
other path ending in a DOS reserved device name is refused at the check rather
than opened.

Three entry points, differing only in how they answer a refusal, because the
callers genuinely need different answers:

| | walk fails | grant refuses |
|---|---|---|
| `locate` | `Err` | `Err` |
| `locate_existing` | `Ok(None)` | `Err` |
| `locate_if_admitted` | `None` | `None` |

`locate` is the syscall site proper. `locate_existing` is what `exists`/`is-file` and
their siblings need: an absent path is `false`, a denied one still raises —
and where the walk found nothing to judge, the refusal falls back to the name,
so a denied path that does not exist cannot leak the difference by reading as
merely absent. `locate_if_admitted` is for scans — `grep-files`, `list-dir`'s
per-entry filter — where one off-limits entry must skip rather than blank the
whole listing.

## Enforcement — every syscall site is accounted for

**That "all I/O surfaces" holds is the conjunction of mechanically checked
facts: a ban on the constructors, a typed site for every exception, and checks
for what clippy cannot state.**

- **All I/O goes through a named site (clippy).** `disallowed-methods` bans the
  fs/process *constructors* — `File::{open,create,create_new}`,
  `OpenOptions::open`, the one-shot `fs::{read,read_to_string,write,read_dir,
  metadata,symlink_metadata,read_link,remove_file,remove_dir_all,create_dir_all,
  create_dir,remove_dir,hard_link,soft_link,rename,copy,set_permissions}`,
  `DirBuilder::create`, the symlink makers, `env::{var,vars,vars_os,temp_dir,
  home_dir}`, the four user-named functions of `ral_core::own`, `Command::new`,
  `CommandExt::exec`, and
  `ignore::WalkBuilder::build` (every directory walk roots at the one
  cancellable site, core's `path/tree/walk.rs`, beneath `glob` and exarch's
  `grep-files`, `explore-dir` and `find-files`). The whole `cap_primitives::fs`
  surface is banned alongside it, not just the entries `path/walk.rs` uses
  today: a ban naming only the current callers is exactly what lets the fs site
  move out from under the list. Enforcement rides the pre-existing
  `[workspace.lints.clippy] disallowed_methods = "deny"` table, which all ten
  crates opt into via `[lints] workspace = true`, which a test checks of every
  member; plain `cargo clippy
  --workspace --all-targets` checks it (`scripts/ci.sh`'s `lint` step runs it on
  the Unix hosts, `windows.yml`'s clippy step on Windows). A call is then a
  typed site or a lint failure *in the crates that do not switch the lint off
  again at their own root*: `ral-daemon/src/lib.rs` and
  `ral-initramfs/src/lib.rs` each carry a crate-level
  `#![allow(clippy::disallowed_methods, …)]` on the grounds of being a guest's
  init rather than the ral shell, so inside them the site set rests on review,
  never on the compiler. exarch and synod carry no crate-level allow. Whether
  that boundary is the discipline's real edge or an artefact of which crates
  existed when it was drawn is open. The ADR's literal `-D
  clippy::disallowed_methods` is *not* used: a command-line `-D` escalates the
  lint onto the vendored `ral-ripgrep-core`, which deliberately opts out, and
  would break the build on vendored code.
- **A program's own files are no site at all.** What a program keeps for itself
  and what the user writes at the places it chooses go through `ral_core::own`,
  whose every syscall is a walk primitive, so the module holds no `site!`; every
  `std::fs` ban points there, and the env bans point at
  `ral_core::host::{var, var_os, vars_os}`. What the user names to it
  (`read_named`, `open_named`, `kind_named`, `create_named`) takes any path, so it
  is banned: each caller is a typed site whose variant says why the name is the
  user's (a CLI argument, a TUI command, the install tree), never a model's
  ([[decisions/261022_own-files|own-files]]).
- **Each exception is a typed site, surfacing or silent.** `site!`
  (`lint/site.rs`) exempts exactly one call, in one of two shapes:
  `site!(Silent::ArgvBeforeMain, std::fs::read("/proc/self/cmdline"))` or, for a
  method, `site!(Silent::WhichStat, &candidate => is_file())`. The ban names
  the call; the site names the place.
  - *The first argument is a variant, and the enums are the manifest.* Each
    crate that has sites defines the enums it uses, `Surface` and `Silent`, in
    its `site.rs`, which `include!`s the one macro file. `Surface` lists the
    sites that raise a card — the exec site that fuses an exec observation into
    the launch, and the open, staging and publish steps of the walk
    (`path/walk/`); `Silent` lists those that raise none — canonicalisation,
    `which` probes, module loading, stat predicates, the tree walk
    (`grep-files` raises its one card above it), capability load, sandbox
    respawn/exec, prelude bake, exarch/ral infrastructure. A variant's doc says what the place is and why it is
    exempt, so silence is a written decision, not an omission. The enums are
    crate-private, derive `Clone, Copy` alone and carry no list of themselves,
    so `dead_code` reports a variant no site uses.
  - *One exemption covers one call.* The receiver and every argument are
    evaluated outside it, so a banned call nested in an argument still fires,
    and a chain is split into one site per banned call.
  - *A stale site is an error.* The macro emits `#[expect]`, not `#[allow]`: a
    site that no longer wraps a banned call is reported as
    `unfulfilled_lint_expectations`, so an exemption cannot outlive its call.
    The decision is [[decisions/261022_typed-sites|typed-sites]].
- **Test, example and build code carries one blanket per unit.** Each file
  directly under `<crate>/tests/` opens with an inner
  `#![allow(clippy::disallowed_methods, reason = "test code")]`; a
  `#[cfg(test)]` module carries the outer `#[allow(…, reason = "test code")]`
  beside its `#[cfg]`; an example and a `build.rs` carry the inner allow with
  their own reason. Nothing finer is written inside them.
- **A scan states what clippy cannot: nothing else exists.** Clippy checks that
  every banned call is exempt, and cannot check *how*: the lint is one for every
  ban, and an allow names the lint, not the ban, so a hand-written allow over an
  `fs::read` passes it as readily as a site does. `core/tests/sites.rs` reads
  the Rust sources of every workspace member but `vendor/`, comments stripped,
  and fails on any occurrence of `clippy::disallowed_methods` that is not one of
  four: (a) in `lint/site.rs`; (b) an inner allow among the leading attributes of
  an integration-test crate root, an example, a `build.rs`, or the two guest
  crate roots; (c) an outer `reason = "test code"` allow on a `mod` item whose
  own `#[cfg(…)]` predicate implies `test` (exactly `test`, an `all(…)` with a
  `test` conjunct, or exactly `any(test, feature = "test-util")`; `any(test, unix)`
  holds in a production build and is refused); (d) the one third-party-codegen
  exception, the `#[expect]` on `tauri::generate_context!()` in
  `synod/src/main.rs`, pinned by file and by the `generate_context!` it
  precedes. Its expansion exits the process on a panic, and `site!` wraps
  calls, not macro invocations. A failure names `file:line` and the rule it
  breaks. Two more checks close the other ways round the lint's name. A test over
  the same sources flags every attribute that names `clippy::style`, where the lint is
  filed, or `clippy::all`, which holds it, and is not a `deny` or a `forbid`,
  since an `allow`, `expect` or `warn` of either group silences every ban
  without naming it. And a test reads the `Cargo.toml` of every workspace member
  but `vendor/`'s and fails one without `[lints]` with `workspace = true`, since
  a member outside the workspace's lints is outside the bans. The test file
  also pins two caller sets that a ban cannot say, since clippy scopes a ban to
  no module and counts nothing: the production files that read through
  `capability::Ungoverned`, and the files outside `path/` that call the string
  containment helpers.

A constructor under no site fails clippy, and an allow in place of a site fails
the scan, so outside the blankets and the one Tauri exception no exemption
exists that a variant does not name
([[decisions/260614_structural-bug-prevention|structural bug prevention]]). What
the lint cannot reach — the syscalls inside `ignore`/`tempfile`/bundled `uutils`,
and what *spawned children* do — is confined by the OS sandbox
([[decisions/260617_sandbox-external-children|sandbox external children]]), not
the lint.

## See also

[[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]] (the
decision), [[decisions/260619_surface-carries-documents|surface-carries-documents]]
(the card/mark grammar these surfaces compose from),
[[decisions/260618_run-turn-host-loop|run-turn-host-loop]] (core names the
operation, exarch the card), [[design/audit|audit]] (the one `Observation`
vocabulary this surface shares with the trail, `--audit`, and the wire),
[[map/exarch/cards|cards]] (the marks and the decoder),
[[map/exarch/shell-eval|shell-eval]] (the `decode_surface` seam),
[[map/exarch/builtins|builtins]] (the witness/search/edit atoms the bulk helpers
became), [[map/core/runtime|runtime]] (the redirect frame and exec completion
sites), [[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]],
[[decisions/260614_structural-bug-prevention|structural-bug-prevention]] and
[[decisions/260601_reduced-authority-witness|reduced-authority-witness]] (the
lint- and witness-discipline Enforcement extends),
[[decisions/260617_sandbox-external-children|sandbox-external-children]],
[[map/exarch|map: exarch]].
