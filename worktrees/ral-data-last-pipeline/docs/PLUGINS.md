# ral plugins

A plugin is a ral module (SPEC §8) whose return value is either a
manifest map, or a block that takes an options map and returns a
manifest map. `_plugin 'load' $name $options` reads the manifest,
builds a capabilities layer from its declared capabilities, and registers the
plugin's hooks and keybindings. There is no plugin DSL and no magic
config binding; a plugin's knobs are fields on its options map and
it extracts them by name.

## 1 Manifest

```
return { |options|
    let key = get $options key 'ctrl-t'
    return [
        name: 'fzf-files',
        capabilities: [
            exec: [fzf: []],
            fs: [read: ['.']],
            editor: [read: true, write: true, tui: true],
        ],
        keybindings: [[key: $key, handler: $_handler]],
    ]
}
```

A plugin that needs no configuration may return the manifest map
directly:

```
return [
    name: 'syntax-highlight',
    capabilities: [editor: [read: true, write: true]],
    hooks: [buffer-change: $_handler],
]
```

| Field | Type | Default |
|---|---|---|
| `name` | `Str` | required |
| `capabilities` | record | `[:]` |
| `hooks` | `[Str: {B}]` | `[:]` |
| `keybindings` | `[[key: Str, handler: {F Bool}]]` | `[]` |
| `aliases` | `[Str: {[Str] → F Any}]` | `[:]` |

The `name` must be unique across loaded plugins.  The top-level
block, if present, takes exactly one argument: the options map.
Plugins extract fields by name (`get $options key <default>` or
`$options[key]`) and decide what is required vs. optional.

## 2 Capabilities

`capabilities` is a record whose fields mirror `grant` (SPEC §11):
`exec`, `fs`, `net`, `editor`, `shell`. Omitted fields default to empty —
the plugin receives no ambient authority for them.
`net` is a boolean: `true` allows network access, `false` denies it.

```
capabilities: [
    exec: [fzf: [], git: ['status', 'diff']],
    fs:   [read: ['~/.cache']],
    net:  true,
    editor: [read: true, write: true, tui: true],
    shell:  [chdir: true],
]
```

`editor` gates the `_editor` sub-commands:

| Field | Enables |
|---|---|
| `read`  | `get`, `parse`, `history` |
| `write` | `set`, `push`, `accept`, `ghost`, `highlight`, `state` |
| `tui`   | `tui` |

`shell` gates shell builtins that modify persistent process state:

| Field | Enables |
|---|---|
| `chdir` | `cd` |

The shell wraps every hook, keybinding handler, and plugin-registered alias
in `grant $capabilities { … }` before invoking it. That grant is pushed on
top of the caller's current capabilities stack, so the effective authority is the
intersection of the caller's current authority and the plugin manifest. A
plugin can never exceed what its manifest declared, and it also cannot
escape an enclosing user grant. The `shell.chdir` capability is required
for hooks, keybindings, and plugin aliases that call `cd`.

## 3 `_editor`

Interactive only. Outside an interactive session every sub-command
raises `_editor: not available outside interactive mode`. The full
sub-command reference is SPEC §18.1; the summary below is oriented
around what plugin code needs to know.

| Sub-command | Shape | Purpose |
|---|---|---|
| `'get'` | `F [text: Str, cursor: Int, keymap: Str]` | read buffer |
| `'set'` | `[text: Str, cursor: Int] → F Unit` | replace buffer |
| `'push'` | `F Unit` | save buffer, clear |
| `'accept'` | `F Unit` | run buffer on return |
| `'tui'` | `{F α} → F α` | suspend editor, run body, capture stdout |
| `'history'` | `Str → Int → F [Str]` | prefix search (limit 0 = all) |
| `'parse'` | `F [words: [Str], current: Int, offset: Int]` | tokenise buffer |
| `'ghost'` | `Str → F Unit` | set suggestion after cursor |
| `'highlight'` | `[[start: Int, end: Int, style: Str]] → F Unit` | set spans |
| `'state'` | `α → {α → F α} → F α` | per-plugin persistent cell |

Indices are character indices, consistent with `length` and `slice`.

**`push` + `accept`.** `push` saves the current buffer on the shell's
buffer stack and clears the editor; the next prompt restores it.
`accept` marks the buffer for immediate execution as if the user had
pressed Enter. The pair implements zsh-style `push-line;
accept-line`.

