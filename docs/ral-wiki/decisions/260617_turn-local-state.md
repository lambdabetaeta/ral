---
status: active
---

# Split `Shell` by lifetime; make turn state a field

**A turn installs one dynamic frame, so `Shell` should name that frame as one
field.** `eval_turn`'s guard ([`core/src/turn.rs`](../../../core/src/turn.rs))
currently saves and restores a scattered set of fields: captured IO and
`surface` from `Local`, the foreground `cancel`, and part of
`Location`. The source registry half of `Location` is deliberately kept so both
hosts can render runtime errors after `eval_turn` returns
([`ral/src/repl/exec.rs`](../../../ral/src/repl/exec.rs),
[`exarch/src/shell_eval.rs`](../../../exarch/src/shell_eval.rs)). The code works,
but the invariant is invisible: "the turn-local part" is encoded by parallel
assignments.

The better cut is not a small overlay. It is a `Shell` shape whose fields are the
lifetimes:

```text
Shell
├─ mobile   — persistable computation state
├─ turn     — the installed top-level turn frame
├─ session  — state that survives turn teardown
└─ local    — host-local scratch with its own flow rules
```

This is wider than the guard-only patch, but the final code has fewer and more
obvious invariants: `eval_turn` builds a `TurnState`, swaps `shell.turn`, runs,
publishes the cancel slots for the same extent, and restores the old `turn` on
drop.

## Context

The current split crosses lifetimes:

- **`Local` mixes turn, session, and scratch.** `io`, `surface`, and `cancel` are
  swapped for a turn; `durable_root` and `exit_hints` live for the session;
  `audit` and `repl` each have their own inherit/return rules
  ([`core/src/types/shell/mod.rs`](../../../core/src/types/shell/mod.rs)).
- **`Location` mixes cursor and registry.** `script`, `line`, `col`, `source`,
  `current`, and `call_site` are the active source cursor; `db: SourceDb` is the
  durable registry used at render time
  ([`core/src/types/audit.rs`](../../../core/src/types/audit.rs)).
- **`FrameGuard` knows the hidden membership.** `install` swaps IO only under
  `IoFrame::Capture`, always swaps the foreground cancel, saves four location
  fields, calls `install_root_context`, and restores the saved fields in `Drop`
  ([`core/src/turn.rs`](../../../core/src/turn.rs)).

The guard is a symptom. The missing type is the installed turn frame.

## Decision

Split `Shell` by lifetime:

```text
pub struct Shell {
    mobile:  Mobile,        // lexical scope, control, dynamic context
    turn:    TurnState,     // io, surface, foreground cancel, location cursor
    session: SessionState,  // durable root, source registry, exit hints
    local:   LocalState,    // audit and REPL/editor scratch
}
```

The field names are the invariants. A field either moves as a turn, survives a
turn, crosses evaluation boundaries, or stays as host scratch.

### `TurnState`

`TurnState` is the whole dynamic frame installed by a top-level turn:

```text
struct TurnState {
    io: Io,
    surface: Option<SurfaceSink>,
    cancel: ForegroundScope,
    loc: LocationCursor,
}
```

- **IO is turn-local.** `IoFrame::Capture` seeds `io` with buffers and an
  optional surface sink. `IoFrame::Inherit` seeds `io` from the ambient
  `shell.turn.io` / `surface`. The swap is total in both cases; only the seed
  differs.
- **The foreground scope is turn-local.** The scope is installed as
  `turn.cancel` for the turn's extent and published to the signal-reachable
  foreground slot for exactly that extent.
- **The location cursor is turn-local.** `LocationCursor` contains every
  non-registry source-position field: `script`, `line`, `col`, `source`,
  `current`, and the full `call_site`. `SourceLoc` construction reads the
  cursor; rendering later resolves the carried `FileId` against the durable
  registry.

The old `SavedIo: Option` disappears. Under `Inherit`, IO is still present in
the next `TurnState`; it is just inherited from the previous one.

### `SessionState`

`SessionState` is the part that must outlive every turn:

```text
struct SessionState {
    root: DurableRoot,
    sources: SourceDb,
    exit_hints: ExitHints,
    builtins: BuiltinTable,
}
```

