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
  could never touch (`cd`, the `spawn` family, `use`, `surface`,
  `warn`, `ask`). The query case is *a syscall bridge, not text parsing*: records and
  lists in place of a shell-out to `stat` / `ls` / `dirname` and a re-parse, so
  the bytes→text→structured round-trip never arises.
- **It is a base computation** — an operation the prelude has no smaller pieces to
  build from: a regex engine, the string transforms, structural comparison
  dispatched on the runtime value, scalar coercion.
- **Its type cannot be given to an ordinary binding** — a decoder's `Returns A` for an
  `A` the stream decides and an encoder's `A → Command` that writes
  ([[design/codecs|codecs]]), and `fail`'s divergent, open-row result.

Filesystem *effects* are deliberately none of these: there is no `copy-file` or
`make-dir`, because `cp` / `mv` / `rm` / `mkdir` already own that and a second
spelling would be a second thing to keep capability-checked
([[design/name-resolution|name-resolution]]).

**Temporary-path creation is a structured acquisition, not a second removal
interface.** `temp-dir` and `temp-file` return a fresh path only after checking
that `rm`, bundled or on `PATH`, is admitted by the active grants. The path is
the caller's to remove; the check is at creation, not a lifetime promise.
Prefer the prelude's `with-temp-dir` and `with-temp-file`: they use `guard`
and ordinary `rm -rf` / `rm -f`, with workers using the path inside a `gather`
in the body ([[decisions/261111_a-guard-acquires|a-guard-acquires]]).

## The families

The core entries group by what they compute:

- **List & higher-order** — `each` `map` `filter` `fold` `sort-list` `sort-list-by`
  `range`. `each`, `fold` and `map` accept either callback grade: they run
  it, stream its output and keep its value. `map { |x| echo $x } $xs` returns
  unit elements; use `{ |x| return !{echo $x} }` to collect text. `filter`
  demands a returned Bool, and `sort-list-by` a returned comparable key.
- **String & regex** — `upper` `lower` `dedent` `slice` `intercalate`
  `re-match` `re-split` `re-find-match` `re-find-matches` `re-replace`
  `re-replace-all` `string-replace` `ral-quote` `posix-quote` `posix-split`.
- **Parsing** — `int` `float` `str`: value→scalar coercions.
- **Structure & comparison** — `length` `is-empty` `keys` `has` `lt` `gt`:
  ad-hoc-polymorphic, dispatched on the runtime value's shape.
- **Structured filesystem queries** — `list-dir` `file-info`
  `is-file` / `is-dir` / `is-link` / `is-readable` / `is-writable` `exists` `glob`
  `resolve-path` `absolute-path` `abbreviate-home` `temp-dir` / `temp-file`.
- **Codecs** — the `from-X` / `to-X` pairs and the streaming `fold-lines`: the
  typed byte↔value crossing, given its own page ([[design/codecs|codecs]]).
- **Concurrency** — `await` `poll` `is-done` `first-settled` `race` `cancel`
  `workers` and the host's `watch`, beside the `spawn` and `gather` control
  operators (`par`, `both` and `first-of` are prelude code over them): a
  worker scheduler and the graded `Handle^ε α` values only the host runtime can
  mint. `await` behaves as the block run where it is written; `poll` is the
  non-blocking 4-state probe of a handle (`running`, `stopping`, `ready`,
  `consumed`) — total over live handles' lifecycle states, reporting completion
  or failure on `ready` as one outcome variant rather than blocking or
  re-raising. `is-done` is the boolean completion predicate over the same
  states. A handle marshalled to a child arrives withheld; every observation
  fails `withheld-handle`
  ([[decisions/260615_poll-total-failed-arm|the settle decision]],
  [[decisions/261019_await-is-the-block-run-here|await-is-the-block-run-here]]).
- **Writes** — `echo`: every argument rendered through the total `to-string`,
  single-space intercalation, a trailing newline, typed `List String -> Command`:
  its value is its output, like every command. Mixed argument types
  coexist because the argv boundary renders each element before the list is
  formed.
- **Diagnostics** — `warn`: one `String` and a newline to standard error,
  typed `∀ν. String → Command[ν] Unit`, with a fresh unit-result grade;
  binding it fixes the grade to `p` and never captures the message. ral has no redirect pointing
  standard output at standard error, and this verb is what stands where the
  bash idiom did
  ([[decisions/260819_diagnostics-are-a-builtin|diagnostics-are-a-builtin]]).
- **Session & terminal** — `cd` `cwd` `use`
  `exit` / `quit` `ask` `clear` `reset` `surface` `help` / `explain`, with the
  underscore probe `_ansi-ok`.

`fail` sits outside these: it diverges rather than computing, and its role in
fallback chains is [[design/failure|failure]].

## How a builtin is typed

**A value builtin is a function, and its type rule is a scheme.** There is one rule,
`fn(&mut Binders) -> Ty`, closed independently of inference by `Binders::close`.
Its first-class polytype is instantiated fresh per use, in command position
*or* as a value (`$map`). A table entry has an arity and a value form, both by
construction
([[invariants/fixed-arity|fixed-arity]]).

Nullary and divergent are shapes a scheme writes, not a second rule:

- **Nullary** — `clear`, `reset`, `help`, and the `from-X` codecs, which read
  the byte channel — is a body with no arrow over it.
- **Divergent** — `fail`, `raise` and `exit` / `quit` — quantifies a fresh
  result type directly, so `if $c { exit 1 } else { return "x" }` takes its
  type from the arm that returns. `fail` takes text and `raise` takes the
  closed error record ([[design/failure|failure]]); generic shape guidance
  points a caught record passed to `fail` toward `raise`.
