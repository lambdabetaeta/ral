# ral plugins

A plugin is a ral module (SPEC §15.5) whose return value is either a
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
| `keybindings` | `[[key: Str, handler: {Returns Unit}, guard?: Str]]` | `[]` |
| `aliases` | `[Str: {[Str] → Returns Any}]` | `[:]` |

Every unmodified key except `f1`–`f12` requires a `guard`; an
unguarded binding on such a key is a load-time error (§6).

The `name` must be unique across loaded plugins.  The top-level
block, if present, takes exactly one argument: the options map.
Plugins extract fields by name (`get $options key <default>` or
`$options[key]`) and decide what is required vs. optional.

## 2 Authority

Plugins run with host authority. A hook, keybinding handler, or
plugin-registered alias executes under whatever capabilities the
caller's grant stack (SPEC §12) already holds — the manifest has no
field for declaring or narrowing that. A `capabilities:` key in a
manifest is a load-time error, so a plugin cannot mistake a listed
capability set for enforcement. Confinement is `grant`, applied at
the call site, not a manifest declaration.

`grant`'s `editor` field gates the `_ed-*` builtins (SPEC §12.8):

| Field | Enables |
|---|---|
| `read`  | `_ed-get`, `_ed-text`, `_ed-cursor`, `_ed-keymap`, `_ed-lbuffer`, `_ed-parse`, `_ed-history` |
| `write` | `_ed-set`, `_ed-set-lbuffer`, `_ed-insert`, `_ed-push`, `_ed-accept`, `_ed-ghost`, `_ed-highlight`, `_ed-state` |
| `tui`   | `_ed-tui` |

`grant`'s `shell` field gates shell builtins that modify persistent
process state (SPEC §12.8):

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
is SPEC §15.5; the summary below is oriented around what plugin code
needs to know.

| Builtin | Shape | Purpose |
|---|---|---|
| `_ed-get` | `Returns [text: Str, cursor: Int, keymap: Str]` | read state |
| `_ed-text` | `Returns Str` | current buffer text |
| `_ed-cursor` | `F Int` | current cursor offset |
| `_ed-keymap` | `Returns Str` | current keymap name |
| `_ed-lbuffer` | `Returns Str` | text left of cursor |
| `_ed-set` | `[text: `keep\|`set Str, cursor: `keep\|`set Int] → Returns Unit` | partial buffer update |
| `_ed-set-lbuffer` | `Str → Returns Unit` | replace left-of-cursor, preserve right |
| `_ed-insert` | `Str → Returns Unit` | insert at cursor, advance |
| `_ed-push` | `Returns Unit` | save buffer, clear |
| `_ed-accept` | `Returns Unit` | run buffer on return |
| `_ed-tui` | `∀ν α. {ν α} → Returns [output: Str, status: Int]` | suspend editor, run body, capture stdout |
| `_ed-history` | `Str → Int → Returns [Str]` | prefix search (limit 0 = all) |
| `_ed-parse` | `Returns [words: [Str], current: Int, offset: Int]` | the simple command at the cursor |
| `_ed-ghost` | `Str → Returns Unit` | set suggestion after cursor |
| `_ed-highlight` | `[[start: Int, end: Int, style: Str]] → Returns Unit` | set spans |
| `_ed-state` | `α:data → {α → Returns α} → Returns α` | per-plugin persistent cell |

Indices are character indices, consistent with `length` and `slice`.

**`_ed-push` + `_ed-accept`.** `_ed-push` saves the current buffer on
the shell's buffer stack and clears the editor; the next prompt
restores it. `_ed-accept` marks the buffer for immediate execution as
if the user had pressed Enter. The pair implements zsh-style
`push-line; accept-line`.

**`_ed-tui`.** The body runs with the line editor suspended and
stdout captured. On success the return record's `status` is 0 and
`output` is the captured stdout, decoded lossily, one trailing newline
stripped.  The body is a command, its value is its output, and the call
displays a terminal selection rather than computing with it. When the body fails, `status` carries the exit code and
`output` carries the error message; the call never raises, so plugins
can discriminate cancellation (fzf 1 = no match, 130 = Esc) from real
errors without wrapping in `try`. Nested `_ed-tui` is reported as
status 1 rather than raised.

**`_ed-history`.** Entries are returned most recent first,
deduplicated. `_ed-history '' 0` returns the full history.

