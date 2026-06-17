# ral

A shell grounded on algebraic effects.

ral is a Unix shell built on one idea: **running an external command is
performing an algebraic effect.** A command like `git` is an *operation* — it is
performed, it returns once, and its meaning is supplied by its *interpretation*,
which by default is the operating system carrying out the syscall. Separating
the operation from its interpretation is what gives ral typed commands, scoped
authority, auditable execution, and a sandbox, all from one principle.

A companion idea keeps data honest: ral refuses the usual shell collapse between
data and source text. Most shells treat every datum as a string and every string
as at once data, a command name, and source for re-evaluation — so captured
output is re-lexed, word-split, and glob-expanded. ral keeps values and commands
apart and removes that whole class of bugs, without giving up first-class
commands and pipes.

## Design, in brief

**System calls are algebraic effects.** An external command is an *operation*
performed against the outside world; the OS is its default *interpretation*. The
language's own value-language — builtins, the prelude — is pure and performs no
effect. From this one identification follow scoped authority (`grant`), the
capability check at the point an effect is performed, audit as a trace of
operations, and failure as an operation's exceptional outcome.

**Values and commands are different things.** Values are inert data; commands
are effectful and may emit bytes, return a value, or fail. The formal account is
call-by-push-value: `{M}` packages a command as a value, `!` runs it. Two
sigils, no ambiguity: `$` retrieves data, `!` runs stored commands.

**Handlers are an orthogonal layer.** Because an operation is separate from its
interpretation, ral lets you *reinterpret* one: `within [handlers: …] { body }`
shadows a command's default meaning over the dynamic extent of `body` — for
mocking, redirection, instrumentation. This is optional and additive; the effect
principle stands without it. ral's handlers are a deliberately small fragment —
deep, self-masking, resumed exactly once in tail position, with no first-class
`resume` — less than algebraic-effect handlers usually offer, and matched to
effects whose results come from the world and are consumed once.

**Authority is dynamic; data is lexical.** `within` and `grant` scope ambient
authority — working directory (`dir:`), environment (`env:`), capabilities — over
a body's whole dynamic extent, so a function defined anywhere respects the
restriction in force where it is called. `let` bindings, by contrast, capture
lexically at definition.

**Capabilities, not trust.** `grant` attenuates authority by intersection and
can never amplify it — a capability is permission over the effect set. A
misbehaving plugin fails at the capability check, not on trust.

**Immutable bindings, shadowing not mutation.** `let` always introduces a fresh
binding; closures capture at definition time. Equational reasoning holds in the
pure fragment, and `spawn` is safe without synchronisation because the child is
an isolated copy that shares nothing mutable. `await` is the only channel; a
second `await` returns the cached result.

**Control structures are a library.** `if`, `for`, `each`, `try`, `case` are
prelude functions taking blocks — not grammar. The parser stays small; a user
can define new control forms with the same tools.

**Pipes and failure are separate.** `|` moves data between stages; `?` reacts to
command failure. `if` branches on `Bool`, never on exit status, so "false"
cannot be confused with "failed". When success must be inspected as data, `try`
is the mechanism. There is no command-level `||`.

**One expression language.** `$[...]` spans arithmetic, comparison, and logic
with strict `Bool` — no `(( ))` versus `[[ ]]` partition.

**Typed values.** `Bool`, `Int`, `Float`, `String`, `List`, `Map`, `Block`,
`Lambda`, `Handle`. Maps are inferred as row-typed records using Leijen's scoped
labels; spread and shadowing compose cleanly.

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
