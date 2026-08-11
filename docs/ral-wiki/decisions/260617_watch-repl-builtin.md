---
status: active
---

# `watch` is the REPL's builtin, not a vetoed core one

**A builtin a host cannot run should be absent from that host — not present
and refused at call time.** `watch` is an interactive-REPL affordance: it
streams a worker's output line-framed to a durable terminal sink. The host that
has no such sink should not resolve the name at all. The proposal moves `watch`
out of the shared core table and registers it only in the REPL, through the same
mechanism exarch already uses to add its agent tools, and collapses the frame's
detached-worker policy to its one remaining axis.

## Context

The active
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
made `watch` a runtime-gated builtin. It is registered in the shared
`CORE_BUILTINS` table (`core/src/builtins.rs:360`), so **both** hosts seed it;
`builtin_watch` (`core/src/builtins/concurrency.rs:184`) then refuses at call
time when the turn frame denies it
(`if shell.turn.detached.watch == WatchAdmission::Denied { … }`,
`concurrency.rs:186`).

That admission flag rides the frame. The detached-worker policy is a struct,
`DetachedPolicy { lifetime: Option<Duration>, watch: WatchAdmission }`
(`core/src/types/shell/mod.rs:170`), supplied by `TurnFrame` and installed into
`TurnState`. The REPL supplies `DetachedPolicy::interactive()`
(`watch: Admitted`, `mod.rs:181`); exarch supplies `DetachedPolicy::agent(1h)`
(`watch: Denied`, `mod.rs:187`).

The cost of this shape is a contract that lies. Under exarch the name `watch`
still resolves and typechecks, then always fails at runtime — the frame carries
a "this builtin is forbidden" axis for a primitive the host simply has no use
for. `watch`'s purpose — live, line-framed streaming to a durable terminal sink
(`ChildIoMode::Watch` → `Sink::LineFramed`, `concurrency.rs:70`) — is inherently
interactive: the REPL's stdout is the rustyline external printer, while exarch's
streams are per-call capture buffers a root-surviving watcher would write into
after the turn had already taken them.

The machinery to add a host's own builtins already exists. exarch installs
`EXARCH_BUILTINS` via `ral_core::builtins::register_builtins`
(`core/src/builtins.rs:444`) from `agent_builtins::install_on`
(`exarch/src/agent_builtins.rs:26`). A host adding builtins beyond the core set
is established, not novel.

## Decision

- **Move `watch` out of `CORE_BUILTINS` and register it only in the REPL.** A
  `REPL_BUILTINS` set, installed at REPL boot through the same
  `register_builtins` mechanism exarch uses for its agent tools, carries the
  `watch` entry. Core then provides the host-agnostic concurrency primitives —
  `spawn`, `await`, `race`, `cancel`, `poll` — and each host adds its own
  affordances: exarch its agent tools, the REPL `watch`. The registration model
  is symmetric.

- **Keep `watch`'s implementation in core.** The line-framed spawn machinery —
  `builtin_watch` (`concurrency.rs:184`), `spawn_child`, `ChildIoMode::Watch`,
  `Sink::LineFramed` — is general and stays private to core. Only *registration*
  moves: core exposes the `watch` `BuiltinEntry` so the REPL can install it,
  which keeps `builtin_watch` and `scheme::watch`
  (`core/src/typecheck/builtins.rs:870`) from having to become `pub`. This is the
  one asymmetry the proposal introduces — implementation in core, registration in
  the host.

- **Collapse `DetachedPolicy` to a single axis.** With `watch` admission gone
  from the frame, the detached-worker policy reduces to the per-host lifetime
  ceiling: the frame carries `detached_ceiling: Option<Duration>` (the REPL
  `None`, exarch `Some(1h)`). `WatchAdmission`, the `watch` field, the
  `interactive()`/`agent()` watch distinction, and the runtime gate in
  `builtin_watch` are all removed. At one field the `DetachedPolicy` struct is
  arguably not worth a name (see Open questions).

- **The result is that exarch genuinely lacks `watch`.** It is absent: not
  in exarch's builtin table, not in `help`, not in exarch's system prompt.
  Naming it gets no special treatment — ral is a shell, so a bare word that
  is not a binding, builtin, or handler falls through to external-command
  resolution (`exec_comp_ty` → `external_exec_comp_ty`,
  `core/src/typecheck/infer.rs`). `watch` under exarch is therefore an
  ordinary unknown command that fails at runtime with *command not found*,
  exactly as if it had never existed — not a builtin that resolves and
  refuses at call time. The frame stops carrying a "this builtin is
  forbidden" axis; the cost is that the failure is the generic
  unknown-command error, not the old tailored "use spawn" hint.

## Consequences

- The frame's detached-worker policy is one axis — a lifetime ceiling — not a
  policy struct with an admission flag.
