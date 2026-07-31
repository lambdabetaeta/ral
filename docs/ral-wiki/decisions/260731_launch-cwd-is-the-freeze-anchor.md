---
status: active
---

# An agent's authority freezes at the cwd it started from, not the cwd it stands in

**Exarch freezes an agent's authority against the cwd it was *started* at — the
trunk against its launch cwd, a desk child against its session's live cwd at
the instant it was spawned — and never re-freezes on a `cd`.** One captured cwd
fans out to the capability ceiling, `AGENTS.md`/skills discovery, project
identity and logs, and `/export`'s anchor; the live shell seeded at that same
place drifts with every `cd` the model issues, but the grants that watch over
it do not follow. Realised in `exarch/src/lib.rs` (`run`), `exarch/src/policy.rs`
(`for_invocation`, `narrow`), `exarch/src/agent/seat.rs` (`boot_root_shell`,
`Seat::identity`), `exarch/src/fleet/desk.rs` (`agent-start`), `exarch/src/prompt.rs`
(`assemble`), `exarch/src/bootstrap.rs` (`project_dir`, `log_run_dir`), and
`exarch/src/tui/commands.rs` (`resolve_export_path`).

## Context

A trunk reads its cwd exactly once, at launch: `run()` calls
`std::env::current_dir()`, and the resulting string is threaded — not
re-derived — into everything that follows in the same invocation:
`policy::for_invocation`'s `FreezeCtx` (the ceiling `~`/`xdg:`/`cwd:` sigils
resolve against), `bootstrap::EXARCH.project_dir`/`log_run_dir` (the state and
log directories a project's runs share), `prompt::assemble`'s `AGENTS.md` and
skill discovery, and `SessionInfo.cwd`, which `tui::commands::resolve_export_path`
anchors a relative `/export` path against — its own doc comment says the
reason plainly: "anchor a still-relative path at the launch `cwd` rather than
the process's own." The live session shell is seeded at that identical spot
(`seat::boot_root_shell` calls `shell.seed_cwd(cwd)`), and from there it is
free to drift: a model's `cd` moves the shell's own notion of "here" for
every relative path it types next, and nothing in this list re-reads it.

A desk-spawned child is the same shape from the child's side. `HostServices`
is rebuilt fresh at every `ral` call from the *parent's* live directory —
`Agent::cwd()` probes the running shell rather than reading the parent seat's
stored `cwd`, "so a desk-spawned child starts where the model is." `agent-start`
narrows `policy::narrow(&s.caps, spec.grant, &s.cwd)` against exactly that
probed value, then `Seat::identity(shell, scratch, s.cwd.clone(), …)` seeds the
new child's shell there and stores the same value as the new seat's own `cwd`
field — the one `/clear` rebuilds from verbatim, never by probing again.

## Decision

- **`cd` moves where relative paths resolve; it does not move the anchor
  grants are frozen against.** `cwd:` in a profile "resolves to where the
  grant was established, so a later `cd` cannot retroactively widen authority
  — the body cannot widen by moving" ([[design/capability-freeze|capability-freeze]]).
  This page names the exarch-level instance of that rule: the anchor is not
  the live shell's cwd at enforcement time, it is the cwd in force when the
  agent came into being.
- **The trunk's anchor is the launch cwd**, captured once by `run()` and
  passed by value to every consumer above. Nothing downstream re-derives the
  anchor: the two process-cwd reads that remain — the TUI title bar's basename
  and the prompt's host-facts `cwd` line — are display-only, and exarch never
  chdirs its own process, so they can only restate the same value.
- **A desk child's anchor is its session's live cwd, taken at the moment it is
  started** — the parent's cwd as `agent-start` sees it when the spawn runs,
  which is where the child's own existence begins. `Seat::identity` fixes that
  reading into the child's own seat once, and `Seat::clear` (`/clear`'s engine
  half) rebuilds from the same stored field rather than probing the live
  shell again, so the child's own anchor does not drift either.
- **One rule, read from both ends: grants freeze where the agent was
  started.** A trunk's "started" is the process's launch; a child's is the
  `agent-start` call that gave it its own identity.

## Consequences

- Re-freezing on `cd` would silently re-anchor authority nobody re-granted —
  an agent that wanders outside its ceiling gains nothing by standing there,
  and one that wanders back out of a directory it was allowed keeps what it
  was given, exactly as `capability-freeze`'s `cwd:` sigil already guarantees
  one level down.
- A launch cwd that cannot be read is not silently absolutized to `.`; the
  launch refuses outright, because everything downstream — the ceiling, the
  project's log directory, `/export`'s anchor — would otherwise freeze
  against a fabricated "here" nobody chose.
- Project identity and logs stay keyed to the directory the session was
  launched from for its whole lifetime: `project_dir`/`log_run_dir` are
  computed once, so a model's `cd` cannot fragment one project's state or
  logs across two directories mid-session.
- `/export` keeps resolving a relative path against the launch cwd even after
  the model has `cd`'d elsewhere, matching what a user watching the launch
  directory expects to find.

## See also

[[design/capability-freeze|capability-freeze]], [[design/grant|grant]],
[[decisions/260731_one-walk-one-anchor|one-walk-one-anchor]].

Cite: `exarch/src/lib.rs` (`run`), `exarch/src/policy.rs` (`for_invocation`,
`narrow`), `exarch/src/agent/seat.rs` (`boot_root_shell`, `Seat::identity`,
`Seat::clear`), `exarch/src/fleet/desk.rs` (`HostServices::cwd`, `agent-start`,
`ExarchDesk::launch`), `exarch/src/agent.rs` (`Agent::cwd`), `exarch/src/prompt.rs`
(`assemble`), `exarch/src/bootstrap.rs` (`App::project_dir`, `App::log_run_dir`),
`exarch/src/tui/commands.rs` (`resolve_export_path`, `cmd_export`).
Read against f414ea84.
