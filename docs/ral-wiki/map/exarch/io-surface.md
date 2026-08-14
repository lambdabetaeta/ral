---
generated_at_commit: 7d9410f0
generated_at_date: 2026-08-13
covers_paths: [core/src/types/observation.rs, core/src/evaluator/audit.rs, core/src/runtime/command/redirect.rs, core/src/evaluator/redirect.rs, core/src/runtime/command.rs, core/src/runtime/command/stdio.rs, core/src/types/shell/mod.rs, core/src/types/mooring.rs, exarch/src/bus/card.rs, exarch/src/bus/card/diff.rs, exarch/src/bus/card/value.rs, exarch/src/bus/card/decode.rs, exarch/src/bus/card/observation.rs, exarch/src/bus/card/done.rs, exarch/src/bus/card/notice.rs, exarch/src/bus/card/testkit.rs, exarch/src/shell_eval.rs, exarch/src/bus.rs, exarch/src/bus/post.rs, exarch/src/bus/inbox.rs, exarch/src/bus/event.rs, exarch/src/bus/channel.rs, exarch/src/bus/emitter.rs, exarch/src/bus/sink.rs, exarch/src/headless.rs, exarch/src/tui/surface.rs, exarch/src/shell_eval/builtins.rs, clippy.toml, core/tests/io_door_set.rs]
---

# Map: exarch / io surface

Every redirect read (`<`), every redirect write (`>` family), every external
or bundled exec image the model launches, and every denied head admission
surfaces on the rail — **one structural observation per logical operation**,
the rail then coalescing a burst into one card per kind. Coverage is a
property of the **runtime**, not of kit discipline: the hooks sit at the doors
where the operation actually happens, so a read/write/exec surfaces no matter
which helper — or no helper — issued it. Core emits a structural
**`Observation`** (`core/src/types/observation.rs`) — the one vocabulary
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

## The doors — core emits its own activity

Three operation classes, each hooked at the doors that realise it. Every door
builds one `Observation` (`types/observation.rs`) and hands it to `observe`
(`evaluator/audit.rs`), the single fan-out point. It judges nothing: the
observation goes to the run's `Mooring::surface` (`types/mooring.rs`) and onto
the open [[design/audit|audit trail]], each already inert when its consumer is
absent. **Which observations matter is the host's call**, made once in
`rail_place` (`bus/card/observation.rs`) and applied at `decode_surface` — so
a builtin command and an allowed capability check are reported by core and
dropped by exarch, never drawn and never journalled.

The one gate core keeps is its own: a capability check joins the trail only
when a grant asked for `audit: true`, which is language semantics, not
presentation. And only a head admission (`command_call.rs`) surfaces a
*structured* denial — the `fs` and full-argv checks in
`capability/enforce.rs` are entered from `types/shell/checks.rs`, whose
callers in `builtins/` and exarch's own doors carry no `Mooring`, so their
denials reach the trail alone. A refused *external* command still surfaces
either way, as a failed command observation carrying the denial message.

With nobody listening — no sink installed and no trail open — a door builds no
observation at all, so it pays for neither the `epoch_us()` syscall nor the
`script` and `principal` clones. With a host attached it pays them per
dispatch, builtins included.

- **Redirects** open through `open_file` and `install_stdin_redirect`
  (`runtime/command/redirect.rs`). A **read** (`< file`, fd 0) emits eagerly
  when the file opens — `Observed::Read { path }`, no outcome — so it precedes
  the body it feeds. A **write** (`>`/`>>`/`>~`, fd 1/2) surfaces **when
  its outcome becomes knowable**, which is one of two moments:
  - *A ral body* — builtin, closure, or a `> file` scope — runs inside the frame
    combinators (`evaluator/redirect.rs`): the open records a `WriteIntent` on
    the `RedirectFrame` and `settle_writes` emits when the **frame settles**,
    with the outcome the door alone can know — `committed` (body ok, an atomic
    `>` only once its commit succeeds), `aborted` (body failed before commit),
    `failed` (open or commit failed).
  - *An external command* fuses its redirects into the spawn instead
    (`wire_stdout_file` / `wire_stderr`, `runtime/command/stdio.rs`). A
    non-atomic target — `>>`, `>~`, a `>` outside the atomic recipe, any `2>`
    (`stderr_mode` coerces it to streaming) — has no later commit step, so it
    emits **eagerly at the open**, `committed`, no snapshots; a failed open
    surfaces nothing at all. Only an atomic `>` defers, settling post-`wait()`
    in `command::run`: `committed` with snapshots, `failed` on a broken rename,
    `aborted` when the child did not succeed and the staged temp is discarded.

  Mode is `write` / `append` / `stream`. No byte count — path, mode, outcome,
  plus the bounded content snapshots the write card previews and diffs from.