**`_ed-parse`.** Returns the words of the simple command around the
cursor: ral's own tokens, split at `|`, `?`, `;`, newlines and braces,
with any `{` or `[` still open read as closed, since a buffer is
routinely mid-block. `words[current]` is the word the cursor touches
and `offset` its character index; a cursor in whitespace stands on a
fresh `''` spliced in at its own offset, as bash's `COMP_CWORD` does.
`words` is empty exactly when the buffer does not lex, as inside an
open quote.

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

The cell is a boundary: a value in it was written by an earlier run, so its
shape is not one this handler decided. The call admits the stored value against
the type the handler uses it at, before the updater runs, and refuses one that
does not fit (a stale cell from an older version of the plugin), naming the
field.

## 4 `load-plugin` / `unload-plugin`

`load-plugin` and `unload-plugin` are host builtins the ral REPL
installs into its own shell's manifest — fixed arity, so they seed
its base scope as natives — not core builtins and not prelude
wrappers around anything else.

| Builtin | Shape |
|---|---|
| `load-plugin` | `Str → Returns Unit` |
| `unload-plugin` | `Str → Returns Unit` |

`load-plugin` resolves its argument in order:

1. `XDG_CONFIG_HOME/ral/plugins/$name.ral` (falls back to `~/.config/…`).
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
loaded through `~/.ralrc`'s `plugins:` map (§9), whose value for that
plugin is forwarded to the plugin's top-level block.

## 5 Hooks

Declared as `hooks: [event: $handler, …]` in the manifest. Plugins
cannot register hooks at runtime.

Every hook handler takes exactly one argument. Event hooks receive
an event record; arity is checked at registration, so a handler of
any other shape is a load error.

| Event | Handler | Fires |
|---|---|---|
| `buffer-change` | `{Map → Returns Unit}` | after buffer or cursor changes |
| `pre-exec` | `{Map → Returns Unit}` | after Enter, before execution |
| `post-exec` | `{Map → Returns Unit}` | after execution completes |
| `chpwd` | `{Map → Returns Unit}` | after a line that moved the session's directory |
| `prompt` | `{Str → Returns Str}` | before each prompt render |

All handlers for an event run in plugin load order regardless of
individual failures. A failing handler's error is logged as
`plugin 'name': hook 'event' failed: <message>`.

**`buffer-change`** receives
`[old_buf: Str, line: Str, pos: Int, history: [Str], keymap: Str,
state]`. Typical uses are highlighting and autosuggestion.

**`pre-exec`** receives `[src: Str]`, the full command line as
typed; **`post-exec`** receives `[src: Str, status: Int]`, adding
the exit status. **`chpwd`** receives `[old: Str, new: Str]`, the
session's working directory before and after the line. It compares
the two ends, not the `cd`s between them: `cd a; cd b` on one line
fires once, while `cd .`, or a `cd` inside `within [dir: …]`, which
restores the directory on exit, fires nothing.

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

A handler is `{|ctx| → Returns Unit}`; its return value carries nothing.
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

## 8 Prelude helpers

The `_ed-*` family is direct builtins (see §3); plugins call them by
name with no prelude indirection. `load-plugin` and `unload-plugin`
are likewise direct host builtins (§4), not prelude wrappers. Plugin
code does reach for one genuine prelude helper:

```
elem   x items   -- membership test
```

## 9 `~/.ralrc`

The config map (SPEC §15.3) accepts an optional `plugins` map, from
plugin name (or path) to that plugin's options map:

```
return [
    env: [EDITOR: 'nvim'],
    plugins: [
        syntax-highlight: [:],
        fzf-files:        [key: 'ctrl-t'],
        fzf-cd:           [key: 'alt-c'],
        fzf-history:      [key: 'ctrl-r'],
        fzf-completion:   [:],
    ],
]
```

A bare hyphenated plugin name needs no quoting; a path does
(`'/path/to/p.ral': [:]`). Each value is forwarded verbatim to the
plugin's top-level block as its single argument; `[:]` is the empty
map, for a plugin taking no options. A value that is not a map is
reported by name and skipped; the other plugins still load.

Plugins load in alphabetical order of plugin name after the ralrc
evaluates — ral maps are sorted by key, so the written order does not
matter. This is the only path by which a plugin receives non-default
options — a `load-plugin` call in the ralrc body (see below) always
loads with `[:]`. For conditional loading with default options, call
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

## 10 Examples

The fzf plugins port fzf's own `key-bindings.zsh` and `completion.zsh`.
What those scripts share — the option stack, the fzf-tmux switch, the
exit statuses — is one module, `plugins/lib/fzf.ral`, which each plugin
binds with `use` at its top level, where the path resolves beside the
plugin file.

### 10.1 The shared module

