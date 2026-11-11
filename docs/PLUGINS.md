# ral plugins

A plugin extends an interactive `ral` session. It is an ordinary ral file
whose result is a manifest: a record naming the hooks, keybindings, and
handlers it contributes. The rc's `plugins:` map loads them at startup;
they remain installed for the session. A new plugin requires a new session.

This document is the design narrative. The [SPEC](SPEC.md) is the
reference: §15.5 (manifest, loading), §15.6 (hooks, key notation,
dispatch), §15.7 (the `_ed-*` builtins), §12.8 (the `editor` and `shell`
grants). Where a rule is exact, this document cites it rather than
restating it. Section numbers below without the word SPEC are this
document's own.

## 1 What a plugin is

A plugin is data, not a program that registers things. The file computes
a manifest and returns it; the host reads the manifest and does the
registering. Four consequences follow.

- **No plugin language.** There is no `register-hook` call, no `plugin`
  form, no magic config binding. A plugin is written in ral and checked
  as ral is checked. The manifest has a declared keyset, ascribed to the
  file before it runs (SPEC §15.5, as for the rc file in §15.3), so a
  misspelled key is a compile error and the file never executes. Quoted
  constant keys are names too: `["name": 'example']` is a record checked
  against the manifest table, not a map. `[:, name: 'example']` is a map
  and cannot stand for a manifest.
- **Load is atomic.** The manifest is validated whole, and registration
  is all or nothing: a rejected load leaves no hooks, keybindings, or
  handlers behind.
- **Registration is static.** A plugin cannot add a hook at run time. What
  it contributes is what its manifest says at startup.
- **No native ABI.** A plugin is ral source and nothing else: no shared
  object, no bytecode module, no guest interface. The interface it
  programs against is the set of hook events and `_ed-*` builtins below,
  all of them ral values and ral builtins.

The file's result is either a manifest, or a factory: a block of exactly
one parameter, the options map, returning the manifest. Top-level
bindings in the file are helpers; only the result matters.

```
{ |options| [
    name: fzf-files,
    keybindings: [[key: !{get $options key ctrl-t}, handler: { |_| … }]],
] }
```

A plugin with nothing to configure returns the manifest directly.
Options are first-order data (SPEC §15.5): they cannot carry blocks or
handles. A factory chooses its own defaults by reading
fields by name; there is no options schema.

| Field | Meaning |
|---|---|
| `name` | required; unique among loaded plugins |
| `hooks` | `[event: handler, …]` (section 4) |
| `keybindings` | `[[key: Str, handler: block, guard?: Str], …]` (section 5) |
| `handlers` | `[:, name: handler, …]`, or `[:]` for none (SPEC §9.4) |

The keyset is closed. In particular `capabilities:` is not an unknown key
but a refused one, with its own message, for the reason in section 2.

The checker generates the file, settles every remaining `let` mode and call
split, holds the manifest contract live, closes gates, nested tables and
errors, then freezes the checked tree. A contract solves types; it cannot
change elaboration or open holes. Factory shape and result equations run in
that same live phase. For example, `name:` can fix a decoded value's type to
text, so incompatible data is refused at its decode, naming the field use.

`handlers:` is a map, held by one equation against
`Map(U([String] → Command))`, as in the rc. Keys are data: written and
computed names follow the same rule. A record such as `[hi: …]` is refused;
write `[:, hi: { |args| … }]`, or `[:]` for none. The live equation reaches
sites inside locally checked handlers; imported handlers retain their module's
sites. Persistent handlers are also vetted by actual name at installation.
Unlike `within`, persistent installation must cover unresolved returns and
member types. Hooks and keybindings retain host validation; their results do
not enter typed code unchecked.

## 2 Authority

A plugin has no authority of its own to declare. A manifest field naming
capabilities would be decoration: nothing could enforce it against the
plugin's own code, and a reader would take it for confinement. So the
key is refused at load, and the advice is the message: wrap the
invocation in `grant`.

What a plugin may do is what the code that runs it may do.

- **Hooks and keybinding handlers** run in the session, under the
  session's grant stack: the root layer, narrowed by any `--capabilities`
  profile the session started under (SPEC §15.3). They fire from the
  read-eval loop, not from a user line, so no lexical `grant` encloses
  them. A plain `ral` withholds nothing; a session under a profile
  narrows every handler exactly as it narrows everything else. A handler
  may itself wrap risky work in `grant`.
