# Builtins: the structured primitive set

**A builtin is a Rust atom the language keeps in-process precisely because the
value language cannot reduce it to anything simpler — it reaches a *syscall* or
the *shell's own state*, performs a *base computation* the prelude has no smaller
pieces for, or carries a *type no ordinary binding could be given*.** The prelude
is written over the builtins, so anything expressible as a value-function is a
prelude binding, not a builtin ([[design/name-resolution|where a capability
lives]]). The set is small and in-process; it computes structured values or
touches the shell's own state, never the filesystem effects the bundled
[[invariants/single-binary|coreutils]] own.

## Why a capability earns a Rust body

The prelude bottoms out in the builtins, so a capability is one exactly when it
cannot be written as a value-function over simpler ones. Three kinds of
irreducibility:

- **It reaches outside the value language** — a syscall or structured OS query
  (the filesystem family), or the shell's own runtime state a spawned process
  could never touch (`cd`, the `spawn` family, `source` / `use`, `surface`,
  `ask`). The query case is *a syscall bridge, not text parsing*: records and
  lists in place of a shell-out to `stat` / `ls` / `dirname` and a re-parse, so
  the bytes→text→structured round-trip never arises.
- **It is a base computation** — an operation the prelude has no smaller pieces to
  build from: a regex engine, the string transforms, structural comparison
  dispatched on the runtime value, scalar coercion.
- **Its type cannot be given to an ordinary binding** — the codec byte-modes
  `F[Bytes, ∅]` / `F[∅, Bytes]` ([[design/codecs|codecs]]) and `fail`'s divergent,
  open-row result.

Filesystem *effects* are deliberately none of these: there is no `copy-file` or
`make-dir`, because `cp` / `mv` / `rm` / `mkdir` already own that and a second
spelling would be a second thing to keep capability-checked
([[design/name-resolution|name-resolution]]).

## The families

The core entries group by what they compute:

- **List & higher-order** — `each` `map` `filter` `fold` `sort-list` `sort-list-by`
  `range`. Each takes a thunk, and the callback's pipeline modes are *universally
  quantified*: `map { echo $x }` typechecks because the callback may itself write
  bytes while the list operation still returns a value
  ([[decisions/260601_modes-equality-constrained-shared|equality-constrained modes]]).
- **String & regex** — `upper` `lower` `dedent` `slice` `intercalate`
  `re-match` `re-split` `re-find-match` `re-find-matches` `re-replace`
  `re-replace-all` `string-replace` `shell-quote` `shell-split`.
- **Parsing** — `int` `float` `str`: value→scalar coercions.
- **Structure & comparison** — `length` `is-empty` `keys` `has` `equal` `lt` `gt`:
  ad-hoc-polymorphic, dispatched on the runtime value's shape.
- **Structured filesystem queries** — `list-dir` `file-info`
  `is-file` / `is-dir` / `is-link` / `is-readable` / `is-writable` `exists` `glob`
  `resolve-path` `absolute-path` `temp-dir` / `temp-file`.
- **Codecs** — the `from-X` / `to-X` pairs and the streaming `fold-lines`: the
  typed byte↔value crossing, given its own page ([[design/codecs|codecs]]).
- **Concurrency** — `spawn` `watch` `await` `poll` `race` `cancel`: a worker
  scheduler and the `Handle α` values only the host runtime can mint. `poll` is the
  non-blocking probe of a handle — total over a finished block, reporting completion
  or failure as one settle variant rather than blocking or re-raising
  ([[decisions/260615_poll-total-failed-arm|the settle decision]]).
- **Session & terminal** — `cd` `cwd` `alias` / `unalias` `source` / `use`
  `exit` / `quit` `ask` `clear` `reset` `surface` `help` / `explain`, with the
  underscore probes `_type` / `_ansi-ok`.

`fail` sits outside these: it diverges rather than computing, and its role in
fallback chains is [[design/failure|failure]].

## Two ways a builtin is typed

The registry's [[internals/builtins-registry|six-facet entry]] carries a typing
rule whose shape follows from how command-like the builtin is:

- **`Scheme`** — an ordinary first-class polytype, allocated fresh per call. The
  default: a curried function usable in command position *and* reifiable as a
  value (`$map`). A builtin is a `Scheme` whenever its surface is an ordinary
  curried function. `fold-lines` is a `Scheme` like any other; its
  byte-boundary modes — bytes in, output following the callback — are invisible
  to a structural projection of the type, so its scheme constructor writes them
  in via `reducer_spec` ([[design/codecs|codecs]]).
- **`Sig`** — a *command signature*: argv shape and result computation read
  directly, without falling through to command-name classification. This is for
  builtins whose surface is not a curried value — *nullary* (`clear`, `reset`,
  `help`), *optional-argument* (`cd`, and the `from-X` codecs, which read stdin
  when given none),
  *divergent* (`fail`, result `Never`, carrying the nonzero-status diagnostic —
  [[design/failure|failure]]), or carrying a compile-time *probe* (`_type`). A
  `Sig` still exposes a first-class form when it declares a value scheme
  (`$range`, `$int`).

Each codec being its own `Sig` rather than one polymorphic `decode` / `encode` is
what lets `from-json < file` dispatch straight through the command arm with the
*concrete* return type in view, and a misspelled codec fail at command lookup
rather than as a runtime "unknown codec" string.

## Reification routes through the name

A builtin used in value position (`$upper`) is η-expanded into a curried lambda
whose body is a *name-dispatched* command, not a direct call into the Rust body.
So a reified primitive is an ordinary closure that composes like a user function,
and a later `within [handlers: …]` frame or `alias` still intercepts it, because
dispatch happens by name at application time ([[design/name-resolution|resolution]]).
Fixed arity ([[invariants/fixed-arity|fixed-arity]]) is what makes the
η-expansion well-defined; a command-only entry, whose argv shape is variable
(`cd`, the `from-X` codecs), has no first-class form.

See also [[design/syscalls-are-effects|syscalls-are-effects]] (builtins are the pure fragment — not every kernel call is an effect),
[[design/name-resolution|name-resolution]], [[design/codecs|codecs]],
[[design/failure|failure]], [[design/pipelines|pipelines]];
[[internals/builtins-registry|builtins-registry]], [[map/core/builtins|map: builtins]],
[[map/core/typecheck|map: typecheck]].
Cite: RATIONALE §"Structured values cross once",
§"The grammar is the residue"; `docs/SPEC.md` §16, §17, §21.
