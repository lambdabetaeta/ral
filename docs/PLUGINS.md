# ral plugins

A plugin is a ral module (SPEC §8) whose return value is either a
manifest map, or a block that takes an options map and returns a
manifest map. `load-plugin` (§4) reads the manifest and registers the
plugin's hooks, keybindings, and aliases. There is no plugin DSL and
no magic config binding; a plugin's knobs are fields on its options
map and it extracts them by name.

## 1 Manifest

```
return { |options|
    let key = get $options key 'ctrl-t'
    return [
        name: 'fzf-files',
        keybindings: [[key: $key, handler: $_handler]],
    ]
}
```

A plugin that needs no configuration may return the manifest map
directly:

```
return [
    name: 'syntax-highlight',
    hooks: [buffer-change: $_handler],
]
```

| Field | Type | Default |
|---|---|---|
| `name` | `Str` | required |
| `hooks` | `[Str: {B}]` | `[:]` |
| `keybindings` | `[[key: Str, handler: {F Unit}, guard?: Str]]` | `[]` |
| `aliases` | `[Str: {[Str] → F Any}]` | `[:]` |

Every unmodified key except `f1`–`f12` requires a `guard`; an
unguarded binding on such a key is a load-time error (§6).

The `name` must be unique across loaded plugins.  The top-level
block, if present, takes exactly one argument: the options map.
Plugins extract fields by name (`get $options key <default>` or
`$options[key]`) and decide what is required vs. optional.

## 2 Authority

Plugins run with host authority. A hook, keybinding handler, or
plugin-registered alias executes under whatever capabilities the
caller's grant stack (SPEC §11) already holds — the manifest has no
field for declaring or narrowing that. A `capabilities:` key in a
manifest is a load-time error, so a plugin cannot mistake a listed
capability set for enforcement. Confinement is `grant`, applied at
the call site, not a manifest declaration.

`grant`'s `editor` field gates the `_ed-*` builtins (SPEC §11.6):

| Field | Enables |
|---|---|
| `read`  | `_ed-get`, `_ed-text`, `_ed-cursor`, `_ed-keymap`, `_ed-lbuffer`, `_ed-parse`, `_ed-history` |
| `write` | `_ed-set`, `_ed-set-lbuffer`, `_ed-insert`, `_ed-push`, `_ed-accept`, `_ed-ghost`, `_ed-highlight`, `_ed-state` |
| `tui`   | `_ed-tui` |

`grant`'s `shell` field gates shell builtins that modify persistent
process state (SPEC §11.7):

| Field | Enables |
|---|---|
| `chdir` | `cd` |

Plugin aliases run with ambient authority — no grant is pushed around
the call — matching the behaviour of `rc` aliases (§7). Hooks and
keybinding handlers likewise run under whatever `grant` is already in
force when they fire; a plugin author who needs `cd` from a hook or
keybinding handler documents that the enclosing session needs
`shell: [chdir: true]`, since the plugin itself cannot grant it.

## 3 The `_ed-*` family

Interactive only. Outside an interactive session every builtin raises
`<name>: not available outside interactive mode`. The full reference
is SPEC §18.1; the summary below is oriented around what plugin code
needs to know.

| Builtin | Shape | Purpose |
|---|---|---|
| `_ed-get` | `F [text: Str, cursor: Int, keymap: Str]` | read state |
| `_ed-text` | `F Str` | current buffer text |
| `_ed-cursor` | `F Int` | current cursor offset |
| `_ed-keymap` | `F Str` | current keymap name |
| `_ed-lbuffer` | `F Str` | text left of cursor |
| `_ed-set` | `[text?: Str, cursor?: Int] → F Unit` | partial buffer update |
| `_ed-set-lbuffer` | `Str → F Unit` | replace left-of-cursor, preserve right |
| `_ed-insert` | `Str → F Unit` | insert at cursor, advance |
| `_ed-push` | `F Unit` | save buffer, clear |
| `_ed-accept` | `F Unit` | run buffer on return |
| `_ed-tui` | `{F α} → F [output: Str, status: Int]` | suspend editor, run body, capture stdout |
| `_ed-history` | `Str → Int → F [Str]` | prefix search (limit 0 = all) |
| `_ed-parse` | `F [words: [Str], current: Int, offset: Int]` | tokenise buffer |
| `_ed-ghost` | `Str → F Unit` | set suggestion after cursor |
| `_ed-highlight` | `[[start: Int, end: Int, style: Str]] → F Unit` | set spans |
| `_ed-state` | `α → {α → F α} → F α` | per-plugin persistent cell |