```
# What fzf's shell integrations share (__fzf_defaults, __fzfcmd and
# __fzf_comprun in key-bindings.zsh and completion.zsh), for the fzf-* plugins.

## $name, or `default` when it is unset or empty: sh's ${name:-default}.
let var = { |name default| let v = get !{env} $name ''; if !{is-empty $v} { $default } else { $v } }

## Run fzf as fzf's own widgets do — `ours` before the user's options file and
## FZF_DEFAULT_OPTS, `theirs` after, in a tmux popup when $FZF_TMUX asks — over
## `walk $command (fzf's walker when '') or `items $list, and give `k` the picks.
let pick = { |ours theirs src args k|
    let h = var FZF_TMUX_HEIGHT 40%
    let [output: out, status: s] = _ed-tui {
        within [
            env: [
                FZF_DEFAULT_OPTS: "--height $h --min-height 20+ --bind=ctrl-z:ignore $ours\n!{from-string < !{var FZF_DEFAULT_OPTS_FILE ''} ? ''}\n!{var FZF_DEFAULT_OPTS ''} $theirs",
                FZF_DEFAULT_OPTS_FILE: '',
            ],
            handlers: [fzf: { |a|
                if $[not !{is-empty !{var TMUX_PANE ''}} && (!{var FZF_TMUX '0'} != '0' || not !{is-empty !{var FZF_TMUX_OPTS ''}})] {
                    fzf-tmux ...!{posix-split !{var FZF_TMUX_OPTS "-d$h"}} -- ...$a
                } else { fzf ...$a }
            }],
        ] {
            case $src [
                `walk:  { |c| within [env: [FZF_DEFAULT_COMMAND: $c]] { fzf --print0 ...$args } },
                `items: { |xs| to-string !{intercalate "\0" $xs} | fzf --read0 --print0 ...$args },
            ]
        }
    }
    if $[$s == 0] { k !{re-find-matches '[^\x00]+' $out} } elsif $[$s != 1 && $s != 130] { fail [status: $s, message: "fzf: $out"] }
}
```

`pick` stacks fzf's options as upstream's `__fzf_defaults` does: `ours`,
then the contents of `$FZF_DEFAULT_OPTS_FILE`, then `$FZF_DEFAULT_OPTS`,
then `theirs`. Upstream's `__fzfcmd` becomes a handler on `fzf`, which
sends every call to `fzf-tmux` when `$TMUX_PANE` and `$FZF_TMUX` or
`$FZF_TMUX_OPTS` say so. `` `walk $command `` runs fzf over that command,
or over fzf's own walker when it is empty; `` `items $list `` feeds the
list. Items and picks travel NUL-separated, so neither a file name nor
a multi-line history entry is ever split. `k` runs only on a selection:
fzf's 1 (no match) and 130 (Esc) leave the line alone, and any other
status fails.

### 10.2 CTRL-T — insert files at cursor

```
# fzf-files — CTRL-T puts the picked paths at the cursor, as fzf's
# key-bindings.zsh does.  Reads $FZF_CTRL_T_COMMAND and $FZF_CTRL_T_OPTS.
# Options: key (default ctrl-t).

let [pick: pick, var: var] = use 'lib/fzf.ral'

{ |options| [
    name: fzf-files,
    keybindings: [[key: !{get $options key ctrl-t}, handler: { |_|
        pick '--reverse --walker=file,dir,follow,hidden --scheme=path' "!{var FZF_CTRL_T_OPTS ''} -m" `walk !{var FZF_CTRL_T_COMMAND ''} [] { |ps|
            _ed-insert "!{intercalate ' ' !{map $ral-quote $ps}} "
        }
    }]],
] }
```

`ral-quote` renders each path as ral source; `posix-quote` would not
do, since a bare `007` reads back as the number 7.

### 10.3 ALT-C — cd to selected directory

```
# fzf-cd — ALT-C cds into the picked directory, as fzf's key-bindings.zsh
# does: the line being typed is pushed, and comes back at the next prompt.
# Reads $FZF_ALT_C_COMMAND and $FZF_ALT_C_OPTS.  Options: key (default alt-c).

let [pick: pick, var: var] = use 'lib/fzf.ral'