- **Writing** — every encoder, `help`, `explain` and the terminal controls are
  `… → Command` rows: the type tells a bind or a pipeline that the command
  writes ([[design/capture|grades and capture]]).

Ordinary builtin results get a fresh grade from `Decl::scheme`; commands,
already graded results and boundary results keep their declared grades.
Boundary results are `p`, since the run supplies data that a command join must
not erase. See [[design/capture|grades and capture]] for the complete rule.

**A builtin's argument diagnostics need no vocabulary of their own.** What a
mismatched argument earns follows from the type that was expected, not from a
tag naming who expected it: a `Ty::Thunk` parameter *is* the block case, a row
carrying `status: Int` *is* the error-record case, and a list meeting `Bytes`
earns the codec hint wherever it happens. So the help reaches a user lambda's
argument as readily as a builtin's, and one application path serves both
([[internals/type-inference|type-inference]]).

`echo` is a row of the *base-frame manifest*, typed `List String -> Command`;
a host may install more rows. This is the argv convention a handler and an
external already share, `List String` inside and bytes at the OS call. The
argv convention carries no type rule: every base frame is `[String] → Command`
by construction, and command-position checking returns `Command` without
reading a row's scheme. Σ distinguishes the in-shell argument gate from the
external gate, which every other command name keeps even when handled.
Every named handler has the same type `U([String] → Command)`, independent of
its name and Σ; the name serves diagnostics only. No handler scheme is persisted
([[design/exec|exec]]).

`detach` is a host-installed native with type
`String → String → [String] → F [pid: Int, desc: String]`: a description, a
program name and one argument list. It curries and has a first-class `$detach`
form. Its program is resolved as `^program` resolves one, bypassing bindings
and the whole handler stack, base frames included. Handlers cannot intercept
`detach` itself; stub it by binding its name:
`let detach = { |desc program args| return [pid: 0, desc: $desc] }`.
Its grant gate remains `ShellVerb::Detach` ([[design/exec|exec]], SPEC §11.5).

No builtin value installs or removes a handler. Persistent handlers are
host-installed between units from rc or manifest `handlers:`;
scoped handlers use `within [handlers: …]`. All three doors take
`Map(U([String] → Command))`, never a record; `handler:` takes
`U(String → [String] → Command)`. Interactive helpers are `let`-bound functions.

Each codec being its own entry rather than one polymorphic `decode` / `encode`
is what lets `from-json < file` dispatch straight through the command arm with
the *concrete* return type in view, and a misspelled codec fail at command
lookup rather than as a runtime "unknown codec" string.

## A name is a value or it is handled

**The set is not a third kind of name: the manifest is authored as two, one half
for each of ral's two existing mechanisms**.

- **A table entry → a builtin value closure `⟨c,∅⟩`.** It declares its
  arguments, so it has a curried function type (arity 0: a thunk type) and
  is first-class in the base signature. `$upper` is the entry itself, not a
  name-dispatched command. `ValClos::builtin` is also public to hosts.
  Force enters the cached η-expansion `λa₀.…λaₙ₋₁.δ(b; a⃗)` over `b ↦ c`;
  δ reads that captured entry, never re-resolving its name. Partial application
  is ordinary β and the remaining λ is boxed at a value demand. A call given
  exactly a builtin's arity invokes its body directly; arity zero runs on
  force too. This keeps residual functions applicable across provenance
  frames ([[decisions/261112_runtime-values-are-value-closures|runtime-values-are-value-closures]]).
  A builtin prints `<native NAME>`; its residual prints
  `<native map, 1 of 2 arguments>` and has type name `Lambda`. The constant
  crosses by name, re-linked against the receiver's builtin table. Its
  residual crosses as that name, an environment row and a supplied-argument
  count. `BuiltinEntry::residual` reconstructs the receiving entry's η suffix
  only when the count is less than its arity and the row binds exactly that
  builtin and its supplied arguments. Zero is a real count (`let g = has`).
  δ is not wire syntax; a builtin, handle or residual beneath a tag is bound
  to a fresh name rather than nested in syntax
  ([[map/core/transport|transport]]).
  Boundary partials are inference-expanded blocks over `Exec`, retaining the
  per-use checked site. Neither builtins nor thunks have language equality.
- **A base-frame row → a *base frame*.** It is variadic over a list of strings,
  so there is no arity, nothing to curry, and no meaning for partial
  application: it is only interpretable as command syntax and lives at the
  bottom of the handler stack; `echo` is the standard row and hosts may add
  others. A user frame stacks above it and forwards into it
  ([[internals/handler-dispatch|handler-dispatch]]).

Interception is therefore lexical shadowing rather than admission: a binding
under a native's name shadows it; a handler under any name installs, but only
intercepts a bare head no binding holds. `^name` — like a path head — skips
both and reaches the external directly
([[design/name-resolution|resolution]]).

See also [[design/exec|exec]] (builtins are the pure fragment — not every kernel call is an effect),
[[design/name-resolution|name-resolution]], [[design/codecs|codecs]],
[[design/failure|failure]], [[design/pipelines|pipelines]];
[[internals/builtins-registry|builtins-registry]], [[map/core/builtins|map: builtins]],
[[map/core/typecheck|map: typecheck]].
Cite: RATIONALE §"Structured values cross once",
§"The grammar is the residue"; `docs/SPEC.md` §14, §16.7.