Indices are character indices, consistent with `length` and `slice`.

**`_ed-push` + `_ed-accept`.** `_ed-push` saves the current buffer on
the shell's buffer stack and clears the editor; the next prompt
restores it. `_ed-accept` marks the buffer for immediate execution as
if the user had pressed Enter. The pair implements zsh-style
`push-line; accept-line`.

**`_ed-tui`.** The body runs with the line editor suspended and
stdout captured. On success the return record's `status` is 0 and
`output` is either the body's non-`Unit` return value or — when the
body returns `Unit` — the captured stdout (one trailing newline
stripped). When the body fails, `status` carries the exit code and
`output` carries the error message; the call never raises, so plugins
can discriminate cancellation (fzf 1 = no match, 130 = Esc) from real
errors without wrapping in `try`. Nested `_ed-tui` is reported as
status 1 rather than raised.

**`_ed-history`.** Entries are returned most recent first,
deduplicated. `_ed-history '' 0` returns the full history.

**`parse`.** Returns tokens of the simple command containing the
cursor. `current` is the token the cursor is in or immediately
after; `offset` is its character index in the buffer. Empty buffer
and unparseable input both yield `[words: [], current: 0, offset: 0]`.
The current implementation is a whitespace-aware tokeniser that
respects single and double quotes and splits on shell metacharacters;
it does not yet use the full ral parser.

**`ghost`.** Empty string clears. Ghost text is a display artifact,
not part of `text`. Last writer wins across plugins.

**`highlight`.** Each call replaces that plugin's spans. Valid
styles are:

```
command  builtin  prelude  argument  option
path-exists  path-missing  string  number  comment
error  match  bracket-1  bracket-2  bracket-3
```

Unknown style is an error. Out-of-range indices are clamped. Spans
from multiple plugins are composited by the shell; for overlaps the
plugin loaded later wins.

**`_ed-state`.** The first call runs the updater with the `default`;
each subsequent call runs it with the previously stored value. To
read without changing:

```
_ed-state $default { |s| return $s }
```

State is per-plugin and is cleared on unload.

## 4 `load-plugin` / `unload-plugin`

`load-plugin` and `unload-plugin` are host builtins the ral REPL
installs into its own shell's builtin table, not core builtins and
not prelude wrappers around anything else.

| Builtin | Shape |
|---|---|
| `load-plugin` | `Str → F Unit` |
| `unload-plugin` | `Str → F Unit` |

`load-plugin` resolves its argument in order:

1. `$XDG_CONFIG_HOME/ral/plugins/$name.ral` (falls back to `$HOME/.config/…`).
2. Each `$dir/$name.ral` for `$dir` in `RAL_PATH` (colon-separated).
3. As a literal path, with `.ral` appended if needed.

The module is evaluated with no options (`[:]`). If its return value
is a block, `[:]` is applied as its single argument; the result must
then be a manifest map. If the module's return value is already a
map, that map is the manifest directly. Unknown hook names are warned
on stderr and skipped; they do not fail the load. Invalid key
notation in a keybinding is a load-time error (§6); a keybinding
shadowed by an earlier same-chord binding loads with a warning naming
the shadower. Loading a plugin whose name is already registered is
an error; `unload-plugin` of an unknown plugin is also an error.

```
load-plugin 'syntax-highlight'
load-plugin 'fzf-files'
unload-plugin 'fzf-files'
```

`load-plugin` takes only a name — there is no way to pass per-plugin
options through it. A plugin that needs non-default options is
loaded through `~/.ralrc`'s `plugins:` list (§9), whose `options:`
field is forwarded to the plugin's top-level block.

## 5 Hooks

Declared as `hooks: [event: $handler, …]` in the manifest. Plugins
cannot register hooks at runtime.

Every hook handler takes exactly one argument. Event hooks receive
an event record; arity is checked at registration, so a handler of
any other shape is a load error.

| Event | Handler | Fires |
|---|---|---|
| `buffer-change` | `{Map → F Unit}` | after buffer or cursor changes |
| `pre-exec` | `{Map → F Unit}` | after Enter, before execution |
| `post-exec` | `{Map → F Unit}` | after execution completes |
| `chpwd` | `{Map → F Unit}` | after `cd` or a builtin `chdir(2)` |
| `prompt` | `{Str → F Str}` | before each prompt render |