{ |options| [
    name: fzf-cd,
    keybindings: [[key: !{get $options key alt-c}, handler: { |_|
        pick '--reverse --walker=dir,follow,hidden --scheme=path' "!{var FZF_ALT_C_OPTS ''} +m" `walk !{var FZF_ALT_C_COMMAND ''} [] { |[d]|
            _ed-push
            _ed-set [text: `set "cd !{ral-quote !{absolute-path $d}}", cursor: `keep]
            _ed-accept
        }
    }]],
] }
```

`+m` makes the pick single, and `{ |[d]| … }` binds it. `_ed-push` and
`_ed-accept` are zsh's `push-line` and `accept-line`: the `cd` runs at
once and the line being typed returns at the next prompt.
`absolute-path` resolves lexically, as zsh's `cd` then `$PWD` does,
leaving symlinks as written.

### 10.4 CTRL-R — history search

```
# fzf-history — CTRL-R replaces the line with the picked history entries,
# newline-joined, as fzf's key-bindings.zsh does; the line so far is the
# query.  Reads $FZF_CTRL_R_OPTS.  Options: key (default ctrl-r).

let [pick: pick, var: var] = use 'lib/fzf.ral'

{ |options| [
    name: fzf-history,
    keybindings: [[key: !{get $options key ctrl-r}, handler: { |_|
        pick '' "--scheme=history --bind=ctrl-r:toggle-sort,alt-r:toggle-raw --wrap-sign '\\t↳ ' --highlight-line --multi !{var FZF_CTRL_R_OPTS ''}" `items !{_ed-history '' 0} [--query, !{_ed-lbuffer}] { |ps|
            let t = intercalate "\n" !{map { |p| re-replace-all '\n+$' '' $p } $ps}
            _ed-set [text: `set $t, cursor: `set !{length $t}]
        }
    }]],
] }
```

Upstream's `-n2..,..` skips an event-number column, and
`_ed-history` has none to skip.

### 10.5 TAB — `**`-trigger completion

`plugins/fzf-completion.ral` binds `tab` with a guard, so a plain tab
still reaches ral's own completer and only a word ending in the trigger
(`**`, or `$FZF_COMPLETION_TRIGGER` at load) claims the key. `_ed-parse`
supplies the command word, the word before the cursor's, and the word
itself; a map from command word to completer stands in for upstream's
`_fzf_complete_<command>` functions, with each of
`$FZF_COMPLETION_DIR_COMMANDS` mapped to the directory completer and
path completion as the default:

```
{ |options|
    let trigger = get $options trigger !{get !{env} FZF_COMPLETION_TRIGGER '**'}
    [
        name: fzf-completion,
        keybindings: [[
            key: tab,
            guard: !{intercalate '' ['\S\s+\S*', !{re-replace-all '[.^$*+?()\[\]{}|\\]' '\$0' $trigger}, '$']},
            handler: { |_|
                let [words: ws, current: i, offset: o] = _ed-parse
                let lb = _ed-lbuffer
                let n = $[!{length $lb} - $o - !{length $trigger}]
                if $[not !{is-empty $ws} && $n >= 0] {
                    let [cmd, ..._] = $ws
                    let t = union !{fold { |m d| [...$m, $d: $_dirs] } [:] !{words !{get !{env} FZF_COMPLETION_DIR_COMMANDS 'cd rmdir'}}} $_by_cmd
                    let complete = if !{has $t $cmd} { $t[$cmd] } else { $_files }
                    complete [prefix: !{slice $lb $o $n}, prev: !{last ['', ...!{take $i $ws}]}, lbuf: !{slice $lb 0 $o}]
                }
            },
        ]],
    ]
}
```

Path and directory completion walk below the word's longest existing
directory, the rest of the word being the query, as upstream's
`__fzf_generic_path_completion` does. ssh and telnet complete hosts from
`~/.ssh/config`, `~/.ssh/config.d/*`, `/etc/ssh/ssh_config`,
`~/.ssh/known_hosts` and `/etc/hosts`, parsed in ral rather than awk;
kill completes PIDs from `ps`. ral has no `export`, `unset` or
`unalias` to complete, and plugin options cannot carry blocks, so
upstream's `_fzf_compgen_*` and `_fzf_comprun` overrides have no
counterpart.

### 10.6 Syntax highlight (sketch)

```
let _handler = { |ev|
    let new = $ev[line]
    _if !{is-empty $new} { _ed-highlight []; return () } {}
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

## 11 Future extensions

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

- **The parser in `_ed-parse`.** `_ed-parse` splits the lexer's
  tokens at separators; reading the parser's simple command instead
  would also see a cursor inside `$[…]`, a list literal, or a redirect
  target for what it is.

- **Highlight-style overrides.** A `highlight_styles` key in ralrc
  that remaps each named style to terminal attributes.

- **Async prompt hooks.** A `prompt` handler that runs slow work
  (`git status`, VCS queries) without blocking the prompt render;
  current workaround is `spawn` with a cached result.
