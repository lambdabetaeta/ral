# ral

A shell grounded on algebraic effects.

ral is a shell built on one idea: **running an external command is
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

## A taste

Everyday use looks like a shell, because it is one; the difference is what
cannot happen. `$file` is one argument whatever whitespace it holds — there is
no word splitting to quote against.

```
let file = 'my report.txt'       # spaces and all
let nlines = wc -l < $file       # capture binds stdout, trailing newline stripped
echo "$file has $nlines lines"
rm $file                         # exactly one argument

curl $primary ? curl $fallback   # ? reacts to failure; | moves data
```

Pipelines are typed. External commands carry bytes; internal functions carry
values; codecs bridge the two. A stage whose input does not match its
predecessor's output is rejected before any process starts.

```
cat foo.txt | head -10                     # entirely external
glob '*.rs' | map { |f| wc -l $f }         # entirely internal
let cfg = curl -s $url | from-json         # decode at the boundary, capture the value

par { |f| convert $f } !{glob '*.wav'} $nproc   # parallel map; nothing mutable is shared
```

Authority is scoped, and it can only shrink:

```
grant [
    exec: [git: 'allow', make: 'allow', '/usr/bin/': 'allow'],
    fs:   [read: ['/home/project'], write: ['/tmp/build']],
    net:  false,
] {
    git clone $repo      # permitted
    make build           # permitted
    curl $url            # denied — not in exec, and net is off
}
```

The [tutorial](docs/TUTORIAL.md) teaches the language from nothing.
[examples/](examples) holds complete scripts, most set against the bash idiom
they replace.

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

**Typed values, checked before execution.** `Bool`, `Int`, `Float`, `String`,
`List`, `Map`, `Block`, `Lambda`, `Handle`, all inferred. Maps are inferred as
row-typed records using Leijen's scoped labels; spread and shadowing compose
cleanly. Type errors surface before any process runs; `ral --check` runs the
checker alone.

**Not POSIX.** POSIX compatibility requires word splitting, glob expansion on
unquoted variables, `$IFS`, and context-dependent quoting. ral eliminates
exactly these.

See [docs/RATIONALE.md](docs/RATIONALE.md) for the full rationale and
[docs/SPEC.md](docs/SPEC.md) for the specification. Both are rendered, with the
tutorial, at <https://lambdabetaeta.github.io/ral/>.

## Install

```sh
curl -fsSL https://lambdabetaeta.github.io/ral/scripts/install.sh | sh
```

Via Homebrew — this repository is itself a tap; tap it once by URL, then
install by short name:

```sh
brew tap lambdabetaeta/ral https://github.com/lambdabetaeta/ral
brew install ral       # the shell, with ral-sh
brew install exarch    # optional: the coding agent
```

On Windows:

```powershell
irm https://lambdabetaeta.github.io/ral/scripts/install.ps1 | iex
```

Or via [Scoop](https://scoop.sh), installing the manifest directly (no bucket
to add):

```powershell
scoop install https://raw.githubusercontent.com/lambdabetaeta/ral/main/packaging/scoop/ral.json
```

A [winget](https://learn.microsoft.com/en-us/windows/package-manager/winget/)
manifest lives at
[`packaging/winget/`](packaging/winget/manifests/l/lambdabetaeta/ral/0.1.0),
ready for submission to `microsoft/winget-pkgs`; until it lands there,
install from the local manifest:

```powershell
git clone https://github.com/lambdabetaeta/ral
winget install --manifest ral/packaging/winget/manifests/l/lambdabetaeta/ral/0.1.0
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
ral --check script.ral    # parse and type-check; do not execute
ral --audit script.ral    # run; emit the execution tree as JSON on stderr
ral --dump-ast script.ral # dump the AST
```

## As a login shell

`ral-sh` is the login-shell shim: interactive sessions get ral, while
non-interactive invocations are forwarded to `/bin/sh`, so POSIX-assuming
tools — scp, rsync, git-over-ssh — never notice. Register it and switch:

```sh
sudo sh -c "echo $(command -v ral-sh) >> /etc/shells"
chsh -s "$(command -v ral-sh)"
```

## Windows

ral runs natively on Windows, MSVC-built: the interactive session (structural
and rustyline frontends), pipelines, redirects, and `&` backgrounding all
work; scripts authored with CRLF line endings parse; the bundled coreutils
subset covers everyday use. Each external command inside a `grant` is
confined under an AppContainer (LowBox token) keyed to its own fs projection
— the fs allow-list is enforced by ACEs stamped for that projection's SID on
the granted prefixes, so a narrowed grant is narrowed at the kernel, and
`net: false` is enforced by withholding the network capability SIDs, so a
denied command cannot open a socket at all (SPEC §11.8). This confinement is
exercised on CI, not merely asserted.

Documented degradations, not bugs:

- No Ctrl-Z / stopped jobs — there is no SIGTSTP analogue; `fg` blocks on
  whole-job completion and `bg` is a no-op.
- No `ral-sh` — it is a POSIX login-shell bridge (`/bin/sh`, `/etc/shells`);
  the concept does not exist on Windows.
- A smaller coreutils subset: `id`, `stat`, `kill`, `test`, and `tac` are
  Unix-only and are not built; `timeout` is a scoped follow-up.

## Around the shell

**exarch.** A small coding agent whose only tool is ral itself: every command
the model emits is evaluated under a capability profile pushed onto the shell's
own stack, so the agent's sandbox is `grant` — in-language, not a wrapper
around a binary. Five profiles ship in the binary, from `dangerous` to
`confined`. See [exarch/README.md](exarch/README.md).

**synod.** A second product over the same engine, an office-work delegate where
exarch is a coding one: you grant it one folder, describe the job in plain
language, and it works in that folder in place, under a checkpoint before, a
report of what changed after, and per-file or whole-job undo. Every conversation
boots a real virtual machine from shipped boot media — one guest over two
backends, Virtualization.framework on macOS and Hyper-V on Windows — and the
engine runs inside it. `just synod-app` bundles it into a runnable `.app` and
`just synod-msi` into a Windows installer, both under `target/release/bundle/`;
each needs the Tauri CLI (`cargo install tauri-cli`) and a guest image under
`vm-image/out/` — `just guest-rootfs` then `just guest-boot`, passing `amd64` for
the Windows guest. On macOS a bare `cargo build` product cannot boot a machine:
the virtualization entitlement arrives with the bundle's signature, or, for a
development binary, from `dev/scripts/sign-virtualization.sh`. On Windows nothing
needs signing and synod stays unprivileged, but Hyper-V must be there to ask —
Windows Pro, Education, or Enterprise with the Virtual Machine Platform feature
installed — and the account must be an administrator or a member of the
computer's local Hyper-V Administrators group, which is empty by default. That
group membership is the one deployment step, and synod checks it and names it
before a folder is granted rather than failing halfway into a session.

**Plugins.** The interactive shell is extended in ral itself:
[plugins/](plugins) has autosuggestions, fzf-backed history, file, and
directory pickers, `**`-trigger fzf completion, and zoxide integration.
[docs/PLUGINS.md](docs/PLUGINS.md) describes the interface.

**Editors.** [editors/](editors) carries a tree-sitter grammar (nvim and other
tree-sitter consumers) and portable syntax definitions for VS Code, Zed,
Sublime Text, bat, and delta.

**Design record.** [docs/ral-wiki](docs/ral-wiki) is the project wiki: design
chapters, dated decision notes, and the invariants the implementation keeps.

## License

Dual MIT / Apache-2.0.
