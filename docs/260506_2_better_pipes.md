# Brief: auto-foreground safe mixed pipelines

This is a briefing for an agent that has not seen the conversation that
produced it. Read all of it before touching code; the surface change is
small but the reasoning matters because the existing code is already
careful about a real Unix hazard, and the change relaxes that care
deliberately.

## What's already done

A previous refactor (`39babda`, `54b2649`) restructured ral's
process-outcome and pipeline-terminal-policy machinery. The relevant
landed pieces:

- `core/src/process.rs` defines `Signal`, `WaitOutcome` (`Exited`,
  `Signaled`, `StoppedThenKilled { stopped_by, killed_by }`,
  `NativeCode`), and `CommandFailure` (`ExitCode`, `Signal`,
  `StoppedByJobControl`, `Spawn(SpawnFailure)`). Wait results stay
  structured to the diagnostic boundary.
- `core/src/types/error.rs` carries `Status::{Code(i32),
  Process(CommandFailure)}` on `Error`. `Error::exit_code()` reduces
  at the boundary.
- `core/src/signal/unix.rs::wait_handling_stop` returns `WaitOutcome`,
  preserving both the original stop signal and the SIGKILL ral
  induced.
- `core/src/evaluator/pipeline/terminal.rs` defines `TerminalPlan`:
  `NoTerminal | ForegroundExternalGroup | ForegroundUnderEdTui |
  MixedNoForeground | RejectTerminalSensitiveMixed { stage_name,
  reason }`, plus a `terminal_sensitivity` heuristic that flags known
  interactive externals (`glow -p`, `less`, `more`, `man`, `vim`,
  `nvim`, `vi`, `nano`, `emacs`, `fzf`, `top`, `htop`).
- `core/src/evaluator/pipeline/analysis.rs::resolve_terminal_plan`
  emits the plan; rejection is raised before launch.
- `core/src/evaluator/pipeline/group.rs::PipelineGroup` is keyed on
  `TerminalPlan` and consults `should_foreground()`.
- Diagnostics and `ExitHints` no longer synthesise meaning from
  `status > 128`. SIGPIPE forgiveness is structural in
  `CommandFailure::from_outcome(_, !is_last)`.
- Integration tests in `ral/tests/pipeline.rs:480-548` cover normal
  exit 137, real SIGKILL, and stopped-pipeline-child variants.

The diagnostics half of the original problem is fully fixed: a
stopped-then-killed child now reads as a stop, not as SIGKILL/137.

## What's still wrong

The original user complaint was:

```
cat README.md | glow -p
```

worked in bash, ral failed. That specific command is pure-external and
already foregrounds — it's fine in the current code. The remaining
class is **mixed pipelines whose tail is a terminal-sensitive
external**:

```
to-lines $entries | glow -p
range 1 10 | to-lines | grep foo | less
echo "# title" | glow -p              # echo is internal in ral
```

Today these hit `RejectTerminalSensitiveMixed` and refuse to launch.
The reasoning baked into the existing `MixedNoForeground` policy is:

> Handing the tty to a pgid that excludes ral's threads would
> background those threads, and an internal stage that reads
> `fd 0 = /dev/tty` would SIGTTIN.
> — `core/src/evaluator/pipeline/group.rs` claim_foreground doc

That hazard is real but applies *only* to internal stages that
actually read from the controlling terminal. Most internal stages do
not. The current code's blanket refusal is over-cautious; it makes
otherwise-fine pipelines impossible.

## The structural observation

Whether an internal stage reads from `/dev/tty` is statically known by
the time `resolve_terminal_plan` runs. The pipeline analyser already
classifies every stage's edges:

```rust
// core/src/evaluator/pipeline/analysis.rs
pub(super) struct StageSpec {
    pub(super) comp_type: crate::ty::CompType,   // input/output Modes
    pub(super) dispatch: StageDispatch,           // External | Internal
    pub(super) incoming: Edge,                    // Bytes | None
    pub(super) outgoing: Edge,                    // Bytes | None
    ...
}
```

Inside `core/src/evaluator/pipeline/launch.rs::launch_internal_stage`
(around line 469) the stdin assignment is:

```rust
child_env.io.stdin = incoming_byte.map(Source::Pipe).unwrap_or(Source::Terminal);
```

So an internal stage's stdin is `Source::Terminal` exactly when:

- it has no incoming byte pipe (`incoming_byte` is `None`), AND
- mode unification chose to route bytes to its first stage.

Mode unification guarantees byte-mode input only flows from a
byte-output predecessor, so byte-mode-input on a non-first stage
implies `incoming = Edge::Bytes` (a real pipe). Therefore the **only**
internal stage that can be reading the terminal is one with:

