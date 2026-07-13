---
generated_at_commit: c754c6b
generated_at_date: 2026-07-13
covers_paths: [core/src/runtime/command/io_event.rs, core/src/runtime/command/redirect.rs, core/src/evaluator/redirect.rs, core/src/runtime/command.rs, core/src/runtime/command/uutils.rs, core/src/types/shell/mod.rs, exarch/src/card.rs, exarch/src/shell_eval.rs, exarch/src/bus.rs, exarch/src/headless.rs, exarch/src/transcript.rs, exarch/src/tui/surface.rs, exarch/src/agent_builtins.rs, clippy.toml, core/tests/io_door_set.rs]
---

# Map: exarch / io surface

Every redirect read (`<`), every redirect write (`>` family), and every external
or bundled exec image the model launches surfaces on the rail — **one structural
event per logical operation**, the rail then coalescing a burst into one card per
kind. Coverage is a property of the **runtime**, not of kit discipline: the hooks
sit at the evaluator doors where the operation actually happens, so a
read/write/exec surfaces no matter which helper — or no helper — issued it. Core
emits a structural **I/O event** (a plain `Value` naming the
operation); exarch binds it to a card from the existing
[[map/exarch/cards|mark grammar]], exactly as it already binds a kit `` `card ``
([[decisions/260619_surface-carries-documents|surface-carries-documents]]). The
division is the one [[decisions/260618_run-turn-host-loop|run-turn-host-loop]]
draws — **core names the operation, exarch names its appearance** — so core
grows no card vocabulary and leaks no representation
([[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak]]). The
governing invariant: **a ral redirect means the model's own I/O and nothing
else**, held not by a flag but by *where code lives* (below). See the decision,
[[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]].

## The doors — core emits its own activity

Two operation classes, each with its runtime door. Emission is the one
`Shell::surface(&self, ev: Value)` method (`types/shell/mod.rs`), the public door
onto the turn-local [[map/core/shell-state|`SurfaceSink`]]; `emit_io` delegates
to it. The typed builders and the `WriteOutcome` enum live in one place,
`runtime/command/io_event.rs`. With no surface installed every door is inert.

- **Redirects** funnel through the frame combinators (`evaluator/redirect.rs`)
  over `open_file` and `install_stdin_redirect` (`runtime/command/redirect.rs`).
  A **read** (`< file`, fd 0) emits eagerly when the file opens — `{io:"read",
  path}`, no outcome — so it precedes the body it feeds. A **write** (`>`/`>>`/
  `>~`/`>|`, fd 1/2) is recorded as a `WriteIntent` on the `RedirectFrame` when
  the sink opens, and emitted when the **frame settles**, carrying the terminal
  outcome the door alone can know: `committed` (body ok — atomic `>` only after
  `commit_atomics` succeeds; `>>`/`>~` once the body succeeds), `aborted` (body
  failed before commit), `failed` (open or commit failed). Mode is `write` /
  `append` / `stream`. No byte count, no preview — path, mode, outcome.
- **Exec images** are hooked *after* resolution, at the completion doors, never
  at the call site (where the head may still resolve to a closure or builtin).
  The external / spawned-bundled path emits in `command::run`
  (`runtime/command.rs`) once `wait()` returns a status — `{io:"exec", argv:
  [prog, …args], outcome:"ok"|"bad", status}` (`ok` iff status 0; a spawn
  failure is `bad` with the synthesized 127/126/… code). The inline-bundled
  fast path returns earlier, so it emits its own exec event in
  `run_uutils_in_process` (`runtime/command/uutils.rs`) — exactly one event per
  exec, no double-fire ([[decisions/260616_bundled-tools-as-exec-images|bundled
  tools as exec images]]).

Not the capability gate (`audit_call`): it is the wrong granularity — it
over-fires on `source`/`use`/`exists`/`list-dir` and under-fires on bundled
coreutils' internal reads. The doors name exactly the model's operations.

## The io event — a Value, not a card

```
{io:"read",  path}
{io:"write", path, mode:"write"|"append"|"stream", outcome:"committed"|"aborted"|"failed"}
{io:"exec",  argv:[prog, …args], outcome:"ok"|"bad", status}
{io:"grep",  scope, pattern}                      # emitted by the grep builtin
```

Core already names this vocabulary in its capability layer, so reporting its own
activity adds no concept. The event is a public `Value::Map`, reaching the same
[[map/core/shell-state|sink]] as a kit `` `card ``.

## Binding to a card — exarch

`decode_surface` ([[map/exarch/shell-eval|shell-eval]]) is the shared surface
decoder: an `io`-keyed map decodes into the typed `IoEvent`
(`exarch/src/card.rs`) and is bound to a card by `io_card`, emitted as
**`Kind::Io { event, card }`** (`bus.rs`) so the bus event carries *both* the
raw structural event and the rendered mark tree. The other surface shapes (pin,
notice, card, done) have their own arms; a value matching none drops, the same
graceful degradation as before.