**`tui`.** The body runs with the line editor suspended and stdout
captured. If the body returns `Unit`, the captured bytes (with one
trailing newline stripped) become the return value; otherwise the
body's return value wins. Nested `tui` is an error. Use `_ed-tui`
(§9) to catch the usual non-zero exits of cancelled TUI tools.

**`history`.** Entries are returned most recent first, deduplicated.
`_editor 'history' '' 0` returns the full history.

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

**`state`.** The first call runs the updater with the `default`; each
subsequent call runs it with the previously stored value. To read
without changing:

```
_editor 'state' $default { |s| return $s }
```

State is per-plugin and is cleared on unload.

## 4 `_plugin`

| Sub-command | Shape |
|---|---|
| `'load'` | `Str → [String:A]? → F Unit` |
| `'unload'` | `Str → F Unit` |

`load` resolves the argument in order:

1. `$XDG_CONFIG_HOME/ral/plugins/$name.ral` (falls back to `$HOME/.config/…`).
2. Each `$dir/$name.ral` for `$dir` in `RAL_PATH` (colon-separated).
3. As a literal path, with `.ral` appended if needed.

The module is evaluated. If its return value is a block, the options
map is applied as its single argument (defaulting to `[:]` when
absent); the result must then be a manifest map. If the module's
return value is already a map, a non-empty options map is a load-time
error.  Unknown hook names and malformed keybindings are warned on
stderr and skipped; they do not fail the load. Loading a plugin
whose name is already registered is an error; `unload` of an unknown
plugin is also an error.

The prelude provides `load-plugin` and `unload-plugin` as thin
wrappers. `load-plugin` takes a name and an options map:

```
load-plugin 'syntax-highlight' [:]
load-plugin 'fzf-files'        [key: 'ctrl-t']
```

## 5 Hooks

Declared as `hooks: [event: $handler, …]` in the manifest. Plugins
cannot register hooks at runtime.

| Event | Handler | Fires |
|---|---|---|
| `buffer-change` | `{Str → Str → Int → F Unit}` | after buffer or cursor changes |
| `pre-exec` | `{Str → F Unit}` | after Enter, before execution |
| `post-exec` | `{Str → Int → F Unit}` | after execution completes |
| `chpwd` | `{Str → Str → F Unit}` | after `cd` or a builtin `chdir(2)` |
| `prompt` | `{Str → F Str}` | before each prompt render |

All handlers for an event run in plugin load order regardless of
individual failures. A failing handler's error is logged as
`plugin 'name': hook 'event' failed: <message>`.

**`buffer-change`** arguments are `old-text`, `new-text`,
`new-cursor`. Typical uses are highlighting and autosuggestion.

**`pre-exec` / `post-exec`** receive the full command line as typed;
`post-exec` also receives the exit status. `chpwd` receives the old
and new working directories.

