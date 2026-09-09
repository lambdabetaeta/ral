---
status: active
generated_at_commit: 5b17290a
---

# `ral-core` is `pub(crate)` by default, so `dead_code` can see

**An item in `ral-core` is `pub` because a consumer outside the crate names
it, and `pub(crate)` otherwise. Visibility is not documentation of intent; it
is what makes the compiler able to tell us an item has no callers left.**
Every `pub` item is invisible to `dead_code` — the crate might be a library
someone links — so a crate that publishes its internals cannot be told when
one of them dies. Narrowing 657 of `core/src`'s 1676 `pub` declarations turns
the compiler back into that witness.

## Context

[[decisions/260610_host-embedding-api|host-embedding-api]] states the intended
surface exactly, and closes with "No builder, no config bag. The API is
exactly the missing type and the one kernel function." Counting generously —
`BakedPrelude` and its four methods, `boot_shell`, `bake_prelude_to_out_dir`,
the `baked_prelude!` macro, `HostSurface`, the run seam's twelve names
([[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]]) and the
compile seam's five — the intent names about 25 items.

`core/src` declared **1675**, of which 1644 were reachable from outside. The
ADR was never wrong; nothing could enforce it. And the price was not
abstraction leakage in the abstract: `sandbox::make_command` lost its last
caller on 2026-07-02 and was deleted two months later, because `pub` had
hidden it from `dead_code` the whole time. `Launch::clear_cloexec_on_spawn`
was the same story seven days old.

The measurement is a script, not a survey: rewrite every `pub` in `core/src`
to `pub(crate)`, compile all five consumers (`ral`, `exarch`, `synod`,
`vm-manager`, `guest-net`) with `--all-targets --keep-going`, widen back
exactly what the compiler names, repeat to a fixed point. `dev/` carries the
full measurement.

## Decision

**`pub` declarations in `core/src` fall from 1676 to 1019, `pub(crate)` rising
from 549 to 1208.** What stays `pub` is
one of five things, and each is a real constraint rather than a concession:

- **A consumer names it.** The residue after the fixed point.
- **A public enum's variant payload, or a public struct's field type.** A
  variant field's visibility is its enum's, so `ir::Val::List(Vec<ValListElem>)`
  keeps `ValListElem` public whether anyone names it or not. `rustc`'s
  `private_interfaces` lint — a *deny* here, since the workspace denies
  warnings — is what enforces this, and it is the binding constraint on how
  far the narrowing can go. Chasing it to a fixed point widened 32 items back.
- **A `#[macro_export]` body's path.** `dbg_trace!` and `baked_prelude!`
  expand in the host crate, so every path inside them must resolve there.
- **An example or a build script.** `ral/build.rs` and `exarch/build.rs` call
  `boot::bake_prelude_to_out_dir`; `vm-manager/examples/boot-smoke.rs` names
  `io::TerminalState` and `protocol::Machine`. CI compiles both, so a
  narrowing checked with `cargo check` on the libraries alone passes and then
  breaks the build.
- **The `scheme!` family.** Its arms expand to `pub fn`, and per-arm
  visibility would mean threading a `$vis` through every call site. Exempted
  deliberately: a base frame's row names each member, and a row is a caller,
  so no member of this family can be stranded silently the way a loose
  function can. The comment at the macro says so.

**Twelve dead items and seven dead re-export names are deleted**, each one
named by `dead_code` or `unused_imports` the moment its declaration narrowed:
`Launch::clear_cloexec_on_spawn` and its Windows twins
`Launch::{creation_flags, admit_handle}` — all three stranded by the same
commit, `c86106ba` "Pipeline stages are threads: retire re-exec" —
`Shell::{with_env, with_handlers, extend_env, native_names}`,
`EnvVars::native_names`, `Engine::events_shared`,
`Map::insert`, `AuditFragment::is_empty`, `ValMapEntry::value`, and the
re-exports `builtins::value_to_json`, `process::watch`,
`types::{DetachPolicy, Reservation, EnvVarsIter}`,
`typecheck::{CompDiff, CachedFreeVars}`. Every one of these types still
exists; it was the *re-export* that had no reader. `GrantStack::is_empty`, on
the dead list too, stays: clippy's `len_without_is_empty` requires it beside a
public `len`, which is a caller of a kind.

