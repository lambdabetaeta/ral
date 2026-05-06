# ral — design rationale

Not part of the specification. The choices below explain the surface
language; §20 of the spec fixes the underlying calculus.

## Influences

- **rc** (Plan 9) and **es** (Haahr–Rakitzis): lists and functions as
  values; control structures in a library, not the grammar.
- **Algol 60**, **Modernised Algol**: block structure, lexical scoping,
  and the distinction between a value and a computation producing one.
- **Call-by-push-value** (Levy): a formal calculus in which that
  distinction is primitive, with thunks first-class.
- **Haskell**, **Backus**: immutable bindings, combinators, equational
  reasoning in the pure fragment.
- **Tcl**: commands are ordinary names, resolved at evaluation time,
  not keywords.
- **Shill** (Moore et al.) and capability systems: authority is
  explicitly delegated and may be attenuated, never amplified.
- **JavaScript**, **Rust**: destructuring, closures, spread, string
  interpolation.

The closest relatives are **YSH/Oil** and **nushell**; ral differs by
not retaining POSIX compatibility and by not enforcing structured data
on every pipeline stage.

## Values and commands

The organising distinction is between *values* — inert data, named,
passed, inspected — and *commands* — effectful processes that may read,
emit bytes, return a value, or fail. Most shells collapse the two:
every datum is a string, and every string is simultaneously data, a
command name, and source text for further evaluation. The consequence
is that captured output is re-lexed, split on whitespace, and
glob-expanded. ral refuses this collapse and thereby avoids the class
of bugs arising from it, without sacrificing first-class commands and
pipes.

The formal account is call-by-push-value (Levy): values are inert,
computations effectful, a thunk packages a computation as a value, and
forcing runs it. The user need not know the theory to use the shell:
`{M}` thunks, `!` forces.

## Blocks as the single abstraction mechanism

`{M}` stores a command; `{ |x| M }` is a function; `!` runs either.
This replaces the several mechanisms of conventional shells —
functions, aliases, `eval`, subshells, trap handlers — with one.

Forcing is always explicit, with a single exception: a bound name
resolved in head position is forced (or applied) implicitly. This
keeps ordinary calls natural (`greet alice`) while making the storage
of commands visible (`let plan = { make build }`).

## Two sigils

`$` retrieves data; `!` runs stored commands. `!$b` composes: dereference
then force. A single sigil covering both would make "retrieve" and
"run" indistinguishable at a glance, and the ambiguity would propagate
into every expression that passes blocks as data.

## Shadowing, not mutation

All `let` bindings are immutable; re-`let` shadows within the enclosing
scope. Closures capture at definition time, so equational reasoning
holds in the pure fragment. The cost is the absence of mutable
accumulators; `fold`, `reduce`, and the streaming `fold-lines` replace
them. The benefit is twofold: easier local reasoning, and a `spawn`
that is safe without synchronisation — the isolated copy shares
nothing that can be mutated concurrently.

## External commands return strings

When an external command's stdout is captured in a `let`, the runtime
decodes it as UTF-8, strips one trailing newline, and binds the result
as `String`. Invalid UTF-8 fails with a message naming the command
and suggesting `| from-bytes`; `Bytes` remains available via that
terminator.

This trades a sliver of generality for a large reduction in
ceremony: most command output is text, and demanding an explicit
decode on every binding generates noise without adding protection
beyond what a strict error already gives. The returned `String` is
data, never re-lexed, split, or globbed; the classic
capture-then-reparse chain does not arise.

## Piping and failure

`|` and `?` are deliberately separate: `|` moves data between stages;
`?` reacts to command failure. Exit status and data flow remain
distinct concerns. `if` branches on `Bool`, not on command success; a
predicate returning `false` still *succeeds*, so confusing "false"
with "failed" is impossible. When success must be inspected as data,
`try` is the mechanism.

## No command-level `||`

`try { a } { |_| b }` replaces `a || b` in command context. A binary
`||` on pipelines would force precedence rules relative to `?` and
`|`, adding grammar for a case `try` already handles. The `||`
operator that *does* exist is the Boolean connective inside
expression blocks `$[a || b]` (§2, SPEC) — it operates on `Bool`
values, not on command success.

## Expression blocks

`$[...]` is one expression language spanning arithmetic
(`+ - * / %`), comparison (`== != < > <= >=`), and logic
(`&& || not`).  Bash partitions these into `(( ))` and `[[ ]]`
because its history forced separate lexers; ral has no such
history.  Comparisons already cross the numeric/Boolean boundary
by returning `Bool`, so the simplest and most honest grammar is
one.  `&&` and `||` short-circuit; operands are strict `Bool` —
`$[1 && true]` is a type error, not truthy.  The `not` keyword
carries unary negation because `!` is already force (`!{...}`)
and `~` is tilde expansion; context-dependent symbol overloading
would be the worse trade.