- exarch lacks `watch` by absence: there is no typechecks-then-always-errors
  path, because there is no builtin. Naming it is an unknown command, not a
  vetoed primitive — the generic *command not found* at runtime, not a
  tailored diagnostic.
- `watch` becomes the one concurrency primitive registered out-of-band from its
  siblings: implemented in core, registered by the host. This is the cost —
  weighed against the removed frame axis and the honester contract.
- SPEC §13.5 gains "a builtin the host installs" in place of "admitted only by
  a host whose streams are a durable sink" — registration-by-absence, not a
  frame rejection.

## What shipped

The split landed on `turn-local-state`.

- **`watch` left `CORE_BUILTINS` for `core::builtins::WATCH_BUILTIN`** — a
  one-entry `&[BuiltinEntry]` core exports. The implementation
  (`builtin_watch`, `spawn_child`, `Sink::LineFramed`) and the scheme
  (`scheme::watch`) stay private to core; only the entry is public, so the
  one asymmetry holds — implemented in core, registered by the host.
- **The `ral` host installs it; exarch does not.** `register_host_surface`
  registers `WATCH_BUILTIN` process-wide (so the typechecker sees it)
  alongside the `_ed-*` editor builtins, and **both** the REPL session boot
  and the batch (`run_batch`) path install it into their shell — the ral
  binary has a durable stdout sink in every mode, so the axis is the
  **host**, not the interactive REPL alone. Read the ADR's "REPL" as "the
  `ral` binary, all modes." exarch installs only `EXARCH_BUILTINS`, so
  `watch` is absent there.
- **`DetachedPolicy` collapsed to `detached_ceiling: Option<Duration>`** —
  resolving the open question toward the bare field on `TurnFrame` /
  `TurnState`. `WatchAdmission`, the `interactive()`/`agent()` constructors,
  and the runtime gate in `builtin_watch` are gone; the frame carries one
  axis — the lifetime ceiling (`None` ral, `Some(1 h)` exarch).
- **The exarch failure is the generic unknown command** (see the corrected
  Decision bullet), not a compile-time diagnostic. The core
  `watch_is_denied_under_an_agent_frame` test is deleted, and the
  worker-thread-path `watch` test left the core layer with the builtin —
  that path is pinned by the `spawn` twin and `ral/tests/spawn_watch.rs`.

## Alternatives considered

- **Keep the runtime gate and `WatchAdmission` (the status quo).** Rejected: an
  extra frame axis and a compile-time lie for a builtin a host has no use for.
- **Move `watch`'s whole implementation into the `ral` crate.** Rejected: it
  would force `spawn_child`, `ChildIoMode::Watch`, and `Sink::LineFramed` to
  become `pub`, leaking concurrency internals across the crate boundary. Only
  `watch`'s *availability* is REPL-specific; its mechanism is general core
  machinery.
- **Derive admission from the IO regime (`Inherit` vs `Capture`) rather than a
  flag.** Rejected: it still requires retaining the regime discriminant on the
  frame and still leaves `watch` resolvable-but-failing under exarch — the same
  lie, differently spelled.

## Relationship

This decision **revises the `watch`-admission mechanism** of the active
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]].
Everything else there stands: the detached-root model, the one-hour death-clock,
the shared `process::reaper`, the `await`/`race` unification, and `forget`'s
deletion. That ADR's frame-gated `watch` and its `DetachedPolicy { lifetime,
watch }` are replaced here by registration-by-absence and the bare
`detached_ceiling`; its "What shipped" watch and policy bullets point forward
to this page.

## Open questions

- **`DetachedPolicy` — resolved.** Collapsed to a bare
  `detached_ceiling: Option<Duration>` on `TurnFrame` / `TurnState`; a
  one-field struct earned no name, and the `None`/`Some(d)` field is its own
  documentation.
- **Registry coverage.** A hand-written `Scheme` `BuiltinEntry` is not
  const-arity-checked the way the macro checks a `Sig` entry — it injects its
  declared arity verbatim. `WATCH_BUILTIN` declares `Some(2)`, matching
  `scheme::watch`'s `String → U(F α) → F (Handle α)`. Once the ral host
  registers it, `watch` rides `builtin_names()` for the arity sweep like any
  other entry; in the bare `ral-core` test binary it is unregistered, so the
  sweep (`registry_and_scheme_arity_agree`,
  `core/src/typecheck/builtins.rs`) does not see it — and `RESOURCE_BACKED` in
  `builtin_registry_property` dropped its now-dead `watch` line.

See also
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(the ADR this revises),
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] (the frame and
the root/foreground split), [[map/core/builtins|map: builtins]],
[[map/repl/loop|map: repl/loop]], and `docs/SPEC.md` §11.5.