**One island is found and left standing.** Narrowing made the Windows
cross-check name a cluster the same `c86106ba` stranded —
`signal::windows::{try_reap_leader, break_pipeline_group,
disown_pipeline_group}` and the `ReapStatus` they return, unreferenced by core
or by any consumer on any platform. Retiring a process-group primitive is a
judgement for a host that can run the tests it belongs to, so these stay `pub`
— the same treatment `path::lex::PathShape` gets on the other side. The
narrowing found them; a Windows host should finish the job.

**An item reached only by platform-conditional or test code says so.**
`Wake::is_fired` becomes `#[cfg(windows)]` and `ExecProjection::carries_veto`
`#[cfg(target_os = "macos")]`, next to the `poll_beside` that was already
`#[cfg(unix)]`. Those whose only caller is a `#[cfg(test)]` module carry the
house's `#[cfg_attr(not(test), allow(dead_code))]`, narrowed to the platform
where that test runs when the test is itself gated. Nothing that a test still
exercises was deleted.

**`core/tests/*` reach internals through one enumerated door,
`core::test_access`,** gated on the `test-util` feature and turned on for
core's own dev targets by a self-dev-dependency in `core/Cargo.toml`. Core's
integration tests link `ral-core` as an external crate, so before this an
internal a test asserted on had to be public to every embedder — 19 items
were public for that reason alone. Each door goes through a `pub(crate)` item
*behaviourally*, never by handing out core's representation, per
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]:
a test is a consumer too. With the feature off the module does not exist, so
the items behind it stay `pub(crate)` and `dead_code` still names one whose
last in-crate caller went away.

**Twelve tests stop hand-seeding a shell.** `Shell::seed_default_env_vars` was
public because twelve `core/tests/*` files each built a shell with the
pre-ADR ritual — construct, seed, register — that `boot_shell` exists to
abolish. They now call one `common::fresh_shell` over `boot_shell` — as do
five more that had already spelled that call out for themselves — and the
method is `pub(crate)`: the highest-value item in the residue disappeared
rather than being relabelled.

## Consequences

- **`dead_code` is a live signal in core.** Had this been in place in July,
  the commit that moved `make_command`'s caller would not have compiled clean.
- **Narrowing exposes an item to the clippy lints that spare public API.**
  `avoid_breaking_exported_api` silences `unused_self`,
  `trivially_copy_pass_by_ref`, `unnecessary_wraps`, `struct_field_names` and
  `len_without_is_empty` on `pub` items. Twelve fired on the newly narrowed
  ones and were fixed rather than allowed, except three where the lint is
  wrong about this code and says so with a reason. `Shell::err`/`err_hint` —
  two `&self` methods that only called `Error::new` — are gone.
- **`clippy.toml` needed no change**: `redundant_pub_crate = "allow"` was
  already set, with the comment "an explicit `pub(crate)` documents reach more
  clearly". The workspace had already decided in this change's favour.
- **A narrowed item is dead per platform, so the cross-checks are load-bearing.**
  An item alive only on one platform reads as dead on the others, and the
  workspace denies warnings, so `check-windows` / `check-macos` /
  `check-linux` each named a set the host build cannot see. Thirty items
  now carry `#[cfg_attr(not(<where it lives>), allow(dead_code))]`, the
  annotation this repo already used — one truthful sentence per item about
  where its callers are, in place of a blanket allow.
- **The blind spot shrinks by 39%, it does not close.** 1019 `pub`
  declarations remain, and `dead_code` will keep missing a stranding among
  them. The measurement widens by *name*, so a name needed in one module keeps
  its homonyms public everywhere; an item-precise pass would go further.
- **Rustdoc is safe by prior choice**: `private_intra_doc_links = "allow"`,
  so a `[link]` into a now-private item still resolves.