- **Commands** are hooked *after* resolution, at the completion doors, never
  at the call site (where the head may still resolve to a closure or
  builtin). Every command — builtin, external, or detached — is one
  `Observed::Command { argv, status, origin, .. }`: `argv` is the shown name
  first, then its arguments; `origin` is `builtin`, `external`, or `detached`.
  The external / bundled path — one door for both, since a bundled tool is a
  `ral --ral-bundled-tool` child like any host executable
  ([[decisions/260731_bundled-tools-always-reexec|bundled-tools-always-reexec]])
  — emits from `finish_command` (`evaluator/audit.rs`), which wraps the whole
  dispatch and so covers a spawn failure too, since that never reaches
  `wait()` (the card derives ok/bad from the status directly; a spawn failure
  carries the synthesized 127/126/… code). `detach` (`runtime/command/detach.rs`)
  is the second door, and the one that surfaces at the spawn rather than the
  wait: a surrendered process is never waited for, so its observation carries
  `origin: detached` and status `0` meaning *exec'd*, not *succeeded*. A
  **builtin** command is recorded into an open trail like any other, and
  reported on the sink like any other; `origin: builtin` is what the host
  drops at `decode_surface` — the rail reports doors to the world, not
  evaluation.

Capability checks are different again. An *allowed* check stays off the rail:
it is the wrong granularity — it over-fires on `source`/`use`/`exists`/
`list-dir` and under-fires on bundled coreutils' internal reads. A *denied*
check is the exception: it is the highest-signal line in a provenance record,
so a head admission reaches the rail whether or not a trail is open — see
[[design/audit|audit]].

A pipeline-stage helper's own doors reach the rail too, not only its trail —
but only when the parent already holds an open audit trail: the stage ships
`active_policy()`, `None` unless the parent is collecting, so with no
`audit { }` in force the child opens no trail and its fragment comes back
empty. When it does collect, the parent reports each merged observation
rather than folding it straight into the trail, so a stage's writes and
commands reach the host exactly as anything run locally does. They arrive at
stage settle, not at their original instant, but each observation's own
`start`/`end` still carry its true timestamp.

## The observation — a Value, not a card

Every observation carries a common envelope — `kind`, `script`, `line`, `col`,
`start`, `end`, `principal` — plus fields particular to its `kind`:

```
{kind:"read",             path, …envelope}
{kind:"write",            path, mode:"write"|"append"|"stream", outcome:"committed"|"aborted"|"failed", new_bytes?, old_bytes?, …envelope}
{kind:"command",          argv:[prog, …args], status, origin:"builtin"|"external"|"detached", stdout, stderr, error, value, …envelope}
{kind:"grep",             scope, pattern, …envelope}                      # emitted by the grep builtin
{kind:"capability-check", resource, decision:"allowed"|"denied", …fields, …envelope}
```