- **`root` is the durable cancel root.** Detached workers parent under it; the
  foreground scope is always a child of it
  ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]).
- **`sources` is the durable source registry.** A turn resets and seeds it at
  turn start, module loads append to it, and hosts read it after the turn returns
  to render runtime errors. It is durable across the teardown of `turn.loc`, not
  across all future turns.
- **`exit_hints` is startup-loaded session data.** It is not part of the dynamic
  frame and is not restored by `eval_turn`.
- **`builtins` is the host-installed command table.** Builtin bodies are Rust
  function pointers or captured host closures; they are process-local dispatch
  state, not serialised ral values.

This removes the present `Location` half-split: the cursor is a turn field, the
registry is a session field.

### `Mobile`

`Mobile` stays the persistable computation state:

```text
struct Mobile {
    scope: Env,             // lexical value bindings
    control: ControlState,
    context: Context,       // env overrides, cwd, grants, handlers, args,
                            // modules; no Location, no builtins
}
```

`Context` no longer owns source position or builtin dispatch. The source cursor
is not mobile: it is installed by the current turn and discarded on teardown.
`SourceDb` is not mobile either: it is session render state and intentionally
skipped by IPC/serde. `BuiltinTable` is also not mobile: it is host/session
dispatch state, supplied by the receiving process rather than shipped in a
`WireMobile`.

IPC follows the same rule as today, but without a special case inside
`WireContext`: the wire mobile carries lexical scope, control, env/cwd/grants,
handlers, args, and modules. It does **not** carry builtin bodies. The receiver
starts from its own booted `Shell`, with core and host builtins already installed,
then installs the wire `Mobile` while preserving `session.builtins`. A
first-class builtin value, when one exists, crosses only as a synthesized ral
lambda/block that dispatches by bare name; the receiver resolves that name
through its own builtin table and handler stack.

### `LocalState`

`LocalState` is deliberately small:

```text
struct LocalState {
    audit: Audit,
    repl: ReplScratch,
}
```

It is not a lifetime category. It is the residue whose members already carry
their own flow manifests: audit has lexical ownership and capture policy; REPL
scratch is host/editor state. Keeping only those two here avoids renaming the
leftovers into a false "durable" bucket.

## The turn guard

`eval_turn` becomes a one-field swap:

```text
let next = frame.build_turn(shell)?;
let mut guard = shell.install_turn(next);
let _fg = publish_foreground(guard.shell().turn.cancel.as_scope());
let _rt = publish_durable_root(guard.shell().session.root.as_scope());
run;
```

`TurnGuard` owns the previous `TurnState` and restores it in `Drop`. The
publication guards live inside the same dynamic extent, and must drop before the
old `TurnState` is restored so the signal slots never point at a scope whose
owning frame has been removed.

This must be a `Drop` guard, not the existing `run_with_mobile` shape:
[`Shell::run_with_mobile`](../../../core/src/types/shell/inherit.rs) documents
that it does **not** restore on unwind. A turn guard must restore on unwind
because exarch catches worker panics and continues the session on the same
`Shell` ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]).

## Child shells and IO flow

Moving IO into `TurnState` does not mean same-thread children share the same
object blindly. The existing IO flow is load-bearing:

- stdout/stderr are cloned where possible;
- stdin is read-once, moved into the child, and returned to the parent;
- capture depth, terminal state, and job-control eligibility follow the child.

So `Io::inherit_from` / `Io::return_to` move under a `TurnState` flow manifest:

```text
TurnState::inherit_from(parent_turn)
TurnState::return_to(parent_turn)
```

Only the turn-local pieces participate: IO and `surface` follow the same-thread
body, and the foreground `cancel` is shared by same-thread work. A spawned
worker does not inherit the foreground `TurnState`; it builds a fresh one under
the durable root with its own IO and worker scope.

## Visibility

The split should not export new ways to violate the invariants. Visibility is
part of the design, not cleanup.

- **Raw fields are core-internal.** `Shell::{turn, session, local}` and the
  internals of `TurnState` / `SessionState` should be `pub(crate)` or private
  inside `ral_core`. Internal evaluator/runtime modules may write them directly;
  host crates should not.
