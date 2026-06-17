---
generated_at_commit: 2df6db85
generated_at_date: 2026-06-10
covers_paths: [exarch/src/policy.rs, exarch/src/policy/, exarch/src/prompt.rs, exarch/data/]
---

# Map: exarch / policy

`policy/` composes the session's effective [[design/grant|grant]] —
[[map/core/capabilities|ral's capability lattice]] — from the CLI flags:

```text
  ceiling   = base ∨ extend_base?
  effective = ceiling ⊓ restrict₁ ⊓ restrict₂ ⊓ ...
```

`for_invocation` composes the lattice in a fixed order:

- resolves the named base;
- joins an optional `--extend-base` (widens the ceiling);
- meets each `--restrict` file (attenuates).

Every profile is frozen as it loads — resolving each `~` / `xdg:` / `cwd:` /
`tempdir:` sigil against the session's home and working directory — so
composition runs on already-resolved `Capabilities`
([[design/capability-freeze|freeze boundary]]). Every restrict file's path
(lexical and canonical) is added to `fs.deny_paths`, so the agent cannot edit a
file that shapes its own authority. Loading reuses
`ral_core::capability::load_capabilities_from_*` — the same surface as ral's
`--capabilities <path>.ral`.

`policy/base.rs` embeds the five bake-in profiles from `exarch/data/*.exarch.ral`
via `include_str!`, ordered from most to least authority:

- `dangerous` — `Capabilities::root`, lattice top;
- `reasonable` — default; everyday tooling + standard binary dirs;
- `read-only` — reasonable's reads/exec, writes only to scratch;
- `minimal` — coreutils + cwd + scratch + net; a base for additive
  `--extend-base`;
- `confined` — offline build jail, exec by subpath only.

Each is a ral script whose terminal expression is a map shaped like the argument
of `grant [...] { body }`, loaded through
`ral_core::capability::load_capabilities_from_str` — the same surface
`--capabilities <path>.ral` consumes at the ral CLI. The in-module tests pin the
load-bearing per-profile properties (git admitted in reasonable/read-only,
bash/zsh denied despite `/bin/` in exec dirs, confined offline + subpath-only).
`exarch/examples/git.exarch.ral` is the canonical `--extend-base` that lifts
minimal/confined into a git-capable shape.

`prompt.rs::assemble` walks an ordered `(heading, body)` list, one section per
entry; the shape of the prompt is the shape of that Vec:

- **persona** (unheaded) — `data/system.md`;
- **Grant** — the `data/grant-legend.md` legend followed by a
  one-effect-per-line render of the frozen `Capabilities`, so the model can scan
  its authority. Ambient authority (the `dangerous` profile) collapses to a
  single "every command, path, network call permitted" line plus the scratch
  path. Grant sits directly under the persona so the model meets its constraints
  before the tool reference tempts it to overreach;
- **Host** — `host.rs::snapshot` (OS, date, cwd, user, git state);
- **Ral** — `data/ral.md`, the ral reference;
- **Headless** (when `--headless`) — `data/headless.md`, warning that assistant
  prose now *is* the program's output; appended last so its recency carries.

The set of builtins is *not* listed in the prompt: the agent discovers it at
runtime with `help`, which reads the live resolver and cannot drift.
`--system FILE...` collapses the persona and Ral slots into one user-supplied
section (the user takes over the tool reference).

`exarch/data/` also holds `agent.ral` (the embedded helper library, see
[[map/exarch/shell-eval|shell-eval]]), the grant legend (`grant-legend.md`), and
the TUI art (`banner.txt`, `eagle.txt`).