## Data-last argument order

`map f items`, `fold f init items`, `filter p items`. Piping and
partial application then align: `items | map $f | filter $p` reads
left-to-right, and `map $f` is a function waiting for its list.

## No context-dependent lexer rules

`:` and `=` are in `BARE` but not `IDENT`. Names (`IDENT`) therefore
terminate naturally on these characters, while in command arguments
(`BARE`) expressions such as `-DFOO=bar` and `http://host:port` remain
single tokens. The lexer is single-pass and modeless.

## Scoped execution contexts

`within` and `grant` are properties of the execution context, not of
source text: a function defined in one module and called inside a
restricted block runs under that restriction. `within [env: [KEY:
VAL]] { body }` overrides environment variables; `within [dir: PATH] {
body }` overrides the working directory. Both are facets of a single
scoping primitive. Lexical capture is the right model for data; dynamic
inheritance is the right model for ambient authority.

## Path construction uses interpolation

Outside quotes, `$name` is a separate atom — `$dir/file` is two
arguments. Paths are built by interpolation: `"$dir/file.txt"`. This
inverts the bash convention, where quoting suppresses word-splitting;
in ral the unquoted form is already safe (there is no splitting), and
quoting performs concatenation.

## Not POSIX

POSIX shell compatibility requires word-splitting, glob expansion on
unquoted variables, `$IFS`, and context-dependent quoting. ral
eliminates exactly these. Compatibility is therefore a non-goal.

## Control structures are library, not syntax

The grammar has no knowledge of `if`, `for`, `while`, `try`, `case`:
they are prelude functions taking blocks as arguments. The parser
stays small, the surface uniform; a user can define new control
structures with the tools the prelude uses.

## Aliases are semantic, not syntactic

Aliases live in the interactive command namespace, resolved at
evaluation time after value-head lookup, active only in interactive
mode. Scripts never see them, so script behaviour cannot depend on
the user's interactive configuration.

## `guard`, not `on EXIT`

`guard` wraps a body, runs cleanup regardless of outcome, and
propagates the original failure unchanged: scoped and lexically
apparent. Registration-based cleanup (`on EXIT`) is mutable global
state whose ordering follows execution flow rather than source
structure, and composes poorly with nested error handling.

## Termination: `return`, `fail`, `exit`

Scripts end at the last statement.  Three primitives end them earlier,
each with its own scope:

- `return` exits the current block or file with success.  Inside a
  sourced file, it stops *that* file, not the caller — so a `return`
  in a library never kills the script that loaded it.
- `fail` aborts the current evaluation with nonzero status and an
  error record:

      fail [status: 1]
      fail [status: 7, message: 'config missing']
      fail $e                          # re-raise inside a try handler

  Errors are values, not numbers.  The record produced by `try { ...
  } { |e| ... }` is the input shape `fail` accepts, so wrap-and-rethrow
  composes without dropping fields.
- `exit N` (alias `quit`) terminates the whole shell process with
  status `N`.  Reserved for top-level use; scripts that want to halt
  cleanly should prefer `return`.

## `_try` and `_audit` are separate builtins

`_try` captures a flat failure record; `_audit` builds the full
execution tree regardless of outcome. Separating them keeps the
common case (catch-and-handle) from paying for the uncommon one
(full tracing).

## `source` is kept for configuration

`~/.ralrc` and interactive configuration need scope merging, which
`use` (returning a module map) does not do. `source` exists for this;
`use` remains the default for library code.

## Concurrency: isolation, not shared state

`spawn` creates an isolated copy of the evaluator; there is no shared
mutable state and no synchronisation. `await` is the only channel.
A second `await` on the same handle returns the cached result,
avoiding the need for affine types or runtime traps on aliased
handles.

