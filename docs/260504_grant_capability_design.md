# Grant capabilities — design analysis and literature context

Written while planning the port of exarch's `.toml` capability format
down into ral as a first-class load primitive (`load-grant <path>`,
`--capabilities a.toml,b.toml`).  The two surfaces (toml file, inline
`grant` map) should be fully symmetric.  This note records why the
existing design is sound, what literature it draws on, and the one
non-orthodox choice that is load-bearing.

## One schema, two surfaces

The toml is the serde projection of `RawCapabilities`
(`core/src/types/capability.rs:340`) with `deny_unknown_fields`.
The inline grant map (`core/src/builtins/scope.rs::builtin_grant`,
parsing helpers in `core/src/builtins/caps.rs`) constructs the same
type from a ral map literal.  Both go through `RawCapabilities::freeze`
to resolve sigils against `FreezeCtx { home, cwd }` and produce a
runtime `Capabilities` frame that is pushed on the dynamic stack.

There is no schema translation step.  The toml *is* the runtime value,
serialised.

This avoids the drift pattern that affects systems where the config
language and the runtime API are designed separately (Kubernetes RBAC
YAML vs. client-go; Java `.policy` files vs. `Permission` subclasses).
Dhall, Cue and Nickel argue for this property explicitly under the
slogan "configuration is programming"; ral gets it for free because the
runtime type *is* the schema.

## Surface fields — schema reference

| TOML | Type | Inline grant key |
|---|---|---|
| `audit = bool` | `bool` (default false) | `audit` |
| `net = bool` | `Option<bool>` | `net` |
| `[shell] chdir = bool` | `Option<ShellPolicy>` | `shell: [chdir: …]` |
| `[editor] read/write/tui = bool` | `Option<EditorPolicy>` | `editor: [read: …, write: …, tui: …]` |
| `[fs] read_prefixes / write_prefixes / deny_paths` | `Option<FsPolicy>` | `fs: [read: …, write: …, deny: …]` |
| `[exec] "<key>" = <policy>` | `Option<BTreeMap<String, ExecPolicy>>` | `exec: [<key>: <policy>, …]` |

`ExecPolicy` toml encoding (default serde tag for the enum):

  - `"Allow"`         → `ExecPolicy::Allow`
  - `"Deny"`          → `ExecPolicy::Deny`  (sticky veto under meet)
  - `["sub1", …]`     → `ExecPolicy::Subcommands(_)`

Exec map keys come in three shapes:

  - bare command name (`git`, `cargo`)
  - absolute literal path (`/usr/bin/git`)
  - absolute subpath prefix, trailing `/` (`/usr/bin/`, `xdg:bin/`,
    `cwd:/`).  Subpath keys may carry only `Allow` or `Deny`;
    `Subcommands` on a subpath is rejected at `validate_paths` time.

Path sigils — `~`, `~user`, `xdg:{config,data,cache,state,bin}[/sub]`,
`cwd:[/sub]`, `tempdir:[/sub]` — are resolved by `freeze` against
`HOME`, the session cwd, and `std::env::temp_dir()`.  XDG values that
escape `home` are rejected at the boundary as defence in depth against
attacker-controlled `XDG_DATA_HOME=/etc`.

## Surface asymmetry today

The inline grant map is currently *less expressive* than the toml on
the exec dimension.  `parse_exec_grant`
(`core/src/builtins/caps.rs:146`) accepts only `Value::List` for the
policy: empty list → `Allow`, non-empty → `Subcommands`.  Bool is
rejected with a hint, string is not even a branch.  So
`ExecPolicy::Deny` is unreachable from inline grant; it enters only
through toml load or through lattice composition where one side
carried it.

`fs.deny_paths` and `net = false` are reachable from both surfaces.

Plan for the port: extend `parse_exec_grant` to also accept the strings
`"Allow"` and `"Deny"`.  ~5 lines, plus a sweep through the error
hint in the bool branch (which currently says "use [] to allow"
unconditionally).  The sticky-deny semantics already work correctly
under meet, so an inline `Deny` does the right thing if pushed before
another grant frame widens.

## Lattice composition