- **Command handlers** run as commands in a user's line and push no grant of their
  own, so they run under whatever grant encloses the line that names
  them. A handler that calls `cd` inside `grant [shell: [`detach]] { … }`
  is refused like any other `cd`.
- **Loading** reads the plugin file through the current grant. Each
  candidate path the grant refuses is passed over for the next, and if
  all are refused the error names the first. Plugins named in the rc are
  loaded during bootstrap, before the command-line ceiling narrows the
  session (SPEC §15.3): the rc is trusted.

Two grant fields exist for plugin machinery (SPEC §12.8). `editor` lists
verbs gating the `_ed-*` builtins, `shell` lists verbs gating `cd` and
`detach`:

| Verb | Gates |
|---|---|
| `` editor: `read `` | `_ed-get`, `_ed-parse`, `_ed-history`, and the read half of `_ed-state` |
| `` editor: `write `` | `_ed-set`, `_ed-set-lbuffer`, `_ed-insert`, `_ed-push`, `_ed-accept`, `_ed-ghost`, `_ed-highlight`, `_ed-clipboard`, and the write half of `_ed-state` |
| `` editor: `tui `` | `_ed-tui` |
| `` shell: `chdir `` | `cd` |

A builtin that needs two verbs checks both before its first effect.
`_ed-state` checks `write` before its updater runs, so a refused call has
done nothing. `_ed-hyperlink` is a pure formatter and needs no verb.

A plugin that needs `cd` from a handler cannot grant it to itself. It
documents that the session needs `` shell: [`chdir] ``, and lives under
whatever the operator allows.

## 3 Architecture: split at the protocol

The `ral` executable is a host over an engine, and a plugin straddles the
two. The split is chosen so that the engine keeps no editor state and the
host keeps no ral values.

- **Engine side.** Hooks, keybinding handlers, and command handlers are ral values
  and live in the shell: the hook table and persistent handler frames, under a
  per-plugin namespace so that a failed load can remove
  exactly what was added. Handlers never cross to the host.
- **Host side.** The editor state, the keybinding router, the per-plugin
  state cells, and the first-order part of each manifest (name, which
  events, the key notations and guards) live in the REPL's plugin
  runtime. Only data crosses.
- **Between them.** The host drives a handler by dispatching a hook run
  to the engine. A handler reaches the editor by calling an `_ed-*`
  builtin, which puts the request to the host as an enquiry.

The consequence for plugin authors is a single rule: **the `_ed-*`
builtins work only inside a handler that carries an editor context**. That
is a `buffer-change` hook, a `complete` hook, or a keybinding handler.
Lifecycle hooks (`pre-exec`, `post-exec`, `chpwd`) and `prompt` carry
none, and an `_ed-*` call there, or at an ordinary prompt, or in a batch
host, is refused with a message saying so.

## 4 Loading

The host loads plugins from the rc's `plugins:` map during startup.
Each key is resolved in order (SPEC §15.5):

1. `<config>/ral/plugins/NAME.ral`, `<config>` being the user
   configuration directory;
2. `DIR/NAME.ral` for each `DIR` in `RAL_PATH`;
3. the literal path, then the literal path with `.ral` appended, read as
   any written path is (live working directory, `~`).

The file is typechecked and evaluated in a fresh scope under the
manifest's declared table, a factory is applied to its options, and the
result is parsed: handler arity, hook names, key notation, guard
regexes, and handler collisions are all checked before anything is
installed. Persistent command handlers already held statically by the map
equation are vetted again against their actual names at installation, yielding
only a verdict, no stored scheme. Runtime parsing also covers wholly
unresolved returns and unresolved field or member types. Hooks and handlers
are then committed and the host told; if the host refuses the manifest, the
whole namespace is rolled back. Errors are reported as `plugin 'NAME': …`,
with source spans in the plugin file.

The rc file's `plugins:` map supplies options (section 8); `[:]` supplies
only the plugin's defaults. Handing a non-empty options map to a plugin
that returns a manifest directly, as from an rc entry giving options to
one, is an error, since they would be silently dropped.

A duplicate plugin name is an error. There is no run-time loading or
unloading command; changing the plugin set requires a new session.

## 5 Hooks

