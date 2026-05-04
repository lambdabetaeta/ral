# Capabilities

How exarch decides what its agent is allowed to do, and how it stops the
agent from rewriting the rules.

## Threat model

Exarch hands an LLM a single tool — `shell` — that evaluates ral source
in a persistent shell.  Every call is wrapped in a typed `Capabilities`
frame pushed onto ral's capability stack and (where applicable) an
OS-level sandbox profile.  The threat we address here is *configured*
escalation: the agent rewriting its own permission file to hand itself
authority on a subsequent tool call or a subsequent exarch run.

We do *not* address: a Seatbelt/bwrap/seccomp kernel bug, a compromised
host, or out-of-band edits to the config (those are the user's
responsibility).

## Three desiderata

1. A `.exarch.toml` capability profile lives in the repo so it travels
   with the project — much like `claude/settings.json` in spirit.
2. Composition follows the **meet-semilattice** model.  Multiple
   profiles fold by intersection of authority; the operator is
   commutative, associative, idempotent.
3. **No centralised** profile dir under `$HOME`.  Each exarch run is
   `(base ∨ extend_base?) ⊓ restrict₁ ⊓ …`, with every input passed
   explicitly on the command line — nothing auto-loaded from cwd.

These are mutually compatible.  The trick is that "untouchable" follows
from a single structural fact: the agent's `write_prefixes` never
include the file's path.  Combined with a kernel-level deny rule,
rewriting the file requires either a Seatbelt/bwrap bug or
out-of-band access.

## Design

### `Capabilities` is a meet-semilattice

`Capabilities` (in `core/src/types/capability.rs`) is a bundle of
per-effect policies:

- `ExecPolicy`: per-command allow / subcommand list (the `exec` field).
- `exec_dirs`: directory-based exec allowance.  A command whose
  resolved absolute path is under any listed prefix is admitted with
  `Allow`.  Used to grant whole package-manager dirs (`/usr/bin`,
  `/opt/homebrew/bin`, …) without enumerating every binary.  Falls
  through only when the `exec` map has no name match — named entries
  take precedence so `Subcommands` restrictions can't be relaxed by
  a directory rule.
- `FsPolicy`: read prefixes, write prefixes, **deny paths**.
- net (tristate).
- `EditorPolicy`, `ShellPolicy` (boolean flags).
- `audit` flag (orthogonal — see below).

`Capabilities::meet(self, other)` is the semilattice operator.  The
elements of the lattice are bounded below by `Capabilities::deny_all()`
(no positive authority anywhere) and above by `Capabilities::root()`
(no attenuation: every `Option` is `None`).  `meet` works field-wise:

| field        | combinator                                                   |
|--------------|--------------------------------------------------------------|
| `exec`       | intersect names; meet per-command policies                   |
| `exec_dirs`  | intersect prefixes (longer-of-two on each chain)             |
| `fs`         | intersect prefixes (longer-of-two on each chain); union denies |
| `net`        | logical AND                                                  |
| `editor` / `shell` | AND each bool                                          |
| `audit`      | logical OR (**not** part of the lattice)                     |

`audit` is OR-combined because asking for an audit at any layer should
turn it on for the composition.  Documented separately as orthogonal.

The operator is verified by property tests in `core/src/types/capability.rs`:
commutative, associative, idempotent, `meet(_, root()) = id`, and
`meet(_, deny_all())` zeroes positive authority (deny lists are
upward-monotone, so they may carry over — what matters is that no
allow capabilities survive).

`meet_exec` and `meet_exec_policy` canonicalise their output (sort by
name, sort+dedupe Subcommands lists) so commutativity and idempotence
hold *literally* on canonical inputs, not just up-to-equivalence.
Without this, e.g., `meet(a, b) ≠ meet(b, a)` would fail when the
intersection has multiple shared command names.

### Composition at boot

`exarch/src/policy.rs::for_invocation` returns:

```
ceiling   = base ∨ extend_base?
effective = ceiling ⊓ restrict₁ ⊓ restrict₂ ⊓ ...
```

Two phases over the same lattice: a single optional join widens the
ceiling, then any number of meets attenuate from it.  Both phases are
commutative within themselves.  Composition is **explicit only** —
nothing is auto-loaded from cwd.