- `dispatch == Internal`
- `incoming == Edge::None`
- `comp_type.input == Mode::Bytes`

By the same token, value-mode internal stages don't read stdin at
all, even though `child_env.io.stdin` is set to `Source::Terminal` —
they take their value from `acc` (the launcher's value accumulator)
or from arguments passed by `invoke`.

This gives a clean, decidable safety predicate:

```rust
fn pipeline_reads_terminal(specs: &[StageSpec]) -> bool {
    specs.iter().any(|s|
        !s.dispatch.is_external()
            && s.incoming == Edge::None
            && s.comp_type.input == crate::ty::Mode::Bytes
    )
}
```

If this is `false`, foregrounding the external pipeline group is
safe. The internal threads do not read fd 0; the kernel will not
SIGTTIN them.

(Note: `incoming == Edge::None` for the first stage of every
pipeline. So in practice this predicate is "the first stage is
internal and byte-input". But formulating it stage-wise is more
honest about what makes each stage safe and matches the launch-time
stdin assignment.)

## What this enables

Examples and their classification under the new predicate:

| Pipeline                                         | Pure-ext? | Reads tty? | Plan                          |
|--------------------------------------------------|-----------|------------|-------------------------------|
| `cat README.md \| glow -p`                       | yes       | n/a        | `ForegroundExternalGroup` (today) |
| `to-lines $xs \| glow -p`                        | no        | no         | `ForegroundExternalGroup` **(new)** |
| `range 1 10 \| to-lines \| grep foo \| less`     | no        | no         | `ForegroundExternalGroup` **(new)** |
| `echo "# t" \| glow -p` (ral's `echo` internal)  | no        | no         | `ForegroundExternalGroup` **(new)** |
| `from-lines \| glow -p`                          | no        | yes        | `RejectTerminalSensitiveMixed` (still) |
| `read \| glow -p`                                | no        | yes        | `RejectTerminalSensitiveMixed` (still) |
| `range 1 10 \| to-lines \| grep foo \| wc -l`    | no        | no         | `ForegroundExternalGroup` **(new)**, harmless |

The known-name `terminal_sensitivity` heuristic stops being
load-bearing for *safety*; it only decides the *wording* of rejection
when the new predicate flags the pipeline as unsafe and a known
terminal-tail is present. Externals that don't need the terminal can
be auto-foregrounded too — for them it's a no-op.

## Where to make the change

The decision lives in
`core/src/evaluator/pipeline/analysis.rs::resolve_terminal_plan`. Its
current body:

```rust
fn resolve_terminal_plan(specs: &[StageSpec], shell: &Shell) -> TerminalPlan {
    if !shell.io.interactive || !shell.io.terminal.startup_stdin_tty {
        return TerminalPlan::NoTerminal;
    }
    if specs.iter().all(|s| s.dispatch.is_external()) {
        return TerminalPlan::ForegroundExternalGroup;
    }
    if shell.repl.plugin_context.as_ref().is_some_and(|pc| pc.in_tui) {
        return TerminalPlan::ForegroundUnderEdTui;
    }
    if let Some((_, _, stage_name, reason)) = specs.iter().find_map(|spec| {
        terminal_sensitivity(spec)
            .map(|(stage_name, reason)| (spec.line, spec.col, stage_name, reason))
    }) {
        return TerminalPlan::RejectTerminalSensitiveMixed { stage_name, reason };
    }
    TerminalPlan::MixedNoForeground
}
```

Insert the predicate between the `in_tui` check and the
`terminal_sensitivity` reject:

```rust
// New: auto-foreground when no internal stage reads /dev/tty.
if !pipeline_reads_terminal(specs) {
    return TerminalPlan::ForegroundExternalGroup;
}
// Falls through to the existing reject / MixedNoForeground branches.
```

The reject branch is now reached only when an internal stage *would*
SIGTTIN. The diagnostic should reflect that — see "Diagnostic
adjustments" below.

`PipelineGroup` does not need to change: `ForegroundExternalGroup`
already routes through `should_foreground() == true`, and
`route_stdin` already keys off `group.owns_tty()` for the stdin slot
of external stages. Internal-stage stdin assignment in
`launch_internal_stage` also does not change — `Source::Terminal` for
non-first byte-mode stages is impossible by mode unification, and
value-mode stages don't read stdin.

## Subtleties to think through

These are the points I want the implementing agent to verify
deliberately, not just trust me on.

### 1. Backgrounded ral threads writing to the tty

When the pipeline pgid is foregrounded, ral's thread pgid is
backgrounded. Writes to a backgrounded tty don't SIGTTOU **unless**
`TOSTOP` is set in the line discipline. ral does not set TOSTOP, and
the inherited disposition is normally clear. Confirm by inspecting
`ral/src/repl.rs` (signal setup) and `core/src/io/terminal.rs`. If
TOSTOP is somewhere set, the auto-foreground may need to clear it for
the duration. Probably not — this is just the safety check.

### 2. Internal stages that bypass `child_env.io.stdin`

The predicate trusts that internal stages read stdin only via
`child_env.io.stdin`. A builtin that opens `/dev/tty` directly, or
calls `libc::read(0, …)` raw, would bypass the abstraction. Quick
audit: grep for `/dev/tty`, `STDIN_FILENO`, `libc::read`, `Source::`
in `core/src/builtins/`. Nothing should match outside `read` /
`read-line` / `from-lines`-style bytes consumers, all of which are
byte-input and would already be caught by the predicate. If the audit
turns up something else, decide whether it's really tty-touching or
just stdin-the-pipe.

### 3. SIGTTIN on writes from internal threads

Some shells set the terminal so that backgrounded reads SIGTTIN but
backgrounded writes are fine. ral does the same by default. If a
future change starts setting the line discipline more aggressively,
the predicate becomes insufficient. Document the predicate's
assumption clearly in the doc comment.

### 4. SIGINT delivery

When ral is foreground and Ctrl-C is pressed, SIGINT goes to ral's
pgid; ral's relay forwards to the pipeline pgid via `PipelineRelay`.
When the pipeline pgid is foreground (today, pure-external; under the
change, also some mixed pipelines), Ctrl-C goes to the pipeline pgid
directly via the kernel's terminal driver. `PipelineRelay` then
becomes a no-op for that case — confirm there is no double-delivery
hazard. The existing `ForegroundExternalGroup` path is the precedent;
the new auto-foreground case just reuses it. Should be fine.

### 5. The `_ed-tui` precedent

`_ed-tui` already foregrounds mixed pipelines. The reason it works
today is that the editor body is single-stage and the pipeline inside
it is whatever the user wrote — including potentially internal
byte-input first stages. Look at how `ForegroundUnderEdTui` is
produced and verify whether the predicate would say "unsafe" for
common `_ed-tui` bodies. If so, the predicate may need to be relaxed
inside `_ed-tui` (or the order-of-checks above already does that —
the `in_tui` branch is checked before the predicate, so `_ed-tui`
keeps its current behaviour and the predicate only matters for
non-`in_tui` mixed pipelines).

### 6. `wait_handling_stop` is still load-bearing

The predicate eliminates the SIGTTIN risk in the common case but
does **not** make ral immune. A buggy stage that does open
`/dev/tty` will still SIGTTIN; `wait_handling_stop` will still detect
the stop and tear down the pgid; the diagnostic will still say
`StoppedByJobControl(SIGTTIN)`. That fallback stays correct. The
change only reduces how often it fires.

### 7. Mode unification first

`resolve_terminal_plan` runs *after* mode unification (see the
analysis.rs flow: `validate_pipeline` → mode resolution → edge
freezing → `resolve_terminal_plan`). The predicate reads
`spec.comp_type.input` which is fully resolved at that point. Good.

## Diagnostic adjustments

The rejection diagnostic in `terminal.rs::into_error` says:

```
{stage_name}: this external stage needs the terminal, but the
pipeline also contains ral-internal stages
```

After the change, rejection only happens when an internal stage in
the pipeline would itself read the terminal. The message should name
that:

```
{stage_name}: cannot foreground this terminal-sensitive stage —
an earlier internal stage reads from the terminal.
```

Hint:

```
{tty_reader_name} reads stdin from the terminal, so foregrounding
{stage_name} would put {tty_reader_name} in the background and
SIGTTIN it. Read the input outside the pipeline (e.g. `let x = read;
to-lines $x | {stage_name}`), or feed {tty_reader_name} from a file
or pipe.
```

This requires identifying *which* internal stage trips the predicate.
The predicate already has to find one — return its name (or the head
identifier) alongside the `bool`. `StageSpec` doesn't carry a name
directly, but the elaborator knows the head identifier; consider
plumbing that into the spec (or recovering it from the IR `Comp` at
analysis time, which is already accessible — see `analyze_stage`).

The `terminal_sensitivity` known-name list is now redundant for
deciding whether to reject. Keep it for one purpose: when rejection
fires, the hint can mention the known-program name (so the user sees
"`fzf`" in the error rather than the path). Or delete it entirely —
the predicate-based diagnostic already names the offending stage.
Prefer deleting; it's dead surface.

## Tests

Live alongside the existing terminal-policy tests in
`core/src/evaluator/pipeline/terminal.rs` (unit) and
`ral/tests/pipeline.rs` (integration).

### Unit tests (terminal.rs / analysis.rs)

These should not require spawning processes. Build `StageSpec`
fixtures and assert on the resolved `TerminalPlan`. Borrow the helper
shape from the existing `terminal_sensitivity` tests.

1. Pure-external pipeline → `ForegroundExternalGroup` (regression).
2. `_ed-tui` body → `ForegroundUnderEdTui` (regression).
3. All-internal value-mode pipeline → `NoTerminal` or
   `MixedNoForeground` per the existing rule.
4. Mixed pipeline, internal value-mode first stage, external tail →
   `ForegroundExternalGroup` (new).
5. Mixed pipeline, internal byte-mode first stage with no upstream,
   external tail → `RejectTerminalSensitiveMixed` (still).
6. Mixed pipeline, internal byte-mode middle stage with byte upstream,
   external tail → `ForegroundExternalGroup` (new — verifies the
   `Edge::None`-only matters).
7. Mixed pipeline, internal value-mode first stage, ordinary external
   tail like `wc -l` → `ForegroundExternalGroup` (new — auto-foreground
   is harmless).

### Integration tests (ral/tests/pipeline.rs)

These invoke ral end-to-end. Keep them small and fast.

1. `to-lines [\"a\", \"b\"] | grep a` (mixed, no terminal-sensitive
   tail, no terminal-reading first stage) → exits 0, prints `a`. With
   auto-foreground this still works; the regression target is "the
   change did not break ordinary mixed pipelines."
2. `range 1 5 | to-lines | wc -l` → exits 0, prints `4`. Same
   regression target.
3. `to-lines [\"x\"] | cat` (mixed, external tail, foregrounds) →
   exits 0, prints `x`. Verifies auto-foreground actually runs the
   tail.
4. `from-lines | wc -l` (single-stage, byte-input from terminal) — this
   is single-stage, not a mixed pipeline; outside the predicate's
   scope. Skip.

Manual verification (PTY required):

```sh
docker exec -it shell-dev ./target/debug/ral
ral $ cat README.md | glow -p
ral $ echo "# title" | glow -p
ral $ to-lines ["# a"] | glow -p
ral $ range 1 5 | to-lines | less
ral $ from-lines | glow -p     # rejected with the new diagnostic
```

The first three should render. `range … | less` should give the user
an interactive `less` session. The last should fail before launch with
a message naming `from-lines` as the tty-reading stage.

## Out of scope for this change

- An explicit user grant (`_term-tail` or similar). The predicate
  closes the gap structurally; an explicit grant only matters if the
  predicate is ever wrong, and there is no evidence it is. If a real
  case turns up, that's a separate plan.
- Any change to `wait_handling_stop`, `WaitOutcome`, `CommandFailure`,
  or `Status`. They're fine as-is.
- Any reshape of `PipelineGroup`. The new predicate funnels into
  `ForegroundExternalGroup`, which already works.

## Suggested commit shape

The whole change is small enough to be a single commit. If you want to
keep tests separate:

1. `core/pipeline: auto-foreground mixed pipelines when no internal
   stage reads stdin from the terminal`
2. `core/pipeline: rephrase reject diagnostic to name the
   tty-reading stage`
3. `ral/tests: cover auto-foreground for mixed pipelines`

(Optional follow-up: delete `terminal_sensitivity` and the known-name
list once the predicate-based reject diagnostic is in place. Worth
its own commit so the diff is reviewable.)

## Risk summary for the reviewer

- **Predicate too loose** → an internal stage would SIGTTIN. The
  fallback is unchanged: `wait_handling_stop` detects the stop, tears
  down the pgid, and `StoppedByJobControl` reports it. Behaviour
  degrades to today's reject-after-stop, with a structured
  diagnostic. Worst case is a clear error one launch later.
- **Predicate too strict** → a pipeline that should foreground gets
  rejected. Same diagnostic as today. No new behaviour.
- **Foreground hand-off race** → the existing
  `ForegroundExternalGroup` path is the precedent. Reusing it for
  the new mixed case introduces no new race.

## What to hand the next agent if this lands

If after this change the user finds a case where the predicate is too
strict (a pipeline they expect to foreground but ral rejects), that
is the trigger for revisiting an explicit-grant builtin
(`_term-tail`-style). Until then, do not speculatively add one — the
language is small for a reason.