A hook is a handler for one of six events. Hooks are registered
statically from the manifest and are never added at run time. The events,
their records, and their exact semantics are SPEC §15.6; the design
points are these.

**One argument.** Every handler takes exactly one: an event record, or
for `prompt` the prompt string. Registration checks the arity, so a
handler of any other shape is a load error, not a run-time surprise. A
record carries only what the event alone knows (`buffer-change` gives the
old buffer, not the new one); the editor is read through `_ed-*`.

| Event | Role |
|---|---|
| `buffer-change` | observe an edit; set ghost text, highlights |
| `complete` | claim the word under the cursor at Tab |
| `pre-exec` | rewrite the line about to run |
| `post-exec` | observe the line that ran and its status |
| `chpwd` | observe a change of the session's directory |
| `prompt` | transform the prompt string |

**Three shapes of composition.** Observers (`buffer-change`, `post-exec`,
`chpwd`) run for every plugin, in load order, and their values are
ignored. Transformers (`pre-exec`, `prompt`) fold: each handler receives
what the previous one returned. Claimants (`complete`) are tried in
order and the first non-empty answer wins, passing to ral's own
completion when none does.

**No handler can wedge the shell.** A failing handler is reported with
the label `plugin 'NAME' hook 'EVENT': ` and the others still run. A
failing `pre-exec` leaves the line as it was, so no hook can stop every
line. A line is refused by rewriting it to one
that fails, and its status reaches `post-exec` through the ordinary run:

```
hooks: [
    pre-exec: { |ev|
        if !{re-match '^rm -rf /' $ev[src]} {
            return "fail 'refused'"
        } else { return $ev[src] }
    },
]
```

`buffer-change` and `complete` fire on every keystroke and every Tab, so
they are held to a different standard. They run aside, so nothing they
do flows back into the session. A `buffer-change` hook's ghost text and
highlights are its output and are kept; a `complete` hook's writes to the
buffer, ghost text, and highlights are discarded. They are also braked per
plugin and event. A run that overruns its budget (100ms, 500ms), or
three failures in a row, disables that hook for the rest of the session
with one notice. A slow handler thus costs the user at most one stutter.

`prompt` is the one event that is a transformer of a string rather than
an event record, and it is read like the session prompt: a returned value
is the prompt, and otherwise captured output is.

```
hooks: [
    prompt: { |base|
        let b = try { return !{git branch --show-current | from-line} } { |_| return '' }
        if !{is-empty $b} { return $base } else { return "$base [$b] " }
    },
]
```

`complete` receives what `_ed-parse` returns, so a plugin completes the
word the way the editor sees it. It claims a word only where it has
something to say, and returns `[]` elsewhere:

```
hooks: [complete: { |[words: ws, current: i]|
    let head = intercalate ' ' !{take 2 $ws}
    if $[$i == 2 && !{elem $head ['git checkout', 'git switch']}] {
        let w = last !{take 3 $ws}
        let branches = try { return !{lines !{git branch '--format=%(refname:short)'}} } { |_| return [] }
        return !{filter { |b| $[!{slice $b 0 !{length $w}} == $w] } $branches}
    } else { return [] }
}]
```

The minimal frontend, used on dumb terminals, dispatches no
`buffer-change` or `complete` hooks and no editor keybindings, and shows
no ghost text or highlights. Command handlers and the other four events do not
depend on raw-mode editing and work there (SPEC §15.6).

## 6 Keybindings

A keybinding is `[key: Str, handler: block, guard?: Str]`. The design
is one ordered dispatch table shared by every frontend, so that
precedence is decided once and the frontends cannot disagree: plugins in
load order, manifest order within a plugin, and the editor's built-in
action as the tail.

**A guard decides, not the handler.** A press runs the first entry whose
keys match and whose `guard`, a regex tested against the text left of the
cursor, allows. Whether a binding claims a press is settled before its
handler runs, and the handler's return value carries nothing. There is
no fallthrough protocol for a handler to forget or get wrong. Several
guarded bindings on one key compose as an ordered pattern match, and an
unmatched press does the built-in thing: a guarded `tab` cooperates with
ral's own completion.

