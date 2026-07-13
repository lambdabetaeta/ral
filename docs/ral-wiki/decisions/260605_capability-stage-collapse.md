---
status: active
---

# Collapse the capability stage split into one always-frozen type

The capability layer carried two structurally-identical types: `RawCapabilities`
(syntactic, sigil-bearing) and `Capabilities` (resolved), with `freeze` the
one-way door and composition (`meet` / `join`) running on the raw form so a
single freeze settled the result. **The decision: freeze at decode time and
carry one `Capabilities` everywhere** — there is no syntactic stage, so the
boundary becomes a pass inside the one constructor rather than a type split.

This **supersedes** the same-day analyses that declined to unify (the
2026-06-05 wiki-log entries "why two types" and "what unification would
cost" — the log file is retired; git history keeps them). Those declined a
typestate `Capabilities<Stage>` — a `Raw` marker plus a serde `bound`
incantation — on legibility grounds, and they were right about *that* shape.
This move is different in kind: it does not parameterise the type, it **deletes
the stage**. With no `Raw` stage there is no `PhantomData`, no serde bound, no
stage-ambiguous `default` / `root` — the legibility cost the typestate carried
never arises.

## Why the stage was not load-bearing

The two-type design justified itself two ways; neither survived scrutiny.

- *Compose-then-freeze needs a pure, environment-free form.* But **every
  composition site already has the freeze environment in hand** — `eval_grant`
  (live shell), `apply_session_profiles` and exarch's `for_invocation`
  (`home_from_env()` + session cwd). Freezing each profile *as it loads* costs
  nothing those sites didn't already have.
- *Asymmetric serialisation keeps sigils off the wire* — only the resolved form
  derived `Serialize`/`Deserialize`. But if every `Capabilities` is resolved by
  construction, the wire is closed to sigils with one type just as well: there
  is no other form to exclude.

The only thing deferring freeze actually bought was a *semantic choice* about one
failable check — the XDG-escapes-`$HOME` guard. Deferred, it validated only the
composed survivor; an escaping `xdg:` path a later `meet` narrowed away was never
checked.

## The guard becomes a per-profile invariant

Freezing at decode makes the escape guard fire on **any** input profile naming an
`xdg:` path that resolves outside `$HOME`, survive composition or not. This is an
intended, fail-closed behaviour change: for a defence-in-depth control, refusing
the offending input at the door is the more honest semantic than letting its
rejection depend on what the lattice keeps. See [[design/capability-freeze|the
freeze boundary]].

## What the collapse removed

- the `RawCapabilities` type, its `freeze` / `freeze_in_env` / `validate_paths`
  methods, and `path::sigil::validate_xdg_tokens` (typo rejection still happens
  during freeze, via `parse_xdg_token`);
- the second instantiation of the `deny_all` / `is_restrictive` macro — inlined
  onto `Capabilities`, the macro retired;
- the `freeze` indirection at every call site: `decode_capability_map` now takes
  a `&FreezeCtx` and returns a frozen `Capabilities`; `meet` / `join` moved onto
  `impl Capabilities`.

Two further intended changes follow: `meet` / `join` now compare **resolved
paths** (strictly more precise — `/work/proj/src ⊂ /work/proj` is seen), and
**error attribution moves earlier**, from "invalid grant after composition" to a
per-profile decode/freeze error the caller's provenance prefix names.

Enforcement is unchanged — `core/tests/grant_policy.rs`, the `lattice_tests`
laws, and `ral/tests/capabilities.rs` pass before and after; the lattice
witnesses use concrete paths, so the laws never depended on the stage.

See also [[design/capability-freeze|capability-freeze]],
[[design/grant|grant]],
[[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]],
[[map/core/capabilities|capabilities]],
[[map/exarch/policy|policy]].
