---
status: active
generated_at_commit: f4e88bce
verified_at_commit: f4e88bce
anchors: [ExecGrant, ExecRules, Verdict, Rank, Program, Subject, Head, Missing, Admitted, check_exec, admits_head, RealPath, meet_insert]
---

# Exec rules: one table, one function

Supersedes [[decisions/260806_a-head-has-three-identities|a-head-has-three-identities]].
[[decisions/260906_object-not-name|object-not-name]] made the `fs` guard
authorise the object; this does the same for exec.

## The law

> Decode turns each grant key into rules about programs. One function,
> `ExecRules::verdict`, gives the verdict for a program. The in-process guard
> calls it. The kernel gets the same rules as an ordered list. No other code
> makes a verdict.

- **Keys.** Decode keeps the author's grant as an `ExecGrant { names, paths,
  dirs }`: bare keys (`git`), frozen path keys (`/usr/bin/git`, `~/bin/x`),
  and frozen dir keys with the expansions of `path:` and `system:`. It rides
  the wire and exarch's prompt shows its spellings; nothing matches it.
- **Rules.** `ExecRules::compile` turns one layer's grant into a table over
  host files (`RealPath`), bundled tools, directories and vetoed names. A bare
  key is three rules at most: the file the *host* `PATH` finds (the process's
  own environment, never a scoped override), the bundled tool of that name,
  and for a deny a veto on the name everywhere. Path and dir keys are their
  frozen resolved forms, never re-read from disk. Every rule enters through
  `meet_insert`, so two keys reaching one file meet.
- **Precedence.** `Rank` is `Dir(depth) < Carrier < Exact < Veto`, and its
  derived `Ord` *is* precedence — the most-specific-match-wins of
  [[related/access-control-algebra|access-control-algebra]], made literal: among the rules that match, the greatest rank
  decides and equal ranks meet (`most_specific`, one fold). No match denies.
  A tool has no place on disk, so only its exact rule and a veto speak to it.
- **Stacking.** A stack's table is the pointwise meet of its layers'
  (`pointwise`), the restrict-is-meet of
  [[related/access-control-algebra|access-control-algebra]]: every key of either side is met at its verdict, so
  `verdict(a ∧ b, p) = verdict(a, p) ∧ verdict(b, p)` for every program. A
  property test over a fixed universe checks it. Compiled afresh on every
  question, since a bare key follows the host `PATH`.
- **Kernel.** `ExecRules::kernel` emits the same rules, carriers spliced in,
  sorted by `Rank` ascending (denies after allows within a rank), so the
  kernel's last-match-wins is highest-rank-wins by construction. A second
  property test checks a model of last-match-wins over that list against
  `verdict`. See [[decisions/261004_exec-carriers|exec-carriers]].

## Types

- **`Verdict`**: `Deny < Only(s) < Allow`. A directory carries a `bool`, since
  it cannot restrict argv; it enters a ranking as `Verdict::from`.
- **`Program`**: `Tool(name)` or `File { path, real }`, where `path` is what
  the launcher runs and `real` its `realpath(3)`. `Subject` is its borrowed
  view, what `verdict` takes.
- **`Head { shown, program: Result<Program, Missing> }`**
  (`runtime/command/head.rs`). A bundled name is its tool with no walk; any
  other bare name is the effective `PATH`'s hit; a path head is anchored at
  the launch cwd with only `.` folded, because the kernel walks a `..` after
  the links before it (Windows folds `..` itself and gets an absolute path).
  A head with no program carries `Missing::{NotFound, NotExecutable}`, never
  reaches the guard, and `vet` reports 127 or 126.
- **`Admitted`**: what `check_exec` returns, holding the program, its argv and
  the table that judged it, with private fields. `SpawnPlan` carries it,
  `build_command` and the sandboxed launch read program and args only from it,
  and the projection renders exactly its table. Nothing launches unjudged, and
  the launcher runs exactly the path whose real path was judged.

## Why

Before, the guard matched an enumeration of spellings against keys frozen in
yet another form, and the projection re-derived admission with its own fold.
Each spelling missing from the list was a bug and each on it a channel, and
two keys naming one file could disagree. One table judged by one function,
and rendered rather than re-derived, leaves no second opinion to drift.

## Consequences

- A symlink to an admitted file is admitted: it runs that file. A symlink
  under an admitted directory to a file outside it is not.
- A bare key is the host-`PATH` file, so `git: allow` admits `/usr/bin/git`
  typed as a path, and a scoped `PATH` that plants another `git` inherits
  nothing.
- A multi-call binary is one file, so a path key cannot tell its applets
  apart; a bare deny still vetoes by name.
- Missing is 127 and non-executable 126 under any grant, never a denial. On
  Windows a path head written without its extension that names no file is
  127, with a hint naming the suffixed file if one exists. Path keys there
  match by `RealPath` equality; the case and extension fold survives only in
  `name_key`, for bare names.

## Declared limits

- **A copy under a new name escapes a veto.** It is a different file holding
  no trace of the denied name, admitted wherever an allow dir covers it. What
  holds is the projection the copy inherits ([[design/grant|grant]]
  §Concessions).
- **The kernel cannot see argv or which bundled tool runs.** `Only` is an
  allow there, and a tool is ral's own binary.
- **Windows has no kernel exec layer**, so the window between check and exec
  stays open there.

## Where

`core/src/types/capability.rs` (`ExecGrant`, `Verdict`,
`meet_insert`), `core/src/capability/decode.rs` (keys to `ExecGrant`),
`core/src/capability/exec.rs` (`ExecRules`, `Rank`, `Program`, `Subject`),
`core/src/capability/enforce.rs` (`check_exec`, `admits_head`, `Admitted`),
`core/src/runtime/command/head.rs` (`Head`, `Missing`),
`core/src/path/real.rs` (`RealPath`).