**Key notation has one spelling per key** (SPEC §15.6): chords are
`modifier-…-key`, a sequence is chords separated by spaces. Where a
terminal cannot distinguish two spellings (`ctrl-h` from `backspace`,
`ctrl-C` from `ctrl-c`, `shift-a` from `A`), the unreachable one is
refused, and the refusal names the spelling to use. A key that could be
written two ways would be bound twice and fire once.

**Load-time rules** keep the table honest:

- `ctrl-c` and `ctrl-d`, alone or in a sequence, are the session's escape
  hatches and cannot be bound. A wedged handler on either would leave no
  way to stop it or to leave the shell.
- An unmodified key other than `f1` to `f12` carries a built-in editing
  action, so binding it requires a `guard`; otherwise the binding would
  shadow typing on every press. Modified chords and function keys may be
  unguarded, since replacing the editor default (`ctrl-t`, `alt-c`,
  `ctrl-r`) is the point.
- A sequence begins with `ctrl-x`, the prefix the line editor leaves free.
- A binding that can never fire loads with a warning naming what claims
  its keys first: an earlier unguarded binding on the same keys, or an
  unguarded binding on a proper prefix of the sequence, wherever loaded.

A handler receives `[key: Str, mode: Str]`: the binding's notation and the
editor's input mode at the press (`insert`, `command`, `replace`; always
`insert` in emacs). It reads the buffer with `_ed-get` and acts through
the other `_ed-*` builtins. When it returns without `_ed-accept`, the
editor re-enters with the handler's final buffer and cursor.

A keybinding handler is the one run that holds the terminal, which is
what `_ed-tui` needs (section 7).

## 7 The editor interface

The `_ed-*` builtins are the whole of a plugin's access to the editor.
They are installed only by the interactive host and hidden from `help`.
Each is its own builtin, not one `_ed` command with an op argument, so
that the checker sees each op's real return type and arity is fixed per
op. Signatures and contracts are SPEC §15.7. What a plugin author needs
to know that the table does not say:

- **Offsets are characters**, as in `length` and `slice`, never bytes.
  The cursor is clamped into the text after every write.
- **State is read whole.** `_ed-get` answers text, cursor, and the
  configured keymap (`emacs` or `vi`); the live vi mode arrives instead
  with a keybinding's `mode`.
- **`_ed-push` and `_ed-accept`** are zsh's `push-line` and `accept-line`.
  The pushed buffer returns at the next prompt, so a handler can run a
  generated command and give the user back the line they were typing.
- **`_ed-ghost` and `_ed-highlight`** set display state, not input. Among
  plugins the last in load order with non-empty ghost text wins. Each
  highlight call replaces the spans that run had set, spans from several
  plugins are composited with the later plugin on top, and out-of-range
  offsets are clamped. The style names are a fixed vocabulary (`command`,
  `builtin`, `prelude`, `argument`, `option`, `path-exists`,
  `path-missing`, `string`, `number`, `comment`, `error`, `match`,
  `bracket-1`, `bracket-2`, `bracket-3`); an unknown one is an error.
- **`_ed-parse`** is the lexer's simple command at the cursor: its tokens
  split at `|`, `?`, `;`, newlines, and braces, with an open `{` or `[`
  read as closed, because a buffer is routinely mid-block. A cursor in
  whitespace stands on a fresh `''` at its own offset, so
  `words[current]` is always the word being typed. `words` is empty
  exactly when the buffer does not lex.
- **`_ed-history`** returns entries most recent first, deduplicated;
  limit `0` means all.
- **`_ed-state`** is the plugin's one persistent cell, held by the host
  for the session. The first call runs the updater on the default,
  later calls on the stored value; to read without changing, return the
  argument. The cell is a boundary: what it holds was written by an
  earlier run, so it is admitted against the type this call uses it at,
  before the updater runs, and refused with the field named when it does
  not fit. The updater's result must be first-order data.
