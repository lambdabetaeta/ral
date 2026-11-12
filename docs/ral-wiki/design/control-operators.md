# Control flow is library; eight operators are syntax

**ral reserves eight named operators: seven scope a runtime extent; `use`
links a module at check time.** The seven are `within`, `grant`, `try`,
`guard`, `audit`, `spawn`, and `gather`: each needs a machine frame or worker
across an operand's whole dynamic extent. Ordinary Hindley–Milner typing is
no criterion: `try`, `guard` and `gather` have HM types.

- `try` intercepts errors and passes a witnessed record to its handler.
- `guard ACQUIRE BODY RELEASE` shields acquisition, installs release before
  the body's first poll, then releases with the resource and ending. Release
  observes but cannot consume that ending; its own halt pre-empts it
  ([[decisions/261111_a-guard-acquires|a-guard-acquires]]).
- `within`, `grant` and `audit` scope handlers and context, capabilities and
  observation ownership respectively, persisting across tail calls.
- `spawn` runs a block on an owned worker. Its optional title is a syntactic
  operand, not a function's optional argument
  ([[invariants/fixed-arity|fixed-arity]]).
- `gather` owns births over its body's extent: join on return, cancel and
  drain on halt ([[decisions/261020_gather|gather]]).
- `use` links a written path under Σ during checking; its linked body runs
  when evaluated ([[decisions/260606_cacheless-module-loader|module linking]]).

**Functions over thunks remain functions.** `map`, `for`, `retry`, `attempt`
and `with-temp-*` compose the existing operators without needing a frame of
their own. A native attempting to implement a runtime-extent operator must
instead enter `machine::apply`: a nested machine loses tail position and meets
`NESTED_MACHINE_LIMIT`.

Capture `!{…}` and redirection scope `Frame::Capture` and `Frame::Redirect`,
but are not named forms: they belong to command and block notation. `if`,
`case` and `?` are syntax for another reason: their alternatives are syntax,
not a single ordinary value
([[decisions/260811_case-is-syntax-try-is-not|case-is-syntax-try-is-not]]).

Audit ownership is detailed in [[design/audit|audit]]: `audit` hands a trail
back as data, paired with the body's outcome; the other operators are
transparent to its collection.

See [[design/failure|failure]], [[design/scoping|scoping]],
[[design/effects-handlers|effects-handlers]], [[design/grant|grant]].
Cite: RATIONALE §"The grammar is the residue"; `docs/SPEC.md` §3.6, §8.