`io_card` composes from the existing marks ([[map/exarch/cards|cards]]). The
operation is a *nominal category*, so it is carried by a word, not a
mirror-orientation glyph: read is a `muted` `read` verb + a `path` span; write
reads `write <path> <outcome>` whatever its mode (the mode rides the recorded
event), the outcome roled `ok`/`warn`/`bad` for committed/aborted/failed — and
a *committed* write previews its content below the heading (`write_preview`): a
whole-file `diff` mark against the prior snapshot when core supplied one (an
atomic write over an existing file, both sides UTF-8 and under the read cap),
else the first lines of the new content as one `listing` mark; exec keeps the
conventional `$` prompt, the program as `path`, each arg as plain ink, and a
`→ status` tail roled `ok`/`bad`; grep is the pattern as `code` `in` the cwd
scope as `path`. `Role::Path` carries a real hue, so the subject of every row
stands as figure against the muted label and the body prose.

The TUI renders observation `Kind::Io`s not one card per event but **grouped by
kind**. Core surfaces each effect as its own `Kind::Io`, so a burst would read
as `read…`, `$…`, `read…`, `$…` — noisy clutter at the rail. An
`ObservationBuf` (`tui/surface.rs`, beside the patch buffer but kept separate)
buckets a consecutive run — even *interleaved*, order-independent — into deduped
buckets (reads by path, execs by argv, greps by `(scope, pattern)`), flushed at
the turn boundaries through per-kind group helpers into **one card per
non-empty kind** in a fixed Read → Exec → Grep order. Writes never join the
buffer: each write is its own block (its diff/listing preview is a barrier, not
a foldable observation). Each group reuses the exact `io_card` span vocabulary,
so a lone surface renders identically; the one departure is that the exec group
**drops the `→ status` tail** — a comma-joined run reads as the *set* of
commands run, and per-command status survives in the structured event. The
render path is shared with `Kind::Card` (`render_card`), so width-reflow and
the rest are free.

## One surface per operation — bulk plumbing below the ral line

The redirect frame cannot tell the model's `view-text 50 100 < foo.rs` from a
library helper's internal read — both install a read frame. The resolution is
**not** a suppression flag but the invariant *if a ral redirect always means the
model's I/O, library plumbing must not be a ral redirect*. So the bulk-I/O
helpers moved below the line into [[map/exarch/builtins|builtins]]
(`agent_builtins.rs`), where their reads happen in Rust and never reach the
frame:

- **`view-text`** reads the whole file in Rust (its adaptive-context witnesses
  depend on file-wide uniqueness) and surfaces its own single `{io:"read",
  path}` card — one logical read, one surface, matching the shape the redirect
  frame would have pushed.
- **`grep-files`** does one `fs::read` per matched file (the `search_tree` walk
  reads the bytes the search already needs) and emits **one**
  `{io:"grep", scope:".", pattern}` surface for the whole logical search — not
  one read card per file.
- **`edit-hash`** / **`edit-replace`**
  ([[design/hash-addressed-editing|hash-addressed editing]]) read, resolve,
  atomically rebuild, and write entirely in Rust through core's atomic write
  door (`Shell::atomic_write`) — the read is silent (a sub-step of one logical
  operation) and the door's single committed `write` event, carrying the
  old/new snapshots, is what renders as the whole-file diff card. With the
  editors below the line, **no** ral helper does internal I/O and no
  suppression mechanism exists anywhere.

The residual on the record: `source` / `use` read ral *code* via `read_to_string`
outside the redirect frame — code-loading, visible as its own statement, not
turn-time data I/O — and surface nothing by design.

## Machine log

`transcript.rs::event_record` serialises `Kind::Io` as `("io", {event})` in the
session-owned `transcript.jsonl`: the raw structural event (snake-case
`io`/`mode`/`outcome`) is the forensic record; the rendered card — and the
serde-skipped old/new preview bytes — are rendering, landing only in the TUI's
`user.log`. The model-protocol `events.json`
([[map/exarch/frontend|frontend]]) carries no cards and is unchanged.

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
operation, exarch the card), [[map/exarch/cards|cards]] (the marks and the
decoder), [[map/exarch/shell-eval|shell-eval]] (the `decode_surface` seam),
[[map/exarch/builtins|builtins]] (the witness/search/edit atoms the bulk helpers
became), [[map/core/runtime|runtime]] (the redirect frame and exec completion
doors), [[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]],
[[decisions/260614_structural-bug-prevention|structural-bug-prevention]] and
[[decisions/260601_reduced-authority-witness|reduced-authority-witness]] (the
lint- and witness-discipline Enforcement extends),
[[decisions/260617_sandbox-external-children|sandbox-external-children]],
[[map/exarch|map: exarch]].