`argv` replaces a separate `cmd`/`args` split; a capability check's `resource`
and `decision` sit at the top level beside its resource-specific fields —
never spliced through a nested `value`, and a denial is never encoded as a
status. Core already names this vocabulary in its capability layer, so
reporting its own activity adds no concept. `Observation::to_value` is the one
projection — the same map shape the [[design/audit|audit trail]] and
`--audit`'s JSON use — reaching the same [[map/core/shell-state|sink]] as a
kit `` `card ``.

## Binding to a card — exarch

`decode_surface` ([[map/exarch/shell-eval|shell-eval]]) is the shared surface
decoder: a map matching the projection above decodes through
`Observation::from_value` into `Surface::Observation`, the raw observation
alone — no card built yet, since the decoder's own codomain carries the
structured value and nothing a printer merely wants a copy of. The card is
bound by `observation_card` only where a `Kind` is still needed for the bus's
still-live frontends (`Surface::into_kind`, emitting **`Kind::Io { event,
card }`**, `bus/event.rs`), and by `absorb_surface` never at all — the
observation records without one. The other surface shapes (pin, notice, card,
done) have their own arms; a value matching none drops, the same graceful
degradation as before.

`observation_card` composes from the existing marks ([[map/exarch/cards|cards]]).
The operation is a *nominal category*, so it is carried by a word, not a
mirror-orientation glyph: read is a `muted` `read` verb + a `path` span; write
reads `write <path> <outcome>` whatever its mode (the mode rides the recorded
observation), the outcome roled `ok`/`warn`/`bad` for committed/aborted/failed
— and a *committed* write previews its content below the heading
(`write_preview`): a whole-file `diff` mark against the prior snapshot when
core supplied one (an atomic write over an existing file, both sides UTF-8
and under the read cap), else the first lines of the new content as one
`listing` mark; a command keeps the conventional `$` prompt, the program as
`path`, each arg as plain ink, and a `→ status` tail roled `ok`/`bad` off the
observation's own `status`; grep is the pattern as `code` `in` the cwd scope
as `path`; a capability check reads `check <resource> <decision> <fields…>`,
the decision roled `Role::Bad` when denied (the only decision the rail ever
surfaces) and its trailing fields — core's own `resource`-specific map,
`name`/`resolved`/`args` for `exec`, `op`/`path`/`granted` for `fs` — rendered
as `key=value` pairs in the map's own order, whatever is present, nothing
inferred. `Role::Path` carries a real hue, so the subject of every row stands
as figure against the muted label and the body prose.

The TUI renders observation `Kind::Io`s not one card per event but **grouped by
kind**. Core surfaces each effect as its own `Kind::Io`, so a burst would read
as `read…`, `$…`, `read…`, `$…` — noisy clutter at the rail. An
`ObservationBuf` (`tui/surface.rs`, beside the patch buffer but kept separate)
buckets a consecutive run — even *interleaved*, order-independent — into deduped
buckets (reads by path, execs by argv, greps by `(scope, pattern)`), flushed at
natural boundaries through per-kind group helpers into **one card per
non-empty kind** in a fixed Read → Exec → Grep order. A capability check never
joins the buffer — rare and high-signal enough to earn its own line. A **write**
joins it but no group: its diff/listing preview is a barrier, not a foldable
observation, so it flushes as its own card, *last*, after the read/exec/grep
groups. That last position is the point. A redirect writes at the *seam*,
mid-call, so a write landed eagerly would sit between a call and the reads it
had yet to make — stranding those reads behind a barrier, where the coalescing
projection could not fold them into the run and the run's census would not count
them. Buffered, every effect of one call reaches
[[map/exarch/frontend|the projection]] contiguously and the barrier merely
*closes* the run. Each group reuses the exact
`observation_card` span vocabulary, so a lone surface renders identically; the
one departure is that the exec group **drops the `→ status` tail** — a
comma-joined run reads as the *set* of commands run, and per-command status
survives in the structured observation. The render path is shared with
`Kind::Card` (`render_card`), so width-reflow and the rest are free.

## One surface per operation — bulk plumbing below the ral line

The redirect frame cannot tell the model's `view-text 50 100 < foo.rs` from a
library helper's internal read — both install a read frame. The resolution is
**not** a suppression flag but the invariant *if a ral redirect always means the
model's I/O, library plumbing must not be a ral redirect*. So the bulk-I/O
helpers moved below the line into [[map/exarch/builtins|builtins]]
(`shell_eval/builtins.rs`), where their reads happen in Rust and never reach the
frame:

- **`view-text`** reads the whole file in Rust (its adaptive-context witnesses
  depend on file-wide uniqueness) and constructs its own single
  `Observed::Read { path }`, via core's public constructor rather than a
  hand-built map — one logical read, one surface, matching the shape the
  redirect frame would have pushed.
- **`grep-files`** does one `fs::read` per matched file (the `search_tree` walk
  reads the bytes the search already needs) and constructs **one**
  `Observed::Grep { scope: ".", pattern }` for the whole logical search — not
  one read card per file.
- **`edit-hash`** / **`edit-replace`**
  ([[design/hash-addressed-editing|hash-addressed editing]]) read, resolve,
  atomically rebuild, and write entirely in Rust through core's atomic write
  door (`Shell::atomic_write`) — the read is silent (a sub-step of one logical
  operation) and the door's single committed `Observed::Write` observation,
  carrying the old/new snapshots, is what renders as the whole-file diff card.
  With the editors below the line, **no** ral helper does internal I/O and no
  suppression mechanism exists anywhere.

The residual on the record: `source` / `use` read ral *code* via `read_to_string`
outside the redirect frame — code-loading, visible as its own statement, not
turn-time data I/O — and surface nothing by design.

## Machine log

There is no independent operational trace: the record log
([[map/exarch/frontend|frontend]]) carries an observation's total wire form
as a display commit so a resumed scrollback can rebuild its card, but never
the rendered mark tree itself — a rendering is not a fact.

## Enforcement — every door is accounted for

That "all I/O surfaces" holds is the conjunction of two mechanically-checked
facts, in the `clippy.toml` style already set for canonicalisation, cwd, and
child-wait.

- **All I/O goes through a known door (clippy).** `disallowed-methods` bans the
  fs/process *constructors* — `File::{open,create,create_new}`,
  `OpenOptions::open`, the one-shot `fs::{read,read_to_string,write,read_dir,
  metadata,symlink_metadata,read_link,remove_file,remove_dir_all,create_dir_all,
  rename,copy,set_permissions}`, `Command::new`, `CommandExt::exec`, and
  `ignore::WalkBuilder::build` (directory walks root at the one cancellable
  grep door). Every call site is then a door or a lint failure. Enforcement rides the pre-existing
  `[workspace.lints.clippy] disallowed_methods = "deny"` table (the four real
  crates opt in via `[lints] workspace = true`); plain `cargo clippy --workspace
  --all-targets` is the command CI runs. The ADR's literal `-D
  clippy::disallowed_methods` is *not* used: a command-line `-D` escalates the
  lint onto the vendored `ral-ripgrep-core`, which deliberately opts out, and
  would break the build on vendored code.
- **Each door is accounted for, surfacing or silent (reasoned allow).** Each
  allowlisted site carries an `#[allow(clippy::disallowed_methods, reason = …)]`
  whose reason opens with a stable tag — `[io-door:surface:<slug>]` (the redirect,
  exec, grep, and edit doors that fuse a surface into the operation),
  `[io-door:silent:<slug>]` (fs work that is not the model's data I/O —
  canonicalisation, `which` probes, module loading, stat predicates, capability
  load, sandbox respawn/exec, prelude bake, exarch/ral infra), or `[io-door:test]`
  (test scaffolding, blanket-allowed and not a door). The slug is unique within
  its file, so the tag is stable across line shifts. So silence is a written
  decision, not an omission.

A meta-test pins it: `core/tests/io_door_set.rs` walks the production `src/`,
checks every door allow is well-formed, and asserts the surface/silent door set
equals a checked-in manifest keyed by `(file, tag)` — stable across line shifts,
so only adding or removing a door perturbs it, and a new constructor call added
with a bare or missing allow fails CI
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
doors), [[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]],
[[decisions/260614_structural-bug-prevention|structural-bug-prevention]] and
[[decisions/260601_reduced-authority-witness|reduced-authority-witness]] (the
lint- and witness-discipline Enforcement extends),
[[decisions/260617_sandbox-external-children|sandbox-external-children]],
[[map/exarch|map: exarch]].
