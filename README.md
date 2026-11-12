<p align="center"><img src="site/assets/logo.svg" alt="ral" width="240"></p>

# ral

A shell grounded on algebraic effects.

Running an external command is performing an effect: `git` is an *operation*,
and the OS is merely its default *interpretation*. Separate the two and you get
typed commands, scoped authority, auditing, and a sandbox from one principle.

And data stays data. Captured output is never re-lexed, word-split, or
glob-expanded, so the whole class of quoting bugs is simply gone.

## A taste

```
let file = 'my report.txt'
let nlines = wc -l < $file       # capture stdout, newline stripped
rm $file                         # exactly one argument, always

curl $primary ? curl $fallback   # ? reacts to failure; | moves data

glob '*.rs' | map { |f| wc -l $f }         # typed pipelines,
let cfg = curl -s $url | from-json         # checked before anything runs

with-temp-dir { |dir|                     # removed when the body ends
    curl -fsS -o "$dir/archive" $url
    tar -xf "$dir/archive" -C $dir
}

grant [exec: [allow: [`name git]], fs: [read: [`absolute /src]], net: `offline] {
    git log              # permitted
    curl $url            # denied
}
```

Start with the [tutorial](docs/TUTORIAL.md); [examples/](examples) sets
complete scripts against the bash they replace.

## Design

- **Syscalls are effects.** Authority, audit, and failure all fall out of that.
- **Values are not commands.** Call-by-push-value: `{M}` thunks, `!` forces,
  `$` reads.
- **Handlers reinterpret commands** — `within [handlers: …]` for mocks,
  redirection, instrumentation.
- **Capabilities only shrink.** `grant` intersects; it never amplifies.
- **Authority is dynamic, data is lexical.** No mutation, only shadowing.
- **Most control flow is a library.** `for`, `retry`, and `with-temp-dir` are
  functions over blocks; named dynamic scopes such as `try` and `guard` need
  machine frames.
- **`if` takes a `Bool`**, never an exit status. There is no `||`.
- **Inferred types**, row-typed records, errors before any process starts.
- **Not POSIX**, on purpose: no word splitting, no `$IFS`, no unquoted globs.

The [rationale](docs/RATIONALE.md) and [spec](docs/SPEC.md) say it at length;
both live at <https://lambdabetaeta.github.io/ral/>.

## Install

```sh
curl -fsSL https://lambdabetaeta.github.io/ral/scripts/install.sh | sh
```

This installs the latest release. To select a tag, replace `sh` with
`sh -s ral v0.10.0`.

```sh
brew tap lambdabetaeta/ral https://github.com/lambdabetaeta/ral
brew install ral exarch
```

For a manual macOS/Linux install, download the matching `.tar.gz` from the
[latest release](https://github.com/lambdabetaeta/ral/releases/latest) and extract
it into a dedicated directory, for example:

```sh
tar -xzf ral-linux-x86_64.tar.gz
./ral-linux-x86_64
```

The executable keeps the archive's platform name (including `-allutils` for
with-utils builds). Keep `NOTICE`, `LICENSE-APACHE`, and `LICENSE-MIT` with it;
if installing the executable elsewhere as `ral`, retain those files too.

On Windows, run `ral-windows.msi` from the same release; the installer retains
these legal files. From source: `cargo install --path ral`.

## Usage

```sh
ral                       # interactive
ral script.ral a b        # $args == [a, b]
ral -c 'echo hello'
ral --check script.ral    # type-check only
ral --audit script.ral    # audit report as JSON on stderr
```

`ral-sh` is ral by another name, for `chsh`: interactive sessions get ral,
POSIX command strings and scripts go to `/bin/sh`, so scp and rsync never notice.

For interactive configuration, copy [docs/rc.example](docs/rc.example) to
`~/.config/ral/rc`. The rc writes all thirteen top-level keys and every nested
`limits` and `theme` key; `` `inherit `` keeps ral's or the system's existing setting,
and `` `set v `` overrides it. See
[SPEC §15.3](docs/SPEC.md#153-startup-files-and-rc-configuration).

## Around the shell

- **[exarch](exarch/README.md)** — a coding agent whose only tool is ral; its
  sandbox is `grant`.
- **synod** — exarch for office work: grant it a folder and it does the job
  there, inside a real VM, then reports what changed.
- **[plugins/](plugins)** — autosuggestions, fzf pickers, zoxide, written in ral;
  [docs/PLUGINS.md](docs/PLUGINS.md) is the design.
- **[editors/](editors)** — tree-sitter grammar and syntax files for most
  editors.
- **[docs/ral-wiki](docs/ral-wiki)** — design chapters and the decision log.

## License

Original workspace code is MIT OR Apache-2.0. Codex-derived portions are
Apache-2.0-only; see [NOTICE](NOTICE). Bundled MIT-licensed dependencies retain
their own terms. Funded by the Advanced Research + Invention Agency (ARIA).
