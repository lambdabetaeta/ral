---
status: active
---

# One walk, one anchor: a PATH search has a single cwd and a single verdict

**A `PATH` search is one traversal, from one anchor, yielding one answer: the
anchor is a `SearchCwd` no call site may improvise, and the 126/127 verdict
travels out of the same walk that produced the resolution.** A textbook
application of [[decisions/260614_structural-bug-prevention|structural-bug-prevention]]
shape 1 — *the path authorised ≠ the path used* — realised in
`core/src/path/which.rs`, `core/src/runtime/command/{identity,vet}.rs`.

## Context

At the REPL, `cd` into a directory and name a file that lives there. It is not
on `PATH`, so the honest answer is `command not found`, 127. What came back was
`build.ps1: permission denied`, 126 — a diagnosis about executability, for a
file no walk had resolved, with `CreateProcess` never called. Three independent
defects composed:

- **Two anchors.** `identity::walk_path` and `policy_names` anchored relative
  `PATH` entries to `ctx.dir` — the `within [dir: …]` override *alone*, unbound
  in a plain REPL — while `vet::check_existence` anchored its own probe to
  `ctx.cwd_chain()`, the override *or* the `cd`-mutated cwd. The choice was
  per-call-site because the parameter was a bare `Option<&Path>`.
- **Two walks.** The verdict was computed by a second traversal
  (`file_exists_on_path`) independent of the one that produced `resolved`. Any
  divergence between them — anchor, search list, timing — presents as "on `PATH`
  but lacking `+x`", because that is the only shape in which walk one misses and
  walk two hits.
- **An empty `PATH` element.** The user's Windows `PATH` ended in `;`. Split,
  that is an empty entry, which `anchor_to_cwd` folded to the cwd — putting
  every file of the current directory on the search list of the *second* walk.

A fourth, independent: `windows_command_candidates` built `%PATHEXT%`
candidates with `Path::with_extension`, which **replaces** a suffix rather than
appending one, so `build.ps1` also matched `build.exe`.

## Decision

- **`SearchCwd<'a>` is the anchor type**, and every `PATH`-walking entry point
  takes it. Its constructors are few and named for provenance:
  `Context::search_cwd` (the `cwd_chain` precedence, for core runtime),
  `Resolver::search_cwd` (for a consumer already minted from a context),
  `SearchCwd::of` (a front end holding `Shell::cwd`), `SearchCwd::nowhere` (no
  shell). There is no constructor from a loose option, so `ctx.dir` no longer
  typechecks where a walk wants an anchor.
- **The verdict is a projection of the walk.** `path::search` returns
  `PathSearch::{Executable, FoundNotExecutable, Missing}`; `walk_path` calls it
  once and stores it on the `CommandIdentity` (`None` for a non-bare head, which
  `PATH` never searched). `vet::check_existence` takes no context at all — it
  pattern-matches the stored verdict. `file_exists_on_path` is gone, so there is
  no second walk left to disagree with the first.
- **An empty `PATH` element never means the cwd** — uniformly, not
  `cfg(windows)`-gated. `path_dirs` drops it, on the element as written, before
  anchoring. A user who wants the cwd searched writes `.`, which
  `anchor_to_cwd` honours deliberately.
- **`%PATHEXT%` appends, never replaces.** `with_appended_suffix` concatenates
  on the `OsStr`; `build.ps1` yields `build.ps1`, `build.ps1.EXE`, …

## The POSIX divergence, stated plainly

POSIX reads an empty `PATH` element as the working directory. **ral does not,
on any platform.** That is a deliberate break with a forty-year-old
implicit-`.`-on-`PATH` foot-gun, in the spirit of the repository's golden rule.
On Windows a trailing `;` is ubiquitous environmental noise no user authored as
a request; honouring it would make every file of every directory the shell
stands in a command — and off Unix, where `is_executable_file` accepts any
regular file, that is literally every file. The rule is one rule, and `.` says
what `.` means.

## Consequences

- **Freshness is unchanged where it was argued.** The executable half keeps
  `LOCATED`'s memo and negative TTL; the presence half stays uncached, and runs
  only when the walk missed — the error path, and bare names of bundled tools
  with no host twin. One stat per `PATH` entry, the same cost the deleted probe
  paid.
- **The memo keys differently, and more correctly.** A runtime walk in a plain
  REPL used to key `cwd: None`; it now keys the shell's cwd, so each `cd` starts
  fresh entries. Correctness over reuse — the anchor is part of the question.
- **Policy identity closes.** `policy_names`' host-`PATH` baseline is still its
  own traversal, because it asks a different question, but it now anchors where
  the dispatch walk anchors. The grant gate and vet judge the same resolved
  binary. `absolutize` moved to `cwd_chain` for the same reason.
- **Completion narrows on Windows.** With empty entries dropped,
  `commands_on_path` no longer offers every file of the cwd as a command.
- **The 126 refusal names the file it found**, since the walk kept it.
- **`Path::with_extension` remains available** to any future contributor; what
  is pinned is the behaviour, by `windows_tests::pathext_appends_never_replaces`.
  That fix is deliberately lighter-touch than the other three — a private helper,
  a test, and an arguing doc, rather than a candidate-iterator type that would be
  ceremony with one consumer.

## See also

[[decisions/260614_structural-bug-prevention|structural-bug-prevention]],
[[map/core/capabilities|capabilities]], [[map/core/runtime|runtime]],
[[design/grant|grant]].

Cite: `core/src/path/which.rs` (`SearchCwd`, `path_dirs`, `search`,
`PathSearch`, `windows_command_candidates`), `core/src/path/resolver.rs`
(`Resolver::search_cwd`), `core/src/types/shell/context.rs`
(`Context::search_cwd`), `core/src/runtime/command/identity.rs` (`walk_path`,
`policy_names`, `absolutize`), `core/src/runtime/command/vet.rs`
(`check_existence`).
