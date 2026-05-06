# ral

A shell grounded on algebraic effects.

ral is a Unix shell that refuses the usual collapse between data and source
text. Most shells treat every datum as a string and every string as
simultaneously data, a command name, and source for further evaluation — so
captured output is re-lexed, split on whitespace, and glob-expanded. ral keeps
the two apart and thereby removes the class of bugs that follow from the
collapse, without giving up first-class commands and pipes.

## Design, in brief

**Values and commands are different things.** Values are inert data; commands
are effectful processes that may emit bytes, return a value, or fail. The
formal account is call-by-push-value: `{M}` packages a command as a value, `!`
runs it. Two sigils, no ambiguity: `$` retrieves data, `!` runs stored
commands.

**Algebraic effects.** External commands are operations; `within [handlers:
...] { body }` installs handlers that intercept them — per-name or catch-all.
The same primitive scopes the working directory (`dir:`), environment (`env:`),
and capability grants. Lexical scope governs data; dynamic inheritance governs
ambient authority.

**Immutable bindings, shadowing not mutation.** `let` always introduces a
fresh binding; closures capture at definition time. Equational reasoning holds
in the pure fragment, and `spawn` is safe without synchronisation because the
child is an isolated copy that shares nothing mutable. `await` is the only
channel; a second `await` returns the cached result.

**Control structures are a library.** `if`, `for`, `while`, `try`, `case` are
prelude functions taking blocks — not grammar. The parser stays small; a user
can define new control forms with the same tools.

**Pipes and failure are separate.** `|` moves data between stages; `?` reacts
to command failure. `if` branches on `Bool`, never on exit status, so "false"
cannot be confused with "failed". When success must be inspected as data,
`try` is the mechanism. There is no command-level `||`.

**One expression language.** `$[...]` spans arithmetic, comparison, and logic
with strict `Bool` — no `(( ))` versus `[[ ]]` partition.

**Typed values.** `Bool`, `Int`, `Float`, `String`, `List`, `Map`, `Block`,
`Lambda`, `Handle`. Maps are inferred as row-typed records using Leijen's
scoped labels; spread and shadowing compose cleanly.

**Capabilities, not trust.** `grant` attenuates authority by intersection; it
cannot amplify. Plugins are ordinary modules whose handlers run under the
capabilities they declared — a misbehaving plugin fails at the capability
check, not on trust.

**Not POSIX.** POSIX compatibility requires word splitting, glob expansion on
unquoted variables, `$IFS`, and context-dependent quoting. ral eliminates
exactly these.

See [docs/RATIONALE.md](docs/RATIONALE.md) for the full rationale and
[docs/SPEC.md](docs/SPEC.md) for the specification.

## Install

```sh
curl -fsSL https://lambdabetaeta.github.io/ral/scripts/install.sh | sh
```

Or from source:

```sh
cargo install --path ral
```

On first interactive run, ral creates a skeleton `rc` file and prints its path.

## Usage

```sh
ral                       # interactive
ral script.ral arg1 arg2  # run a script; $args == [arg1, arg2]
ral -c 'echo hello'       # inline
ral --check script.ral    # syntax check
ral --dump-ast script.ral # dump the AST
```

## License

Dual MIT / Apache-2.0.