- **`_ed-clipboard`** copies natively when ral runs locally and by OSC 52
  otherwise, and answers whether all of the text landed. It needs
  `editor: `write`.

**`_ed-tui`** runs a body with the editor suspended, stdout captured, and
the terminal lent to it, so that a fuzzy finder can draw while its pick
comes back as a string. The body is a command, its value its output; the
captured bytes are decoded lossily after stripping exactly one trailing
LF or CRLF. The call never raises. On success `status` is `0`; when the
body fails, `status` is its exit code and `output` its error message, so
a plugin tells cancellation (fzf's 1 and 130) from a real error without
wrapping in `try`. The terminal is lent only to a keybinding handler.
From any other hook, or nested inside another `_ed-tui`, the call
reports status 1 and says why instead of raising, since those runs have
no terminal to lend.

## 8 Configuration

Plugins are configured, not scripted into place, by the rc file's
`plugins:` map (SPEC §15.3): from plugin name or path to that plugin's
options map, forwarded to the plugin's factory as its single argument.

```
return [
    plugins: [
        autosuggestion: [:],
        fzf-files:      [key: 'ctrl-t'],
        zoxide:         [key: 'alt-z'],
    ],
]
```

`[:]` is the empty map, for a plugin taking no options. A value that is
not a map is reported by name and that plugin skipped; the rest load. A
plugin that fails to load is reported and skipped, never fatal to the
session. Plugins from `plugins:` load after the rest of the rc has taken
effect, in alphabetical order of name (ral maps are ordered by key), so
load order, and with it hook and keybinding precedence, does not depend
on how the file was written. To load conditionally, compute the map:

```
let plugins = if !{succeeds { which fzf > /dev/null }} {
    return [autosuggestion: [:], fzf-files: [:], fzf-history: [:]]
} else {
    return [autosuggestion: [:]]
}
return [env: [EDITOR: 'nvim'], plugins: $plugins]
```

## 9 Examples

The plugins shipped in `plugins/` are the working examples, and each is
loaded in CI (`just test`).

- `autosuggestion.ral`: fish-style ghost text from history, one
  `buffer-change` hook over `_ed-get`, `_ed-history`, and `_ed-ghost`.
- `zoxide.ral`: handlers (`z`, `zi`), a `chpwd` hook, and a keybinding
  with `_ed-tui`, with a configurable key.
- `fzf-files.ral`, `fzf-cd.ral`, `fzf-history.ral`: fzf's `CTRL-T`,
  `ALT-C`, and `CTRL-R` widgets. They port fzf's own `key-bindings.zsh`,
  and what those scripts share (the option stack, the `fzf-tmux` switch,
  the exit statuses) is one module, `plugins/lib/fzf.ral`, bound with
  `use` at each plugin's top level, where the path resolves beside the
  plugin file.
- `fzf-completion.ral`: `**`-trigger completion on a guarded `tab`, so a
  plain Tab still reaches ral's completer.

`fzf-cd` shows the whole shape: a factory, one keybinding, the shared
picker with a continuation that runs only on a selection, and the
`push`, `set`, `accept` triple.

```
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

A small `buffer-change` sketch, dimming everything after a `#`:

```
let dim-comment = { |_|
    let text = !{_ed-get}[text]
    let [code, ...rest] = re-split '#' $text
    if !{is-empty $rest} { _ed-highlight [] } else {
        _ed-highlight [[start: !{length $code}, end: !{length $text}, style: 'comment']]
    }
}
return [name: 'dim-comment', hooks: [buffer-change: $dim-comment]]
```

## 10 Deliberately not supported

Each of these is a choice, not a gap.

- **A capability manifest.** See section 2. Confinement is `grant` and
  the session ceiling.
- **Run-time registration.** Hooks, keybindings, and command handlers come
  only from manifests loaded at startup.
- **Return-value fallthrough in keybindings.** A guard claims a press
  before the handler runs.
- **Non-first-order options.** A plugin that wants a block of its own
  writes it in the plugin.
- **Plugins outside the interactive `ral`.** Batch hosts, exarch, and
  synod load no REPL plugins and install no `_ed-*`; portable code must not
  assume them (SPEC §15, §15.8).
- **Key sequences not under `ctrl-x`, and a bindable `ctrl-c` or
  `ctrl-d`.** See section 6.

Wanted, not built:

- **Left and right prompt hooks.** `prompt` transforms the whole prompt
  string, which suffices for decorating the left and leaves no clean way
  to contribute to a right-hand prompt.
- **Asynchronous prompt segments.** A `prompt` handler runs to
  completion before the prompt draws; slow work such as VCS queries
  wants a cached result.
- **Highlight-style overrides.** The style vocabulary is fixed and the
  rc keyset has no key to remap it.
- **`_ed-parse` over the parser.** It splits the lexer's tokens, so a
  cursor inside `$[…]`, a list literal, or a redirect target is not seen
  for what it is.
