---
name: exarch-sandbox
description: How to write a personal exarch sandbox rule (an `--extend` file) and where it goes. Use when a command fails with "denied by grant", "denied by active grant" or an OS-sandbox hint, when the user asks why exarch cannot read, run or reach something, or when they want exarch to have more (or less) authority by default.
---

# Personal sandbox rules for exarch

Your authority is a grant the user chose at launch: a built-in base (`--base`),
optionally widened by one personal file (`--extend FILE`) and narrowed by
`--restrict` files. Widening it is the user's decision, never yours. Explain the
denial, propose the smallest rule that fixes it, say where it goes, and let the
user add it. Do not write into their config, or anywhere else exarch reads,
unless they ask you to; never route around a denial meanwhile.

## What is in force

Your prompt's Host section lists the grant, layer by layer, with the legend.
The user's banner shows `base` and `extend`. Exarch discovers nothing on its
own: no `--extend` on the command line means no personal rules, and
`config.ral` holds provider endpoints only, never grants.

## Reading a denial

- `… denied by grant` names a path; `… denied by active grant` names a command.
- An OS-sandbox hint lists the kernel's lines, then a remedy per class: paths
  outside `fs.read` or `fs.write`, programs outside the exec allow-list,
  `` net: `offline ``, `` gpu: `withheld ``, a service, or a Unix socket. A
  pathname socket needs a stated fs write, e.g.
  `` fs: [write: [`absolute /path/to/socket]] ``; an fs deny still wins.
  Linux Landlock ABI < 9 does not enforce this pathname-socket limit.
  A service withheld by the base profile cannot be opened by a grant.
- The sandbox matches resolved paths. A file reached through a symlink, such as
  `~/.config/git/config -> ~/dotfiles/git/.config/git/config`, needs its real
  target granted; the hint already prints the resolved path.
- If an exclusion covers the path, no allowance helps; tell the user why the
  base excludes it instead of proposing a rule.

## The file

A `.exarch.ral` file is ral source returning a record. Every key is optional;
an omitted key changes nothing.

```ral
return [
  fs: [
    read:  [`home dotfiles/git],     # places readable
    write: [`home builds],           # places writable (and so readable)
    deny:  [`home .aws],             # exclusions: win over every allowance
  ],
  exec: [
    allow: [`name rg, `dir `homebrew, `absolute /opt/tool/bin/tool],
    deny:  [`name curl],
  ],
  net: `offline,                     # withhold the network
  gpu: `withheld,                    # withhold the GPU
  editor: [`read],                   # line-editor verbs: `read `write `tui
  shell: [`chdir],                   # session verbs: `chdir `detach
]
```

Places, frozen at launch: `` `home SUB ``, `` `xdg-config SUB `` (likewise
`xdg-data`, `xdg-cache`, `xdg-state`, `xdg-bin`, honouring `XDG_*_HOME`),
`` `cwd SUB `` (the launch directory), `` `gitdir SUB ``, `` `scratch SUB ``
(`$EXARCH_SCRATCH`), `` `tempdir SUB ``, `` `absolute P ``. `.` is the anchor
itself. Prefer an anchored place to `absolute`, so the file travels between
machines.

Exec targets: `` `name git `` (as `PATH` finds it at launch), a place naming one
file, or `` `dir PLACE `` and `` `dir SELECTOR `` for every program under a
directory. Selectors name tool roots: `system`, `homebrew`, `path` (each `PATH`
entry), and per-ecosystem ones such as `rustup`, `cargo`, `nvm`, `uv`, `pyenv`,
`ghcup`, `opam`, `nix`. Whatever may run may also be read.

## Semantics that matter

- **Additive.** An extension's allowances join the base's; they never remove
  anything. Its exclusions (`fs.deny`, `exec.deny`) are added too and always win.
- **The base's exclusions stay.** Credential denies (`gh`, `op`, `gcloud`
  configs, exarch's own credentials, exarch's binary) cannot be undone by any
  extension; do not re-state them.
- **Vetoes only withhold.** `` net: `offline `` and `` gpu: `withheld `` in a base
  cannot be restored by an extension, and one in an extension takes the network
  or the GPU away. `confined`, `minimal`, `read-only` and `edit-only` withhold
  the GPU; `reasonable` and `dangerous` do not. Windows cannot withhold it, and
  the prompt says so. Verb lists (`editor`, `shell`) join by union.
- **Silence permits.** An axis neither the base nor the extension names stays
  unrestricted. `--restrict` files apply after the extension and only narrow.
- **One extension.** Exarch takes a single `--extend` file per launch.
- **Writes end up run.** Caches are already redirected into scratch; never
  propose writing to `xdg-config`, `xdg-bin`, shell rc files or dotfiles. A
  writable directory that is also in `exec` lets the agent plant a program.

## Choosing a base

`reasonable` (default: reads home configs and caches, writes the tree, runs
`PATH` and toolchains, network on), `edit-only` (edits, no build tools),
`read-only` (writes scratch only), `minimal` (system tools, tree, network; no
home), `confined` (the tree, offline), `dangerous` (no confinement; only inside
another boundary). Start from the tightest base the work allows and extend it;
do not reach for `dangerous` to fix one denial.

## Where it goes

- Put it in exarch's config directory: `$XDG_CONFIG_HOME/exarch/`, else
  `~/.config/exarch/`, on every platform (`~` is `%USERPROFILE%` on Windows).
  Conventionally `personal.exarch.ral`.
- Never in a project tree or any directory the grant makes writable: exarch
  does not shield an `--extend` file from the agent, so a writable one would
  let the agent widen its own next session.
- Name it at every launch, e.g. through the user's alias or launcher:
  `exarch --base reasonable --extend ~/.config/exarch/personal.exarch.ral`.

## Checking a rule

A grant only narrows, so you may try a record's syntax in your own shell
safely: `grant [fs: [read: [`home dotfiles/git]]] { echo ok }`. The user can
check the file itself with `ral --norc --capabilities FILE -c 'echo ok'`, and
exarch refuses to launch on a malformed `--extend`, naming the file and the fault.

## Worked example: symlinked dotfiles

On macOS, `~/.config/git/config` links into `~/dotfiles`, so `git` fails with
"Operation not permitted" although `reasonable` reads `xdg-config`. The hint
lists `/Users/me/dotfiles/git/.config/git/config` under `fs.read`.

```ral
return [
  fs: [
    read: [
      `home dotfiles/git,   # ~/.config/git links here
    ],
  ],
]
```

Granting `` `home dotfiles `` whole fixes every linked config at once and is
reasonable only if that tree holds no secrets: check for ignored files (shell
history, `.env`, keys) before proposing it, and grant per package otherwise.