`base` is selected by `--base <name>`.  Three bake-ins, no directory
convention for adding more:

  - `minimal` — coreutils + cwd + /tmp + net + chdir.  The practical
    bottom for any session.
  - `reasonable` (**default**) — coreutils + everyday tooling
    (shells, git, curl/wget, search, scripting).  In spirit similar
    to Claude Code's default tool set.  Build drivers excluded.
  - `dangerous` — `Capabilities::root()`.  Lattice top; expects an
    outer trust boundary.  Combine `--base dangerous --restrict <FILE>`
    to "start permissive" via a TOML: `root ⊓ file = file`.

Each bake-in is authored as a TOML in `exarch/data/` and embedded at
build time via `include_str!`.  Edit the TOML and rebuild to change
what a base allows.  `minimal` and `reasonable` deliberately omit
`[fs]`; the resolver fills in cwd + /tmp + tempdir at runtime since
cwd is per-invocation.

`--extend-base <FILE>` (single, optional) is **joined** into the
ceiling before any attenuation, widening it for nonstandard tools.
This widens authority, so deny-paths are *not* installed for it —
trust the source.

`--restrict <FILE>` (repeatable) is **meet'd** with the (possibly
extended) ceiling.  Each restrict file's resolved path (lexical
*and* canonical) is appended to `fs.deny_paths`, so the agent gets
read access (it can inspect what constrains it) but cannot modify
any file influencing its own permissions.

`Capabilities` already derives `Serialize`/`Deserialize`, so a TOML
file is just a serialised `Capabilities` value.  No wrapper struct,
no per-profile schema:

```toml
[exec]
cargo = "Allow"
git = { Subcommands = ["log", "status"] }

[fs]
read_prefixes  = ["/Users/me/cache"]
write_prefixes = []

[net]
allow = false
```

### Structural untouchability

The deny for each `--restrict` file enters at three layers:

1. **In-ral check** (`EffectiveGrant::check_fs_write`, called via
   `Dynamic::check_fs_write` in `core/src/types.rs`): the path's
   resolution is compared against every `deny_paths` entry on the
   active capability stack.  Reads ignore `deny_paths` — only writes
   are denied, so the agent can still inspect its own profile.
2. **macOS Seatbelt** (`core/src/sandbox/macos.rs`): for each
   `deny_paths` entry the renderer emits
   ```
   (deny file-write* (literal "<path>"))
   (deny file-link*  (literal "<path>"))
   ```
   *after* the broad `(allow file-write* (subpath ...))`.  Seatbelt is
   last-match-wins, so the deny overrides the allow on exactly that
   path.  `file-write*` covers data writes, create, unlink, rename,
   chmod.  `file-link*` closes the hardlink/rename hole — without it
   an agent could `link("foo", ".exarch.toml")` and write through `foo`,
   reaching the same inode.
3. **Linux** (`core/src/sandbox/linux.rs`): bwrap has no negative path
   rule, so each `deny_paths` entry is overlaid as a `--ro-bind` of
   the file onto itself *after* the broad `--bind` of the writable
   parent.  Last mount wins — the same "deny overrides allow" shape
   as the Seatbelt profile.

### What is *not* defended

- **Tamper evidence**.  We chose prevention over vouching, per
  desideratum 3.  A `git pull` that updates `.exarch.toml` silently
  changes the policy on the next exarch run.  If you want a "policy
  changed" prompt later, the lattice gives you the comparison for
  free: `a ≤ b ⟺ meet(a, b) = a`.  The "last-trusted" state could
  live in `.git/exarch-last-trusted` to stay project-local.
- **Symlink games.**  Seatbelt's path resolution is path-based, not
  inode-based; subtle symlink swaps can theoretically mislead it.
  Adding the canonical form to `deny_paths` mitigates this for the
  common case but does not close every theoretical avenue.

## Module layout