`RawCapabilities::meet` and `::join` (`capability.rs:523`, `:551`) are
verified commutative, associative, idempotent (tests in
`lattice_tests`).  Per-field rules:

  - `Option<T>`: `None` is identity for both meet and join (no opinion).
  - `bool`: meet is `&&`, join is `||`.
  - `FsPolicy.read_prefixes` / `.write_prefixes`: meet intersects (with
    longest-prefix-wins overlap), join unions.
  - `FsPolicy.deny_paths`: meet **unions** (more denies = less
    authority), join **intersects**.
  - `ExecPolicy` per name: a 3-element lattice with `Allow` at top,
    `Subcommands` partially ordered by inclusion, `Deny` at bottom.
  - Exec map: literal half follows per-key meet with sticky `Deny`
    (a one-sided `Deny` survives meet against absence on the other
    side); subpath half is prefix-set intersection.
  - `audit`: not a lattice element — propagates upward as logical OR
    on both meet and join.

Exarch's `for_invocation` orchestrator (`exarch/src/policy.rs`) is just
`base.join(extend_base).meet(restrict_1).meet(restrict_2)…` — a fixed
shape over this lattice.  Nothing in the orchestration is privileged;
when ral grows `--capabilities a.toml,b.toml` the natural semantics is
left-to-right meet.

## Literature mapping

### Capability foundations

  - Dennis & Van Horn (1966), *Programming Semantics for
    Multiprogrammed Computations* — capabilities as the primitive of
    authority transfer.
  - Saltzer & Schroeder (1975), *The Protection of Information in
    Computer Systems* — POLA (principle of least authority).  Every
    `grant` block is a POLA narrowing.
  - Levy (1984), *Capability-Based Computer Systems* — book-length
    survey.  KeyKOS, EROS, Coyotos descend from this lineage.
  - Mark Miller (2006), *Robust Composition* (PhD thesis) — the
    canonical articulation of why "attenuation only" is the correct
    composition rule.  Local reasoning is sound iff the frame stack is
    monotone-narrowing.  This is exactly what ral's meet enforces.
  - Miller, Yee, Shapiro (2003), *Capability Myths Demolished* —
    distinguishes ACLs, trademarks and capabilities.  ral is closest to
    ocap with named-string keys playing the trademark role on the exec
    side and absolute-path keys playing the strict-capability role.

### Operating-system precedent

  - **Capsicum** (FreeBSD; Watson et al., 2010) — process-wide
    capability mode entered via `cap_enter()`; per-fd rights via
    `cap_rights_limit`, monotonically narrowing.  The closest OS-level
    analogue to ral's frame stack.
  - **OpenBSD `pledge` / `unveil`** — coarse string-tagged permission
    classes plus filesystem visibility narrowing.  Both are
    monotone-only by construction.  Less expressive than ral's
    structured map; same monotonicity invariant.
  - **Linux Landlock** — composable filesystem rulesets;
    `landlock_restrict_self` may only narrow.  ral's projection on
    Linux compiles fs to bwrap + Landlock-shaped rules.
  - **macOS Seatbelt / sandbox-exec** — SBPL profile language,
    Lisp-shaped rule list.  ral's `SandboxProjection::bind_spec`
    renders to this.
  - **WASI preopens** — file capabilities resolved at module
    instantiation, not at use site.  Same hygiene principle as ral's
    `freeze`-at-boundary.
  - **Java Security Manager** — cautionary tale.  Used stack
    inspection (Wallach et al., *SAFKASI*, 2000; Fournet & Gordon,
    *Stack inspection: theory and variants*, 2003).  The bug was
    `doPrivileged`, an upward authority lift.  ral's `EffectiveGrant`
    folds via meet only; no upward lift exists, so the failure mode is
    precluded by construction.

### Lattice-based access

  - Denning (1976), *A Lattice Model of Secure Information Flow* —
    classic.  ral applies the same lattice machinery to authority
    rather than confidentiality.
  - Bell & LaPadula (1973) — lattice-based confidentiality model;
    structural cousin.