All handlers for an event run in plugin load order regardless of
individual failures. A failing handler's error is logged as
`plugin 'name': hook 'event' failed: <message>`.

**`buffer-change`** receives
`[old_buf: Str, line: Str, pos: Int, history: [Str], keymap: Str,
state]`. Typical uses are highlighting and autosuggestion.

**`pre-exec`** receives `[src: Str]`, the full command line as
typed; **`post-exec`** receives `[src: Str, status: Int]`, adding
the exit status. **`chpwd`** receives `[old: Str, new: Str]`, the
old and new working directories.

**`prompt`** is a transformer, not an event record. Each handler
receives the current prompt string (starting from the shell's base)
and returns a new one. Handlers compose: the output of handler `n`
is the input to handler `n+1`.

```
# Append a git branch segment.
hooks: [
    prompt: { |base|
        let b = try { return $[!{git-branch}] } { |_| return '' }
        _if !{is-empty $b} { return $base } { return "$base [$b] " }
    }
]
```

## 6 Keybindings

Declared as `keybindings: [[key: $str, handler: $thunk, guard?: $regex], …]`.

**Key notation:**

| Notation | Meaning |
|---|---|
| `'a' … 'z'`, `'0' … '9'` | literal |
| `'ctrl-<c>'`, `'alt-<c>'` | modified letter |
| `'tab'`, `'enter'`, `'escape'`, `'backspace'`, `'delete'` | named keys |
| `'up'`, `'down'`, `'left'`, `'right'`, `'home'`, `'end'` | navigation |
| `'f1' … 'f12'` | function keys |

Invalid notation is a load-time error.

**Dispatch.** Plugin keybindings form one ordered dispatch table
shared by every REPL frontend: entries in plugin load order (manifest
order within a plugin), with the editor's built-in action as the
tail. A key press runs the first entry whose chord matches and whose
`guard` — a regex tested against the text left of the cursor —
allows; when no entry claims it, the built-in action runs. Several
guarded bindings on one chord compose as an ordered pattern match:
`key: 'tab'` with a guard cooperates with built-in completion, and
unmatched presses complete as usual.

A handler is `{|ctx| → F Unit}`; its return value carries nothing.
All effects flow through the `_ed-*` builtins (`_ed-set`,
`_ed-insert`, `_ed-set-lbuffer`, `_ed-push`, `_ed-accept`, `_ed-tui`,
…). Whether a binding claims a press is decided entirely by its
guard, before the handler runs — there is no return-value fallthrough.

**Load-time rules.** Three rules keep the table honest:

- `ctrl-c` and `ctrl-d` are reserved (interrupt, end-of-file) and
  cannot be bound, guard or not.
- Every unmodified key except `f1`–`f12` carries a ral-owned built-in
  editing action (typing, cursor movement, deletion, completion,
  history, accept-line, keymap escape); binding one requires a
  `guard:`, since an unguarded binding would shadow the action on
  every press. Modified chords (`ctrl-…`, `alt-…`) and function keys
  may be bound unguarded — replacing the underlying editor default
  (`ctrl-t`, `alt-c`, `ctrl-r`) is the point.
- A binding that can never fire — an earlier-loaded unguarded binding
  owns its chord — loads with a warning naming the shadower.

**What a handler may do.** Use `_ed-set` / `_ed-push` / `_ed-accept`
/ `_ed-ghost` / `_ed-highlight` to mutate editor state. Use `_ed-tui`
(gated by `grant`'s `editor.tui`, §2) to run a fuzzy finder or other
full-screen program.

After a handler that returns without `accept`, the shell re-enters
readline with the handler's final buffer and cursor.

## 7 Aliases

Declared as `aliases: [name: $thunk, …]` in the manifest. Each thunk is
called with a single `$args` list (same calling convention as `rc`
aliases). Aliases are merged into the shell's alias table at load time.

**Collision policy.** Loading a plugin whose `aliases` map names an
alias already present (from `rc` or a previously loaded plugin) is an
error. The load is rejected in full; no aliases from that manifest are
registered.

**Unload.** `unload-plugin $name` removes exactly the aliases that
plugin installed. Other aliases are untouched.

**Authority.** Plugin aliases run with ambient authority — no grant is
pushed around the call. This matches the behaviour of `rc` aliases.
A plugin alias that calls `cd` therefore always succeeds, regardless
of the caller's `grant` stack. A *hook* or *keybinding handler* that
calls `cd`, by contrast, needs the enclosing session to hold
`shell: [chdir: true]` (§2).

```
aliases: [
    z: { |args|
        if !{is-empty $args} {
            cd ~
        } else {
            let here = !{cwd}
            let result = try {
                return !{zoxide query --exclude $here -- ...$args | from-line}
            } { |_| return "" }
            if $[not !{is-empty $result}] { cd $result }
        }
    },
]
```

## 9 Prelude helpers

The `_ed-*` family is direct builtins (see §3); plugins call them by
name with no prelude indirection. `load-plugin` and `unload-plugin`
are likewise direct host builtins (§4), not prelude wrappers. Plugin
code does reach for one genuine prelude helper:

```
elem   x items   -- membership test
```

## 10 `~/.ralrc`

The config map (SPEC §9) accepts an optional `plugins` list.  Each
entry is a map `[plugin: Str, options?: Map]`:

```
return [
    env: [EDITOR: 'nvim'],
    plugins: [
        [plugin: 'syntax-highlight'],
        [plugin: 'fzf-files',      options: [key: 'ctrl-t']],
        [plugin: 'fzf-cd',         options: [key: 'alt-c']],
        [plugin: 'fzf-history',    options: [key: 'ctrl-r']],
        [plugin: 'fzf-completion'],
    ],
]
```

`options` is forwarded verbatim to the plugin's top-level block as
its single argument.  Omit `options:` for plugins that take no
configuration (or pass `[:]` explicitly).  Unknown top-level keys in
an entry are warned and ignored.

Plugins are loaded in list order after the ralrc evaluates. This is
the only path by which a plugin receives non-default options — a
`load-plugin` call in the ralrc body (see below) always loads with
`[:]`. For conditional loading with default options, call
`load-plugin` directly in the body before the final `return`:

```
load-plugin 'syntax-highlight'
_if !{is-executable 'fzf'} {
    load-plugin 'fzf-files'
    load-plugin 'fzf-history'
} {}

return [env: [...]]
```

**Receiving configuration in a plugin.** A configurable plugin's
top-level block takes exactly one parameter: the options map.
Fields are extracted by name, with defaults via the prelude's `get`:

```
return { |options|
    let key = get $options key 'ctrl-r'
    # ... use $key ...
    return [name: 'fzf-history', ..., keybindings: [[key: $key, handler: $_handler]]]
}
```

Plugins that need no configuration return the manifest map directly
without a wrapping block.

## 11 Examples

### 11.1 CTRL-T — insert files at cursor

Ported from fzf's `key-bindings.zsh`. Reads `$FZF_CTRL_T_COMMAND`,
`$FZF_CTRL_T_OPTS`, `$FZF_DEFAULT_OPTS`, `$FZF_DEFAULT_OPTS_FILE` (its
contents are spliced into the option stack rather than passed through
as a file), and renders in a tmux pane/popup via fzf-tmux when
`$TMUX_PANE` and one of `$FZF_TMUX` / `$FZF_TMUX_OPTS` say to.

```
return { |options|
    let key = get $options key "ctrl-t"
    let _handler = { |ctx|
        let cmd = try { return $ENV[FZF_CTRL_T_COMMAND] } { |_| return "" }
        let user = try { return $ENV[FZF_DEFAULT_OPTS] } { |_| return "" }
        let extra = try { return $ENV[FZF_CTRL_T_OPTS] } { |_| return "" }
        let fopts = try { let optsf = $ENV[FZF_DEFAULT_OPTS_FILE]; return !{from-string < $optsf} } { |_| return "" }
        let height_env = try { return $ENV[FZF_TMUX_HEIGHT] } { |_| return "" }
        let height = if !{is-empty $height_env} { return "40%" } else { return $height_env }
        let opts = "--height $height --min-height 20+ --bind=ctrl-z:ignore --reverse --walker=file,dir,follow,hidden --scheme=path\n$fopts\n$user $extra -m"
        let pane = try { return $ENV[TMUX_PANE] } { |_| return "" }
        let ftm = try { return $ENV[FZF_TMUX] } { |_| return "" }
        let ftm_opts = try { return $ENV[FZF_TMUX_OPTS] } { |_| return "" }
        let tmux_args = if $[not !{is-empty $pane} && ((not !{is-empty $ftm} && not !{equal $ftm "0"}) || not !{is-empty $ftm_opts})] {
            if !{is-empty $ftm_opts} {
                return ["-d$height"]
            } else {
                try { return !{shell-split $ftm_opts} } { |_| return ["-d$height"] }
            }
        } else { return [] }
        let r = _ed-tui {
            within [env: [
                FZF_DEFAULT_COMMAND: $cmd,
                FZF_DEFAULT_OPTS_FILE: "",
                FZF_DEFAULT_OPTS: $opts,
            ]] {
                if $[not !{is-empty $tmux_args}] {
                    fzf-tmux ...$tmux_args --
                } else {
                    fzf
                }
            }
        }
        if $[$r[status] == 0 && not !{is-empty $r[output]}] {
            let quoted = intercalate " " !{map $shell-quote !{re-split "\n" $r[output]}}
            _ed-insert "$quoted "
        } elsif $[$r[status] != 0 && $r[status] != 1 && $r[status] != 130] {
            fail [status: $r[status], message: "fzf: $r[output]"]
        }
        return unit
    }

    return [
        name: "fzf-files",
        keybindings: [[key: $key, handler: $_handler]],
    ]
}
```

### 11.2 ALT-C — cd to selected directory

Ported the same way as 11.1, walking directories instead of files and
selecting a single pick (`+m`). Uses `_ed-push` + `_ed-accept` to
mirror zsh's `push-line` + `accept-line`: the buffer is saved and
restored on the next prompt while the `cd` runs immediately. The
target path is resolved with the `absolute-path` builtin (lexical,
zsh `:a`-style — no filesystem access, unlike `resolve-path`).

```
return { |options|
    let key = get $options key "alt-c"
    let _handler = { |ctx|
        let cmd = try { return $ENV[FZF_ALT_C_COMMAND] } { |_| return "" }
        let user = try { return $ENV[FZF_DEFAULT_OPTS] } { |_| return "" }
        let extra = try { return $ENV[FZF_ALT_C_OPTS] } { |_| return "" }
        let fopts = try { let optsf = $ENV[FZF_DEFAULT_OPTS_FILE]; return !{from-string < $optsf} } { |_| return "" }
        let height_env = try { return $ENV[FZF_TMUX_HEIGHT] } { |_| return "" }
        let height = if !{is-empty $height_env} { return "40%" } else { return $height_env }
        let opts = "--height $height --min-height 20+ --bind=ctrl-z:ignore --reverse --walker=dir,follow,hidden --scheme=path\n$fopts\n$user $extra +m"
        let pane = try { return $ENV[TMUX_PANE] } { |_| return "" }
        let ftm = try { return $ENV[FZF_TMUX] } { |_| return "" }
        let ftm_opts = try { return $ENV[FZF_TMUX_OPTS] } { |_| return "" }
        let tmux_args = if $[not !{is-empty $pane} && ((not !{is-empty $ftm} && not !{equal $ftm "0"}) || not !{is-empty $ftm_opts})] {
            if !{is-empty $ftm_opts} {
                return ["-d$height"]
            } else {
                try { return !{shell-split $ftm_opts} } { |_| return ["-d$height"] }
            }
        } else { return [] }
        let r = _ed-tui {
            within [env: [
                FZF_DEFAULT_COMMAND: $cmd,
                FZF_DEFAULT_OPTS_FILE: "",
                FZF_DEFAULT_OPTS: $opts,
            ]] {
                if $[not !{is-empty $tmux_args}] {
                    fzf-tmux ...$tmux_args --
                } else {
                    fzf
                }
            }
        }
        if $[$r[status] == 0 && not !{is-empty $r[output]}] {
            let resolved = absolute-path $r[output]
            _ed-push
            _ed-set [text: "cd !{shell-quote $resolved}", cursor: 0]
            _ed-accept
        } elsif $[$r[status] != 0 && $r[status] != 1 && $r[status] != 130] {
            fail [status: $r[status], message: "fzf: $r[output]"]
        }
        return unit
    }

    return [
        name: "fzf-cd",
        keybindings: [[key: $key, handler: $_handler]],
    ]
}
```

### 11.3 CTRL-R — history search

Ported from fzf's `key-bindings.zsh`. History entries flow
NUL-separated (`--read0`/`--print0`) so multi-line commands survive
intact; `--multi` allows picking more than one entry, and multiple
picks join with newlines into the buffer (upstream's ctrl-r is
single-pick — this port isn't). `alt-r:toggle-raw` and `--wrap-sign`
are additional fzf features not in the original zsh integration.
Upstream's `-n2..,..` is omitted: there is no event-number column
here.

```
return { |options|
    let key = get $options key "ctrl-r"
    let _handler = { |ctx|
        let query = _ed-lbuffer
        let entries = _ed-history "" 0
        let user = try { return $ENV[FZF_DEFAULT_OPTS] } { |_| return "" }
        let extra = try { return $ENV[FZF_CTRL_R_OPTS] } { |_| return "" }
        let fopts = try { let optsf = $ENV[FZF_DEFAULT_OPTS_FILE]; return !{from-string < $optsf} } { |_| return "" }
        let height_env = try { return $ENV[FZF_TMUX_HEIGHT] } { |_| return "" }
        let height = if !{is-empty $height_env} { return "40%" } else { return $height_env }
        let opts = "--height $height --min-height 20+ --bind=ctrl-z:ignore\n$fopts\n$user --scheme=history --bind=ctrl-r:toggle-sort,alt-r:toggle-raw --wrap-sign '\\t↳ ' --highlight-line --multi $extra"
        let pane = try { return $ENV[TMUX_PANE] } { |_| return "" }
        let ftm = try { return $ENV[FZF_TMUX] } { |_| return "" }
        let ftm_opts = try { return $ENV[FZF_TMUX_OPTS] } { |_| return "" }
        let tmux_args = if $[not !{is-empty $pane} && ((not !{is-empty $ftm} && not !{equal $ftm "0"}) || not !{is-empty $ftm_opts})] {
            if !{is-empty $ftm_opts} {
                return ["-d$height"]
            } else {
                try { return !{shell-split $ftm_opts} } { |_| return ["-d$height"] }
            }
        } else { return [] }
        let hist_nul = intercalate "\0" $entries
        let r = _ed-tui {
            within [env: [
                FZF_DEFAULT_OPTS_FILE: "",
                FZF_DEFAULT_OPTS: $opts,
            ]] {
                if $[not !{is-empty $tmux_args}] {
                    to-string $hist_nul | fzf-tmux ...$tmux_args -- "--query" $query --read0 --print0
                } else {
                    to-string $hist_nul | fzf "--query" $query --read0 --print0
                }
            }
        }
        if $[$r[status] == 0 && not !{is-empty $r[output]}] {
            let picks = filter { |p| return $[not !{is-empty $p}] } !{re-split "\0" $r[output]}
            let cleaned = map { |p| return !{re-replace "\n*\$" "" $p} } $picks
            let joined = intercalate "\n" $cleaned
            _ed-set [text: $joined, cursor: !{length $joined}]
        } elsif $[$r[status] != 0 && $r[status] != 1 && $r[status] != 130] {
            fail [status: $r[status], message: "fzf: $r[output]"]
        }
        return unit
    }

    return [
        name: "fzf-history",
        keybindings: [[key: $key, handler: $_handler]],
    ]
}
```

### 11.4 TAB — `**`-trigger completion

Ported from fzf's `completion.zsh`. Binds `tab` with a `guard` regex
so that plain tab still falls through to ral's built-in completer;
only a word ending in the trigger (`**` by default, `$FZF_COMPLETION_TRIGGER`
at load time) claims the key. Dispatches on the command word to the
left of the trigger: path/dir completion via `fzf --walker` for most
commands (`cd`/`rmdir` get dir-only completion via
`$FZF_COMPLETION_DIR_COMMANDS`), host completion for `ssh`/`telnet`
(parsed from `~/.ssh/config`, `~/.ssh/known_hosts`, `/etc/hosts`,
`/etc/ssh/ssh_config` — no `awk`), and PID completion for `kill`. Not
ported: `export`/`unset`/`unalias` completers, `~user` expansion,
quoted multi-word prefixes, and the `_fzf_compgen_*` /
`_fzf_comprun` override hooks.

The manifest, the guard construction, and the dispatch handler:

```
return { |options|
    let env_trigger = try { return $ENV[FZF_COMPLETION_TRIGGER] } { |_| return '**' }
    let trigger = get $options trigger $env_trigger

    let _envor = { |name default|
        try { return $ENV[$name] } { |_| return $default }
    }

    # Guard: a word, a gap, then the current word ending in the trigger —
    # upstream's "tokens > 1 and LBUFFER ends with trigger" check.
    let _guard-for = { |t|
        if !{is-empty $t} { return '\S\s+\S*$' } else {
            let esc = re-replace-all '([.^$*+?()\[\]{}|\\])' '\${1}' $t
            return !{intercalate '' ['\S\s+\S*', $esc, '$']}
        }
    }
    let tab_guard = _guard-for $trigger
```

```
    let _handler = { |ctx|
        let text = _ed-text
        let cur = _ed-cursor
        let lb = _ed-lbuffer
        let p = _ed-parse
        let cw = _cur-word $p $text $cur $lb
        let word = $cw[word]
        let wn = length $word
        let tn = length $trigger
        if $[$wn < $tn] { return unit }
        if $[$tn > 0 && not !{equal !{slice $word $[$wn - $tn] $tn} $trigger}] { return unit }
        let prefix = slice $word 0 $[$wn - $tn]
        let lbuf = slice $text 0 $cw[off]
        let seg = last !{re-split '[|;&\n]' $lbuf}
        let segw = words $seg
        let cmd = _first-or $segw ''
        let prev = _last-or $segw ''
        let d_cmds = words !{_envor 'FZF_COMPLETION_DIR_COMMANDS' 'cd rmdir'}
        if !{elem $cmd $d_cmds} {
            _path-complete $prefix $lbuf 'dir'
        } elsif $[!{equal $cmd 'ssh'} && !{elem $prev ['-i', '-F', '-E']}] {
            _path-complete $prefix $lbuf 'path'
        } elsif !{equal $cmd 'ssh'} {
            _list-complete !{_with-user $prefix !{_hosts}} $prefix $lbuf '+m'
        } elsif !{equal $cmd 'telnet'} {
            _list-complete !{_hosts} $prefix $lbuf '+m'
        } elsif !{equal $cmd 'kill'} {
            _kill-complete $prefix $lbuf
        } else {
            _path-complete $prefix $lbuf 'path'
        }
        return unit
    }

    return [
        name: 'fzf-completion',
        keybindings: [[key: 'tab', handler: $_handler, guard: $tab_guard]],
    ]
}
```

The elided body defines `_path-complete` (walks up from the prefix's
nearest existing directory ancestor, then runs `fzf --walker` rooted
there), `_list-complete` and `_kill-complete` (feed a candidate list
or `ps` output to plain `fzf`), and the host-list readers
`_cfg-hosts` / `_known-hosts` / `_etc-hosts` / `_hosts`.

### 11.5 Syntax highlight (sketch)

```
let _handler = { |ev|
    let new = $ev[line]
    _if !{is-empty $new} { _ed-highlight []; return unit } {}
    let toks  = split '[ \t]+' $new
    let head  = $toks[0]
    let style = try { which $head; return 'command' } { |_| return 'error' }
    _ed-highlight [[start: 0, end: !{length $head}, style: $style]]
}

return [
    name: 'syntax-highlight',
    hooks: [buffer-change: $_handler],
]
```

## 12 Future extensions

The following appear in earlier design notes but are not yet
implemented. They are collected here as candidates for future
releases.

- **Multi-key bindings.** A key notation `'escape escape'` and a
  configurable timeout (`key_timeout` in ralrc, e.g. 500ms) to
  support chords.

- **`buffer-change` deadline.** A soft deadline (e.g. 16ms,
  configurable as `editor_hook_deadline_us`) after which remaining
  buffer-change handlers are deferred to the next idle. Today
  handlers run unconditionally; a slow handler slows every
  keystroke.

- **Left/right prompt hooks.** A `prompt` signature taking the side
  (`"left"`/`"right"`) and returning that side's segment,
  concatenated by the shell. Today `prompt` is a transformer on the
  full prompt string, which is adequate for left-prompt decoration
  but leaves no clean place to contribute to a right-prompt.

- **Full parser in `_ed-parse`.** Replace the whitespace tokeniser
  with the real ral lexer/parser so that `_ed-parse` returns exactly
  the same tokens the shell would execute.

- **Highlight-style overrides.** A `highlight_styles` key in ralrc
  that remaps each named style to terminal attributes.

- **Async prompt hooks.** A `prompt` handler that runs slow work
  (`git status`, VCS queries) without blocking the prompt render;
  current workaround is `spawn` with a cached result.