- `core/src/types/capability.rs` — types + meet/join + property tests.
- `core/src/capability/` — runtime decisions: `EffectiveGrant` (single
  decision authority), `prefix` (`GrantPath`, canonical-aware
  intersection), `exec` (per-layer / stack verdicts), `check`
  (private helpers behind `EffectiveGrant`'s methods).
- `core/src/types.rs` — `Dynamic::check_fs_*`, `Dynamic::sandbox_projection`
  (thin shims forwarding to `EffectiveGrant`; entangled with the audit
  machinery, intentionally left here).
- `core/src/sandbox/{linux,macos}.rs` — bwrap and Seatbelt profile
  renderers, both consuming `&SandboxProjection`.
- `exarch/src/policy.rs` — `for_invocation` (returns
  `(Capabilities, Vec<PathBuf>)`), with submodules `policy/base.rs`
  (bake-in resolution + dynamic fs prefixes) and `policy/load.rs`
  (TOML loader + path utilities).  Bake-in TOMLs embedded via
  `include_str!`.
- `exarch/data/{minimal,reasonable,dangerous}.exarch.toml` — bake-in
  source.
- `exarch/src/cli.rs` — `--base <NAME>` (default `reasonable`),
  `--extend-base <FILE>` (single, joined), `--restrict <FILE>`
  (repeatable, meet'd).
- `exarch/src/main.rs` — `caps = policy::for_invocation(cwd, &c.base,
  c.extend_base.as_deref(), &c.restrict)?`.  No env-var fallbacks.
- `exarch/src/{eval,runtime}.rs` — pass `&Capabilities` directly; no
  wrapper enum.

## Naming convention

Each *field* of `Capabilities` is a `*Policy` (`ExecPolicy`,
`FsPolicy`, `EditorPolicy`, `ShellPolicy`).  The bundle itself is
`Capabilities` — one frame of the dynamic stack.  `SandboxProjection`
is the meet-folded effective fs+net authority used by the OS
renderer; the *Projection* name marks it as the type-level boundary
the sandbox backends consume, distinct from a per-frame policy.
`SandboxBindSpec` / `SandboxCheckSpec` are *parameter shapes* for
specific consumers (profile renderer / in-ral path checker), not
policies — hence `Spec`.

## Academic sources

The design borrows ideas from a small literature; nothing here is
novel.

- **Object-capability model** (Mark Miller, *E*; Dennis & Van Horn
  1966): authority is a value, lexically scoped, transferred by giving
  someone a reference.  ral's `grant { … }` and `Capabilities` stack
  are a dynamic version of this.
- **Capability attenuation / membranes** (Miller, "Robust composition";
  Caja project).  A profile is a function from `Capabilities` to
  `Capabilities` that can only weaken.  The semilattice structure is
  the formalisation of this.
- **Region capability calculus** (Crary, Walker, Morrisett, *Typed
  Assembly Language*; Tofte, Talpin, *Region inference*).  Static
  capabilities as proof tokens; the dynamic stack here is a runtime
  version with the same algebra.
- **Effects-as-capabilities** (Brachthäuser, Schuster, Ostermann,
  *Effekt*; Boruch-Gruszecki, Odersky, *Capt*).  First-class
  lexically-scoped capabilities tracked by types.  ral's `grant { … }`
  is the dynamic counterpart; we don't track scope statically because
  Rust's borrow checker doesn't help us at the ral-source level.
- **Dependency Core Calculus** (Abadi, Banerjee, Heintze, Riecke).
  History-based authority and effect colouring.  We don't use the
  monad, but the audit tree (in `audit { … }`) carries the same
  spirit: each effect is recorded with its authorising context.
- **Petnames** (Stiegler, Miller, Tribble; *Capability Myths
  Demolished*).  Considered but not adopted — desideratum 3 rules out
  any registry under `$HOME`.  If centralisation were ever allowed,
  the canonical-cwd-hash → human-name registry would be the right
  answer.
- **VS Code workspace trust / SPKI/SDSI signed capabilities**
  (Ellison; Rivest; Lampson).  Considered for tamper-evidence and
  rejected for now.  The structural deny carries the load instead.

The cheap stealable ideas we *did* take:

1. Capabilities as a meet-semilattice, with `meet` exposed as the only
   way to combine profiles.  Order-independent composition, by theorem
   not convention.
2. A first-class deny path inside `FsPolicy`, rendered as a
   kernel-level deny rule.  Closes the "writable cwd minus this file"
   gap that pure prefix-based policies can't express.

The rest of the literature — DIFC/Asbestos, Soutei/Datalog-based
authorisation logic, sealer/unsealer, linear capabilities — is
beautiful but solves problems we don't have.