**`prompt`** is a transformer. Each handler receives the current
prompt string (starting from the shell's base) and returns a new
one. Handlers compose: the output of handler `n` is the input to
handler `n+1`.

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

Declared as `keybindings: [[key: $str, handler: $thunk], …]`.

**Key notation:**

| Notation | Meaning |
|---|---|
| `'a' … 'z'`, `'0' … '9'` | literal |
| `'ctrl-<c>'`, `'alt-<c>'` | modified letter |
| `'tab'`, `'enter'`, `'escape'`, `'backspace'`, `'delete'` | named keys |
| `'up'`, `'down'`, `'left'`, `'right'`, `'home'`, `'end'` | navigation |
| `'f1' … 'f12'` | function keys |

Invalid notation is warned and skipped.

**Dispatch.** When a bound key fires, the shell walks the handlers
for that key in **reverse load order**:

- Return `true` → the keypress is consumed; stop.
- Return `false` → fall through to the next handler.
- Raise an error → log, treat as consumed, stop.
- All handlers return `false` → run the shell's built-in binding.

This is the `?` fallback pattern applied to keypress dispatch. A
plugin can decline a key it does not want to handle.

**What a handler may do.** Use `_editor 'set' / 'push' / 'accept' /
'ghost' / 'highlight'` to mutate editor state. Use `_editor 'tui'`
(with `editor.tui` in capabilities) to run a fuzzy finder or other
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

**Unload.** `_plugin 'unload' $name` removes exactly the aliases that
plugin installed. Other aliases are untouched.

**Authority.** Plugin aliases run with ambient authority — no grant is
pushed around the call. This matches the behaviour of `rc` aliases.
A plugin alias that calls `cd` therefore always succeeds, without
requiring `shell: [chdir: true]` in `capabilities`. Declare `shell:
[chdir: true]` when a *hook* or *keybinding handler* calls `cd`.

```
aliases: [
    z: { |args|
        if !{is-empty $args} {
            cd ~
        } {
            let cwd = pwd | from-string
            let result = try {
                return zoxide query --exclude $cwd -- ...$args | from-string
            } { |_| return '' }
            _if !{is-empty $result} {} { cd $result }
        }
    },
]
```

## 9 Prelude helpers

Bound in the interactive prelude (not in script mode):

```
shell-quote      s           -- _str 'shell-quote' $s
shell-split      s           -- _str 'shell-split' $s
_ed-get                      -- _editor 'get'
_ed-set          s           -- _editor 'set' $s
_ed-text                     -- text field of _ed-get
_ed-cursor                   -- cursor field of _ed-get
_ed-keymap                   -- keymap field of _ed-get
_ed-lbuffer                  -- slice _ed-text 0 _ed-cursor
_ed-set-lbuffer  l           -- replace the portion left of cursor
_ed-insert       str         -- insert str at cursor, advance
_ed-tui          body        -- try { ... } { |e| return [output, status] }
load-plugin      name opts   -- _plugin 'load' $name $opts
unload-plugin    name        -- _plugin 'unload' $name
elem             x items     -- membership test
```

`_ed-tui` runs the body and always returns `[output: Str, status: Int]`
without raising on body status.  Plugins discriminate cancellation
from other failures (e.g. fzf returns 0 on selection, 1 on no match,
130 on Esc, 2 on error) by checking `status`.  Infrastructure failures
(cannot enter TUI mode, etc.) still raise.  Raw access without the
record wrap is `_editor 'tui' $body`.

## 10 `~/.ralrc`

The config map (SPEC §9) accepts an optional `plugins` list.  Each
entry is a map `[plugin: Str, options?: Map]`:

```
return [
    env: [EDITOR: 'nvim'],
    plugins: [
        [plugin: 'syntax-highlight'],
        [plugin: 'fzf-files',   options: [key: 'ctrl-t']],
        [plugin: 'fzf-cd',      options: [key: 'alt-c']],
        [plugin: 'fzf-history', options: [key: 'ctrl-r']],
    ],
]
```

`options` is forwarded verbatim to the plugin's top-level block as
its single argument.  Omit `options:` for plugins that take no
configuration (or pass `[:]` explicitly).  Unknown top-level keys in
an entry are warned and ignored.

The `load-plugin` helper takes a name and an options map:

```
load-plugin 'syntax-highlight' [:]
load-plugin 'fzf-history'      [key: 'ctrl-r']
```

Plugins are loaded in list order after the ralrc evaluates. For
conditional loading, call `load-plugin` directly in the body before
the final `return`:

```
load-plugin 'syntax-highlight' [:]
_if !{is-executable 'fzf'} {
    load-plugin 'fzf-files'   [key: 'ctrl-t']
    load-plugin 'fzf-history' [key: 'ctrl-r']
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

```
# Options:  key   keybinding (default 'ctrl-t')
return { |options|
    let key = get $options key 'ctrl-t'
    let _handler = {
        let cmd       = try { return $env[FZF_CTRL_T_COMMAND] } { |_| return '' }
        let extra_str = try { return $env[FZF_CTRL_T_OPTS] }    { |_| return '' }
        let extra = shell-split $extra_str
        let r = _ed-tui {
            within [env: [FZF_DEFAULT_COMMAND: $cmd, FZF_DEFAULT_OPTS_FILE: '']] {
                fzf --reverse --walker 'file,dir,follow,hidden'
                    --scheme path -m ...$extra
            }
        }
        # fzf: 0=selection, 1=no-match, 130=Esc; 1 and 130 are silent.
        if $[$r[status] == 0 && not !{is-empty $r[output]}] {
            let quoted = join ' ' !{map $shell-quote !{split '\n' $r[output]}}
            _ed-insert "$quoted "
        } elsif $[$r[status] != 0 && $r[status] != 1 && $r[status] != 130] {
            fail [status: $r[status], message: "fzf: $r[output]"]
        }
        return true
    }

    return [
        name: 'fzf-files',
        capabilities: [
            exec: [fzf: []],
            editor: [read: true, write: true, tui: true],
        ],
        keybindings: [[key: $key, handler: $_handler]],
    ]
}
```

### 11.2 ALT-C — cd to selected directory

```
# Options:  key   keybinding (default 'alt-c')
return { |options|
    let key = get $options key 'alt-c'
    let _handler = {
        let cmd       = try { return $env[FZF_ALT_C_COMMAND] } { |_| return '' }
        let extra_str = try { return $env[FZF_ALT_C_OPTS] }    { |_| return '' }
        let extra = shell-split $extra_str
        let r = _ed-tui {
            within [env: [FZF_DEFAULT_COMMAND: $cmd, FZF_DEFAULT_OPTS_FILE: '']] {
                fzf --reverse --walker 'dir,follow,hidden'
                    --scheme path +m ...$extra
            }
        }
        if $[$r[status] == 0 && not !{is-empty $r[output]}] {
            let resolved = resolve-path $r[output]
            _editor 'push'
            _editor 'set' [text: "cd !{shell-quote $resolved}", cursor: 0]
            _editor 'accept'
        } elsif $[$r[status] != 0 && $r[status] != 1 && $r[status] != 130] {
            fail [status: $r[status], message: "fzf: $r[output]"]
        }
        return true
    }

    return [
        name: 'fzf-cd',
        capabilities: [
            exec: [fzf: []],
            editor: [read: true, write: true, tui: true],
        ],
        keybindings: [[key: $key, handler: $_handler]],
    ]
}
```

### 11.3 CTRL-R — history search

```
# Options:  key   keybinding (default 'ctrl-r')
return { |options|
    let key = get $options key 'ctrl-r'
    let _handler = {
        let query     = _ed-lbuffer
        let entries   = _editor 'history' '' 0
        let extra_str = try { return $env[FZF_CTRL_R_OPTS] } { |_| return '' }
        let extra = shell-split $extra_str
        let r = _ed-tui {
            to-lines $entries | fzf --scheme history
                                    '--bind' 'ctrl-r:toggle-sort'
                                    --highlight-line '--query' $query ...$extra
        }
        if $[$r[status] == 0 && not !{is-empty $r[output]}] {
            _editor 'set' [text: $r[output], cursor: !{length $r[output]}]
        } elsif $[$r[status] != 0 && $r[status] != 1 && $r[status] != 130] {
            fail [status: $r[status], message: "fzf: $r[output]"]
        }
        return true
    }

    return [
        name: 'fzf-history',
        capabilities: [
            exec: [fzf: []],
            editor: [read: true, write: true, tui: true],
        ],
        keybindings: [[key: $key, handler: $_handler]],
    ]
}
```

### 11.4 Syntax highlight (sketch)

```
let _handler = { |_old new _cursor|
    _if !{is-empty $new} { _editor 'highlight' []; return unit } {}
    let toks  = split '[ \t]+' $new
    let head  = $toks[0]
    let style = try { which $head; return 'command' } { |_| return 'error' }
    _editor 'highlight' [[start: 0, end: !{length $head}, style: $style]]
}

return [
    name: 'syntax-highlight',
    capabilities: [editor: [read: true, write: true]],
    hooks: [buffer-change: $_handler],
]
```

## 10 Future extensions

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

- **Full parser in `_editor 'parse'`.** Replace the whitespace
  tokeniser with the real ral lexer/parser so that `parse` returns
  exactly the same tokens the shell would execute.

- **Highlight-style overrides.** A `highlight_styles` key in ralrc
  that remaps each named style to terminal attributes.

- **Async prompt hooks.** A `prompt` handler that runs slow work
  (`git status`, VCS queries) without blocking the prompt render;
  current workaround is `spawn` with a cached result.

- **Scripted completion dispatch.** A per-command completion map
  (e.g. `{ cd: $dir-completer, ssh: $host-completer, … }`)
  implementable in pure ral on top of `_editor 'parse'`.