### Confused deputy

  - Hardy (1988), *The Confused Deputy* — a process with composite
    authority can be tricked into using one piece of authority on
    behalf of another.  ral mitigates by:
      - resolving exec to absolute paths before authority check
        (so a sandboxed child can't `PATH=/evil; cmd` past the gate);
      - making `xdg:bin` writable would create exactly the confused
        deputy attack — drop a binary, next call admits it via subpath
        — which is why `reasonable.toml` keeps `xdg:bin` read-only;
      - rejecting XDG vars that escape `HOME` at freeze time, so
        attacker-controlled `XDG_DATA_HOME=/etc` cannot widen authority.

### Configuration-as-value

  - Dhall, Cue, Nickel — typed config languages with the
    "config is just a frozen value" property.  ral has the same
    property without a config language: the toml is direct
    `serde::Deserialize` of the runtime type.
  - Anti-pattern (drift): Kubernetes YAML vs. client-go; Java
    `.policy` files vs. `Permission` subclasses.

## The 3-valued exec lattice

Most ocap systems are pure subset lattices: a frame either has a right
or it doesn't, and "absent" means "denied".  ral's exec dimension is
3-valued per name:

  - `Allow` (top)
  - `Subcommands(_)` (middle, partially ordered by inclusion)
  - `Deny` (bottom, sticky under meet)
  - "key not in map" (= top, "no opinion, inherit caller")

The reason both "absent = top" and "explicit Deny = sticky bottom" are
needed is that two distinct intents must be expressible:

  - "I am restricting fs; leave exec alone" — absent ⇒ inherit;
  - "the base ceiling vetoes `bash`, even if a restrict file does not
    mention it" — `Deny` must propagate against absence.

`meet_literal_exec` (`capability.rs:746`) handles this: a one-sided
`Deny` survives meet against absence; non-`Deny` one-sided entries
drop.  `meet` remains commutative, associative, idempotent — checked.

This is genuinely more expressive than orthodox ocap.  The cost is a
bespoke per-name meet rule (set intersection no longer suffices).  The
benefit is that a base profile can pin specific names out and trust
the veto across arbitrary restrict files written by people who do not
know about those names.

There is no clean precedent for this exact shape in the capability
literature.  The closest formal cousin is **default logic** (Reiter,
1980), where "absent ⇒ infer X" mirrors the absent-as-inherit rule;
the **negative authorisation** debate in RBAC (Sandhu et al., RBAC96;
Crampton on prohibitions) is the closest applied-security parallel.
The RBAC consensus, such as it is, accepts negative authorisations
provided they are monotone-sticky in one direction — which is exactly
what ral has done.

## Concessions and things to watch

  - **Bare command names are ambient**, not capabilities in the strict
    ocap sense.  `git` resolves through `PATH`.  ral mixes bare names
    and absolute keys for ergonomics; a paper-strength critique would
    point this out.  Probably not worth fixing — `reasonable.toml`'s
    deny-on-shells is the principled mitigation: the names you most
    care about are pinned, the rest depend on `PATH` integrity, which
    the fs deny on `xdg:bin` write protects.

  - **TOCTOU on path resolution.**  Standard sandboxing concern
    (Bishop & Dilger, 1996, *Checking for Race Conditions in File
    Accesses*).  Resolver-form `check_spec` and bind-form `bind_spec`
    are split on `SandboxProjection` precisely so the OS profile and
    the in-ral check work from the same source — but symlink races
    inside the admit set remain a known surface.

  - **Documentation surface.**  The 3-valued exec lattice
    (`Allow` / `Subcommands` / `Deny` / absent) is the single thing in
    this design that is not obvious from the type, and it is
    load-bearing.  When the toml/grant symmetry lands, this needs a
    user-facing paragraph in SPEC.md alongside the new
    `--capabilities` and `load-grant` documentation.

  - **Confused-deputy at the name layer (recurring).**  The interaction
    between subpath admit (`xdg:bin/` ⇒ `Allow`) and fs write to that
    same prefix is exactly the confused-deputy escape hatch; the
    invariant "no prefix is both `[exec]`-admit and `[fs]` writable"
    is a candidate for a `validate_paths`-time check.

## Plan summary

  - Move `load_capabilities_toml` from `exarch/src/policy/load.rs` into
    core (alongside `RawCapabilities`).  Trivial — it is already just
    `toml::from_str` plus the freeze step that lives in core.
  - Add `load-grant <path>` builtin returning a frozen `Capabilities`-
    shaped map suitable as the first argument of `grant`.
  - Add `--capabilities path[,path,…]` CLI flag.  Multiple files
    compose by left-to-right meet (narrows only — symmetric with the
    grant frame stack).  Pushed as a single top-level frame at session
    entry.
  - Extend `parse_exec_grant` to accept `"Allow"` and `"Deny"`
    strings; sweep error hints in the bool branch.  This closes the
    last asymmetry between toml and inline grant.
  - Leave `for_invocation` and the named base profiles
    (`reasonable`, `confined`, …) in exarch.  Those are policy
    opinions, not language primitives.
