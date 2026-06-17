---
status: active
---

# One XDG base-directory resolver; exhaustive transport walks

Two routines resolved the same XDG base directories by two different rules:

- the `xdg:` [[design/grant|grant]] sigil went through
  `path::sigil::resolve_xdg`;
- the binary's own config/data loaders (rc file, history, plugin search,
  exit-hint overrides) went through `path::config::config_base` / `data_base`.

The two disagreed on the edges:

- `config_base` returned `$XDG_CONFIG_HOME` verbatim even when relative, read
  the process `$HOME` directly, and appended a `%APPDATA%` fallback;
- the sigil resolver honoured `$XDG_*_HOME` only when absolute (the XDG spec's
  rule that relative values are ignored) and fell back to the home-joined Linux
  defaults on every platform.

So a grant naming `xdg:config` and the rc loader could point a user at two
different directories.

Resolution: a single resolver, `path::basedir` (`XdgKind`, `resolve_xdg`), owns
the rule. `XdgKind::default_suffix` makes each kind carry its own Linux default;
`resolve_xdg` reads the override only when absolute, else joins the default onto
`home`. `path::sigil` and `path::config` both defer to it — `config_base` /
`data_base` / `xdg_config_subpath` / `xdg_data_subpath` are now thin wrappers
that keep a base only when it resolves to an absolute path. The policy is
XDG-everywhere including macOS (`xdg:config` is `~/.config`, no
`~/Library/Application Support`), matching how cross-platform CLI tools and
dotfiles use XDG. The relative-`$XDG_*_HOME`-verbatim behaviour and the
`%APPDATA%` fallback are dropped; `%HOMEDRIVE%%HOMEPATH%` is *not* added —
`$HOME`/`%USERPROFILE%` resolution stays in `path::home`. The legacy
`home_dot` (`~/.ralrc`) convention is unchanged: a dotfile, separate from XDG.

The same edit hardened the serde mirror in `core/src/serial.rs`. The three
hand-written walks that mirror the runtime `Value` to `SerialValue` now all
match their value type exhaustively. `from_runtime` already did; the
handle-sanitiser `value_carries_handle` (over `Value`) and the dependency
collector `collect_scope_deps` (over `SerialValue`) ended in `_ =>` catch-alls,
so a future container-shaped variant would compile while being silently not
descended — a spurious snapshot failure or a missing scope-dependency edge in
`build_arcs`. Listing every variant forces both walks to be reconsidered when a
variant is added. Behaviour is unchanged: closures (`Lambda` / `Block`) are not
descended for handles (their captured env is sanitised separately by
`intern_scope`) and contribute their `captured.scopes` as dependencies;
containers recurse; scalars do nothing.

Realised in `core/src/path/basedir.rs`, `core/src/path/sigil.rs`,
`core/src/path/config.rs`, and `core/src/serial.rs`. See
[[map/core/capabilities|capabilities]] and [[map/core/transport|transport]].