A spawned handle buffers its output and replays it on `await`; a
watched handle (`watch "label" P`) streams each line live to the
caller's stdout, prefixed `[label] ` (stdout) or `[label:err] `
(stderr). `watch` is a one-line prelude alias over the `_watch`
builtin, not a keyword. The framing lives in a single `Sink`
variant — `LineFramed` — that buffers bytes until `\n` and emits
`prefix + line + '\n'` as one write to the caller's stdout;
sibling watchers serialise through the OS stdout lock (or, under
the interactive REPL, rustyline's external printer) so each line
is atomic even when several watchers run concurrently. Live
watching hides the usual `cmd > /tmp/log &; tail -f` scaffolding
behind a library function. Ral deliberately does not ship a
read-API on handles (`read-line $h`, `select-line [h₁,h₂]`): value
builtins like `each` are value-complete, so a
handle-as-pipe-source would require a streaming-internals
refactor, whereas line-framed watching satisfies the observed
motivating use case at a much smaller surface.

## Paths are strings

No `Path` type. UTF-8 for textual values, and the absence of word
splitting removes the historical reason shells needed path-specific
quoting.

## `let` unifies binding, capture, and storage

The `let` RHS is a command context, and a single mechanism covers
three operations:

- `let x = foo`        runs `foo` and binds its result;
- `let x = 'foo'`      binds the string;
- `let f = { |x| … }`  stores the block.

Bare words run commands; quoted words are data; value forms
(literals, blocks, lists, maps, derefs, arithmetic) receive an
implicit `return` in command context. The shell convention
(unquoted words run commands) is preserved without collapsing the
language into strings.

## Three layers, one asymmetry

The filesystem surface is split into three layers:

1. **Structured queries** — primitives that return values: `list-dir`,
   `file-size`, `file-mtime`, `file-empty`, `path-*`, `grep-files`,
   `temp-dir`, `temp-file`. These are what drives a structured
   pipeline; they have no shell-tool analogue worth bothering with.
2. **Bytes I/O** — codecs (`from-string`, `to-json`, …) plus redirects.
   `to-json $v > $path` replaces the old `write-json`; `from-string
   < $path` replaces a read-file primitive. Atomic-rename-on-write is
   built into `>` for regular files.
3. **Filesystem effects** — bundled coreutils (`cp`, `mv`, `rm`,
   `mkdir`, `ln -s`, `chmod`, …). Effects don't return structured
   values, so giving them ral-native primitives buys nothing the shell
   form doesn't already give. The old `copy-file`, `move-file`,
   `make-dir`, `remove-file` wrappers are gone for this reason.

The asymmetry is the design: structured returns earn a primitive;
effects don't.

## `remove-file` was a footgun

The dropped `remove-file` did `rm -rf` if you pointed it at a
directory. That is the kind of behaviour ral exists to abolish. The
dangerous verb wears its name — `rm -r` (or `rm -rf`) — and the
caller writes it on purpose. Effects are bundled coreutils now;
the trap goes away.

## Bundled coreutils are mandatory in exarch, optional in ral

A sealed exarch profile that depends on host coreutils isn't sealed —
it's reproducible only modulo whatever `cp` or `mv` the host happens
to ship (BSD vs GNU drift, version skew, locale defaults). Exarch
therefore bundles a curated coreutils set and pins behaviour. The
binary-size cost is paid once per profile build and is the price of
"I know exactly what's in this".

The bare `ral` binary keeps coreutils behind a feature flag. An
interactive shell on a developer machine has system coreutils
already; no reason to ship 30+MB of duplicate tools.

## Capability-checked dispatch for bundled tools

Every uutils invocation goes through a wrapper that consults the
tool's own clap parser to find the path-argv positions, then calls
the same `check_fs_read` / `check_fs_write` that the structured
primitives use. Bypassing the sandbox by reaching for `cp` instead
of a primitive is therefore not possible — both paths land at the
same chokepoint. `within [dir: ...]` scope propagates by chdir under
a per-call lock, so relative paths resolve against ral's scoped CWD,
not the host process CWD.

## Syscall bridge, not text parsing

The structured query primitives (`_fs lines/size/mtime/empty/list`,
`_path …`) replace shelling out to `stat`, `dirname`, `basename`,
etc. and parsing their text. Platform differences and the perpetual
bytes–text–structured round-trip disappear. Effects are not in the
bridge — they are bundled commands invoked through the
capability-checked dispatch.

## Record types and scoped labels

The checker infers per-field types for map literals with static keys.
Representation is a row: a list of `(label, type)` pairs with an
optional tail variable standing for unknown fields. Field access
unifies the target with `[label:α | ρ]` and returns `α`. The unifier is
Rémy (1989): mismatched head labels permute past each other into a
shared fresh tail.

The spread `[...$base, port: 9090]` raises the question of duplicate
labels: if `$base` already has `port`, the result has two. Rémy's
original system assumes uniqueness and would require absence markers
(`Pre(T)` / `Abs`) and a restriction operator `ρ ∖ port`. Introducing
them means new row constructors and changes to unifier, generaliser,
and display.

ral instead adopts the scoped-label row types of Leijen (2005).
Duplicates are permitted in rows; selection always takes the first;
extension prepends, shadowing the prior entry rather than removing it.
The key observation is that the Rémy rewrite rule already treats
duplicates correctly — it swaps only *different* labels past each
other, so same-label entries keep their relative order. No changes to
unifier, generaliser, or occurs check are required.

Effect: `[...$base, port: 9090]` with `$base : [host: String | ρ]` infers
as `[port: Int, host: String | ρ]`. The explicit field prepends over the
spread's row variable, which becomes the result's open tail. Shadowed
duplicates are invisible to selection and suppressed in display. With
multiple spreads the result is open but imprecise — chaining two
arbitrary rows needs row concatenation, which is not part of Leijen's
system and is not included.

## Plugins are modules

A plugin is an ordinary ral module (§8) whose return value is either
a manifest map or a block that takes an options map and returns a
manifest map. There is no plugin DSL, no separate loader language,
and no magic `$config` binding: a plugin's knobs are fields on the
options map it receives. `_plugin 'load' 'fzf-files' [key: 'ctrl-t']`
evaluates the file, applies the options map to the returned block,
and reads `name`, `capabilities`, `hooks`, `keybindings` off the
resulting record. Record destructuring, row polymorphism, and
`grant` already exist for other reasons; the plugin system is a
thin composition of them.

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

The shell wraps each handler in `grant $capabilities { … }` before
installing it, so a plugin runs with exactly the authority it
declared. A handler that tries to do more fails at the capability
check, not on trust.

## `_editor 'tui'` captures stdout

Interactive plugins invoke fuzzy finders (`fzf`, `sk`, …) that draw
on the terminal via `/dev/tty` and print the user's selection on
stdout. A plugin needs that selection as a value. If the body's
stdout went to the terminal, the selection would appear above the
prompt and the handler would get nothing back.

`_editor 'tui'` therefore opts into byte capture for the duration of
its body, analogous to what `let x = !{ … }` does at a binding site.
When the body returns `Unit`, the captured bytes (trimmed of one
trailing newline) become the return value; when it returns something
non-`Unit`, that wins. This is the same "last command's bytes are
the value" rule as `let`, applied inside a higher-order builtin.

```
let dir = ed-tui { fzf --walker dir +m }
```

## Keybinding dispatch is handler composition

Multiple plugins can bind the same key. Dispatch walks handlers in
reverse load order; a handler returning `true` consumes the
keystroke, a handler returning `false` falls through to the next,
and if every plugin handler declines the shell runs its built-in
binding for the key.

This is the same shape as the `?` fallback chain for commands: a
stack of alternatives where each one decides whether to handle or
pass. Load order controls precedence, the same way `use` order does
for bindings, so the user's last-loaded plugin wins by default and
earlier plugins remain reachable.

```
# if autosuggest's CTRL-F doesn't apply, the built-in binding still runs
load-plugin 'autosuggest' [:]
```

## Plugin code runs on the real env under the plugin's grants

Hooks (`buffer-change`, `pre-exec`, `post-exec`, `chpwd`, `prompt`)
fire on the live evaluator, not a clone. They need to observe and
sometimes alter shell state — the `prompt` hook returns the prompt
segment, `chpwd` may update state cells. The shell wraps each handler,
keybinding, and plugin alias in `grant $caps { ... }`, pushed on top of
the caller's current capabilities stack. The plugin's declared capabilities are
an authority ceiling, and any enclosing caller grant can only narrow them.

Handlers for an event run in load order; a failing handler's error
is logged but does not cancel siblings. `buffer-change` runs under
a soft deadline (default 16ms) so a slow plugin cannot make typing
feel laggy; stale handlers are re-run at the next input idle.



### Shadowing looks like mutation

```
let count = 0
for [1, 2, 3] { |x|
    let count = $[$count + 1]   # shadows in the loop body
}
echo $count                      # 0 — outer binding untouched
```

Idiomatic replacement:

```
let count = fold { |acc x| return $[$acc + $x] } 0 [1, 2, 3]
```

### A block returns its last result, not its full stdout

```
let b = { echo one; echo two }
let x = !$b                    # "two", not "one\ntwo\n"
```

In a `let` RHS only the last command's byte output is captured as the
bound value. Earlier commands run for effect: their bytes are not
captured but are still forwarded to the terminal.

```
let x = !{ echo "log: starting"; hostname }
# "log: starting" appears on the terminal; x is the hostname
```

### Return values do not enter pipes

```
let helper = { |x|
    echo "log: $x"
    return [result: $x]
}
let r = helper foo        # captures the map
helper foo | grep log     # pipes stdout; the map is discarded
```

`let` binds command results; `|` carries the byte stream (or
structured values on a value edge). These are separate channels.

### Early exit from `for`

`for` stops on failure; there is no `break`. For early-termination
logic use a dedicated combinator: `take-while` / `drop-while` for
pure predicates, `first` for "find the first item matching a
predicate" (it fails if no item matches), or an explicit `fold` that
threads a decision through the accumulator.

### Inline function application needs braces

```
_if !$pred $head { … } { … }     # wrong — 4 atoms to _if
_if !{$pred $head} { … } { … }   # right — 3 atoms, middle one evaluates the call
```

`!$pred` is one atom (force of the pred value); `$head` is another.
Writing them side-by-side passes both to the surrounding command as
separate arguments. To inline a call, wrap the application in braces
and force the block: `!{$pred $head}` evaluates to the call's return
value and occupies one atom position.
