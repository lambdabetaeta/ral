---
status: active
generated_at_commit: 446e3123
verified_at_commit: 446e3123
anchors: [GrantStack, admit, Admitted, Flag, SandboxProjection, ExecProjection, for_kernel, FreezeCtx, PolicyError, Denial, refused, record_check]
---

# Capability is data; the guard and the sandbox are its two consumers

**The capability model reads no session and renders nothing: `capability`
holds the lattice and the folds over it, and the in-process guard and the OS
sandbox are its two readers, so they cannot disagree about what a stack
permits.** The order is `capability < sandbox < guard`. Reinforces
[[decisions/260605_capability-stage-collapse|capability-stage-collapse]] and
moves the builder [[decisions/260605_witness-collapse|witness-collapse]] names,
`sandbox_projection`, below the guard that used to hold it.

## Context

Authority had no home. The model (`Capabilities`, `GrantStack`, the lattice)
lived in `types`, its enforcement in `capability`, and its sandbox projection
back in `types`; the three folds in `capability` (`exec::rules`, `fs::region`,
the deputy overlap) read the model from above and were rendered by a builder
that took a `Context`.

- **`Admitted`**, the proof that an exec was judged, was minted by the audited
  check, not by the judgment, so a launch could trust a value whose mint also
  wrote to the trail.
- **A grant string was frozen in two places**, `path/sigil.rs` and
  `capability/decode.rs`, each with its own relative-path, foreign-root and
  `HOME` rules; `PolicyError` sat in `types/flow.rs`.
- **Three builders of the capability observation**, one per door.
- **The kernel list was a third language.** `ExecProjection` carried an
  ordered `Vec<ExecRule>` that `kernel()` wrote and `from_kernel()` read back
  into the table it came from, because the model sat above the sandbox and
  could not be named from it.

## Decision

- **The model is `capability` (rank 3).** `Capabilities`, `GrantStack`,
  `Verdict`, `Meet`, `Widen`, and the two tables of
  [[decisions/261006_one-table-two-instances|one-table-two-instances]], named
  nothing in `types`. `GrantStack::admit` is the one exec judgment and the only
  mint of `Admitted`; a refusal is data (`Refused`, `ExecDenial`), worded by
  the guard. `GrantStack::admits(&Program)` is head-only admission and
  `respelled` the refusing deny's other spelling.
- **One boolean vocabulary.** `Flag { Detach, EditorRead, EditorWrite,
  EditorTui, ShellChdir }` with `GrantStack::permits(Flag)`: a meet over the
  layers, silence permitting. `Shell::check(Flag, who)` is its one door.
- **The projection is the sandbox's (rank 4).**
  `SandboxProjection::of(&GrantStack, &Resolver, Option<&Admitted>)` folds
  once, at spawn, off the same `region` and the very table that admitted the
  launch. `ExecProjection::Restricted` carries `ExecRules` itself, built by
  `ExecRules::for_kernel` (carriers admitted, each file at its final verdict),
  so `ExecRule`, `kernel` and `from_kernel` are gone; Seatbelt reads the table
  in `precedence` order, Landlock and the envelope read it as the guard does.
  The projection takes a `GrantStack` and a `Resolver`, never a `Shell`.
- **The guard is `guard` (rank 7).** `enforce` words the model's refusals and
  returns a typed `Denial { check, error }`; `Shell::refused` mints its
  `Break` and records it through `Shell::record_check`, the one door for a
  check, with no `Mooring`, so on the trail alone. A head admission passes
  its `Mooring` to the same door and so broadcasts.
- **A grant string becomes a `FrozenPath` in `guard::freeze`**, against one
  `FreezeCtx` (`FreezeCtx::of(&Shell)`, owned `home` and `cwd`): `path`,
  `paths`, `absolute`. `FrozenPath::from_surface` is the one minting door, and
  the `xdg:` escape guard asks the fs guard's own question,
  `from_surface(home).contains::<Allow>(resolved.real_path())`. `PolicyError`
  and the `gitdir:` refusals live there, `decode` and `SpawnGrant` beside it.

### Rejected

- **Leaving the projection in `capability`.** It would name the OS vocabulary
  (`Rendered`, the backends' `WriteReach`), and the model is data the backends
  read, not the other way round.
- **`ExecProjection` public.** It now holds `ExecRules`, which is `pub(crate)`
  with `Table` and `Scope`; the projection's `exec` field is crate-private and
  `test_access` answers the three questions the integration tests ask of it.
- **Flags as four forwarders and a verb gate.** One enum is one vote, one fold
  and one refusal wording.

## Consequences

- Guard and profile agree by construction: both ask `region` per op and the
  exec table `admit` judged with. What separates them is when the fold runs,
  afresh per check against a live `Resolver`, or once per spawn
  ([[design/two-enforcers|two-enforcers]]).
- The kernel's rules are the guard's table. The property that the rendered
  last-match list judges as the table and its carriers is tested over
  `precedence`, and the table `for_kernel` returns judges as the one it was
  built from.
- One door records every reported check, a refusal or a flagged deputy:
  `Shell::record_check(Option<&Mooring>, Check)` over `observe_stamped`. Only
  a head refusal holds a `Mooring`, so only it reaches the rail as well.

## Where

`core/src/capability.rs` and `capability/{lattice,exec,fs,table,deputy}.rs`
(`GrantStack`, `Flag`, `admit`, `Admitted`, `Refused`, `ExecDenial`,
`for_kernel`, `precedence`), `core/src/sandbox/projection.rs`
(`SandboxProjection::of`, `ExecProjection`, `FsRules`, `WriteReach`),
`core/src/guard.rs` and `guard/{enforce,shell,freeze,decode,grant}.rs`
(`check_exec`, `check_flag`, `Denial`, `Shell::refused`, `FreezeCtx`,
`PolicyError`, `decode_capability_map`, `SpawnGrant`), and
`core/src/types/audit/door.rs` (`Shell::record_check`).
