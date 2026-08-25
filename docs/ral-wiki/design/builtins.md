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
  `warn`, `ask`). The query case is *a syscall bridge, not text parsing*: records and
  lists in place of a shell-out to `stat` / `ls` / `dirname` and a re-parse, so
  the bytes→text→structured round-trip never arises.
- **It is a base computation** — an operation the prelude has no smaller pieces to
  build from: a regex engine, the string transforms, structural comparison
  dispatched on the runtime value, scalar coercion.
- **Its type cannot be given to an ordinary binding** — the codec routes
  `F[Value] A` / `A → F[Bytes] Unit` ([[design/codecs|codecs]]) and `fail`'s
  divergent, open-row result.

Filesystem *effects* are deliberately none of these: there is no `copy-file` or
`make-dir`, because `cp` / `mv` / `rm` / `mkdir` already own that and a second
spelling would be a second thing to keep capability-checked
([[design/name-resolution|name-resolution]]).

## The families

The core entries group by what they compute:

- **List & higher-order** — `each` `map` `filter` `fold` `sort-list` `sort-list-by`
  `range`. Each takes a thunk, and the callback's [[design/types|payload route]]
  is *universally quantified*: `map { echo $x }` typechecks because the callback
  may itself be byte-routed while the list operation still returns a value.
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
- **Byte writes** — `echo`: every argument rendered through the total
  `to-string`, single-space intercalation, a trailing newline, typed
  `List String -> Return(Bytes, Unit)` so a value boundary reads the bytes it
  wrote. Mixed argument types coexist because the argv boundary renders each
  element before the list is formed.
- **Diagnostics** — `warn`: one `String` and a newline to standard error,
  typed `String -> F[Value] Unit`. Deliberately *not* a byte write — the route
  stays `Value`, so a caller binding the computation's payload never picks the
  message up. ral has no redirect pointing standard output at standard error,
  and this verb is what stands where the bash idiom did
  ([[decisions/260819_diagnostics-are-a-builtin|diagnostics-are-a-builtin]]).
- **Session & terminal** — `cd` `cwd` `alias` / `unalias` `source` / `use`
  `exit` / `quit` `ask` `clear` `reset` `surface` `help` / `explain`, with the
  underscore probe `_ansi-ok`.

`fail` sits outside these: it diverges rather than computing, and its role in
fallback chains is [[design/failure|failure]].

## How a builtin is typed

**A builtin is a function, and its type rule is a scheme.** There is one rule,
`fn(&mut Unifier) -> Scheme`: an ordinary first-class polytype, allocated fresh
per call, usable in command position *and* reifiable as a value (`$map`). A
table entry has an arity and a value form, both by construction
([[invariants/fixed-arity|fixed-arity]]).

Nullary and divergent are shapes a scheme writes, not a second rule:

- **Nullary** — `clear`, `reset`, `help`, and the `from-X` codecs, which read
  the byte channel — is a body with no arrow over it.
- **Divergent** — `fail` and `exit` / `quit`, whose escape unwinds past every
  binding — quantifies a fresh value *and* route directly, so
  `if $c { exit 1 } else { return "x" }` takes its type from the arm that
  returns. `fail` also carries the nonzero-status diagnostic
  ([[design/failure|failure]]), which is a facet of its registry row rather
  than of its type.
- **Byte-routed** — every encoder returns `Return(Bytes, Unit)`: those bytes
  belong to whoever consumes the command as a value.

Two schemes close a computation variable, which a written quantifier list
cannot bind, so they generalise against the empty environment instead:
`from-lines`, whose stream type recurses through its own tail, and `alias`,
whose block argument is a thunk over an unconstrained computation
([[invariants/schemes-leave-closed|schemes-leave-closed]]).

**A builtin's argument diagnostics need no vocabulary of their own.** What a
mismatched argument earns follows from the type that was expected, not from a
tag naming who expected it: a `Ty::Thunk` parameter *is* the block case, a row
carrying `status: Int` *is* the error-record case, and a list meeting `Bytes`
earns the codec hint wherever it happens. So the help reaches a user lambda's
argument as readily as a builtin's, and one application path serves both
([[internals/type-inference|type-inference]]).

`echo` and `detach` are not table entries. They are the two rows of the
*base-frame manifest*, typed `List String -> Return(Bytes, Unit)` and
`List String -> F Any` — the argv convention a handler and an external already
share, `List String` inside and bytes at the OS call — and their schemes are
seeded into the checker's env at boot, so a base frame is looked up as a handler
is ([[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]]).

Each codec being its own entry rather than one polymorphic `decode` / `encode`
is what lets `from-json < file` dispatch straight through the command arm with
the *concrete* return type in view, and a misspelled codec fail at command
lookup rather than as a runtime "unknown codec" string.

## A name is a value or it is handled

**The set is not a third kind of name: the manifest is authored as two, one half
for each of ral's two existing mechanisms**
([[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]],
[[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]]).

- **A table entry → a *native* value.** It declares its arguments, so it has a
  curried function type (arity 0: a thunk type) and is a first-class value bound
  in the base scope. `$upper` *is* the entry, not a lambda around a
  name-dispatched command: it curries by collecting arguments until the entry's
  arity is reached, prints as `<native NAME>`, is equal by name plus collected
  arguments, and crosses the scope envelope by name, re-linked against the
  receiving shell's manifest. Its type is the η-equivalent lambda's, uncurried
  all the way, so partial application in a typed position goes through a `let`
  rethunk rather than a provenance-sensitive rule.
- **A base-frame row → a *base frame*.** It is variadic over a list of strings,
  so there is no arity, nothing to curry, and no meaning for partial
  application: it is only interpretable as command syntax and lives at the
  bottom of the handler stack; `echo` and `detach` are the two. A user frame
  stacks above it and forwards into it
  ([[internals/handler-dispatch|handler-dispatch]]).

Interception is therefore lexical shadowing rather than admission: a binding
under a native's name shadows it, a handler under any name installs, and
`^name` — which skips the env — reaches the handler
([[design/name-resolution|resolution]]).

See also [[design/syscalls-are-effects|syscalls-are-effects]] (builtins are the pure fragment — not every kernel call is an effect),
[[design/name-resolution|name-resolution]], [[design/codecs|codecs]],
[[design/failure|failure]], [[design/pipelines|pipelines]];
[[internals/builtins-registry|builtins-registry]], [[map/core/builtins|map: builtins]],
[[map/core/typecheck|map: typecheck]].
Cite: RATIONALE §"Structured values cross once",
§"The grammar is the residue"; `docs/SPEC.md` §14, §16.7.