- **The invariant-bearing handles are opaque.** `DurableRoot` and
  `ForegroundScope` are public handle types with private fields. A
  `ForegroundScope` is constructed only from a `DurableRoot`, so an unrelated
  root cannot be installed as a turn's foreground by accident.
- **Hosts get narrow accessors, not fields.** The public surface should be the
  operations hosts actually need:
  - build a foreground scope for a frame, clone/cancel/inspect it for deadlines;
  - read `SourceDb` after a turn for diagnostics;
  - load `exit_hints` and install host builtin sets during bootstrap;
  - snapshot and restore `Mobile` at exarch's clean tool-call boundary;
  - inspect terminal/interactivity state needed by frontends.
- **`Mobile` remains the intentional embedding seam.** Exarch already snapshots
  `Mobile` and restores it after a caught tool panic. Keep that seam explicit
  with methods (`mobile_snapshot`, `restore_mobile`) rather than exposing the new
  turn/session fields just because the old `Shell` fields were public.

This keeps the low-LoC internal shape while preventing the host boundary from
becoming another convention. The fields that encode turn safety are not a public
API.

## Consequences

- **The main invariant is one sentence.** A turn replaces `shell.turn`, runs, and
  restores the old `shell.turn`.
- **The source-location split is complete.** There is no struct with both
  turn-local cursor fields and durable registry fields.
- **The foreground/root relation is typed.** A foreground scope is minted from
  the durable root; detached workers use the root path, foreground work uses the
  turn path.
- **The broad rename is real.** Existing call sites move roughly as follows:

  ```text
  shell.local.io                      → shell.turn.io
  shell.local.surface                 → shell.turn.surface
  shell.local.cancel                  → shell.turn.cancel
  shell.local.durable_root            → shell.session.root
  shell.local.exit_hints              → shell.session.exit_hints
  shell.mobile.context.location.db    → shell.session.sources
  shell.mobile.context.location.*     → shell.turn.loc.*
  shell.mobile.context.builtins       → shell.session.builtins
  ```

- **The public host API shrinks after the move.** Some host code currently
  reaches directly into `shell.local` / `shell.mobile.context.location.db`; the
  cutover should replace those reaches with named methods rather than recreate
  the same visibility leak under the new field names.

## Alternatives considered

- **Keep the field-by-field guard.** Rejected: it preserves the hidden membership
  rule and makes every future turn-local field a manual save/restore hazard.
- **Add only a small `TurnLocal` overlay.** Rejected: it names the swapped bundle
  but leaves `Shell` itself lying about lifetimes. The code still says
  `local.io`, `local.cancel`, and `mobile.context.location`.
- **Move only IO/surface.** Rejected: it removes `SavedIo` but leaves foreground
  cancellation and source position as ad-hoc guard state.
- **Keep `Location` whole under `Mobile`.** Rejected: it keeps durable
  `SourceDb` and turn-local cursor fields in one struct, which is the core
  ambiguity this decision removes.
- **Make every field private immediately.** Rejected as a migration tactic:
  `Mobile` is already the embedding seam and many host/bootstrap paths configure
  it. Privacy should be tightened around the new invariant-bearing state first,
  then the older host conveniences can be narrowed separately.

## Open questions

- **Does the turn guard own signal-slot publication?** The publication lifetime
  matches the turn swap lifetime, so owning both in one guard is attractive. The
  implementation must still drop the publications before restoring the previous
  `TurnState`.
- **How much of `Mobile` should remain directly visible long term?** This
  decision keeps `Mobile` as the host snapshot seam, but the existing broad
  field visibility is larger than that seam. A later embedding-API cleanup can
  narrow it without changing the turn/session split.

See also [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] (the
turn lifted into `ral_core`), [[internals/a-turn-end-to-end|a-turn-end-to-end]]
(the turn flow), [[internals/evaluator-machine|evaluator-machine]] (the
Mobile/Local split this supersedes), [[decisions/260614_structural-bug-prevention|structural-bug-prevention]]
(the missing-type discipline), and [[decisions/260610_host-embedding-api|host-embedding-api]]
(the embedding seam hosts cross).
