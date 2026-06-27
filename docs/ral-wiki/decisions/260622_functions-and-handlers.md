---
status: implemented (help→explain done 2026-06-27; echo→sugar pending)
---

# Functions and handlers, and the end of "alias"

**ral has the function and the handler, and no third. A *function* has fixed
arity: it is a curried `Fun(A₁, … Fun(Aₙ, B))`, applied positionally. A *handler* takes a single
argument — the argv collected into one `List α` — and is dispatched by name; its
*surface* arity varies, but its *value* arity is one.** Every builtin is a function; no builtin is a handler.
A handler is a user mechanism — a ral lambda installed for a scope by `within` or
persistently by `handle`. With "handler" naming the variable-arity calling
convention, the word "alias" is redundant and is retired everywhere.

## Function and handler

- **Function** — fixed arity. The trampoline consumes one value argument per
  `Lambda` layer and faults on a surplus
  ([`core/src/evaluator/trampoline.rs`](../../../core/src/evaluator/trampoline.rs)),
  so arity is part of the type. A function reifies into a curried lambda.
- **Handler** — one argument. The calling convention forces the argv into a
  single homogeneous list, so a parameterised arm is typed `Fun(List α, B)`
  and every supplied argument inhabits the element type `α`
  ([`apply_handler_arm` / `infer_handler_arm`, `core/src/typecheck/infer.rs`](../../../core/src/typecheck/infer.rs)).
  A catch-all `{ |name args| … }` also receives the dispatched name, typed
  `Fun(String, Fun(List α, B))` — the shape that lets it monitor every call
  (`{ |name args| echo "$name called with ...$args" }`). That is the runtime
  calling convention (`run_handler`,
  [`core/src/runtime/command_call.rs`](../../../core/src/runtime/command_call.rs));
  the typechecker does not yet pin it (see *Known weaknesses*).

Variable arity is therefore a *surface* phenomenon — a handler invoked `g a b c`
sees one three-element list — while at the value level a handler is fixed-arity-one. The
[[invariants/fixed-arity|fixed-arity]] invariant holds with **no exception**:
there is no variadic application anywhere; variable surface arity is always a
list value, exactly as optionality is always a variant.

A handler reaches the dispatch site from one of two places, differing only in
lifetime:

- **scoped** — `within [handlers: …, handler: …]`, popped at scope exit;
- **persistent** — `handle NAME { |args| … }`, removed by `unhandle NAME`,
  surviving across turns (the form formerly called an alias).

No builtin is a handler — the core ships only functions, and the argv-as-list
convention is reached only through `within` and `handle`.

## Every builtin is a function

No builtin is a handler. The audit that "migrate the variadic builtins to
handlers" once implied resolves the other way: the only two variadic entries,
`echo` and `help`, leave the registry as sugar or functions, and nothing
argv-as-list remains. The current `arity: _` entries were never one shape:

| entry | today | real shape | becomes |
|---|---|---|---|
| `echo` | `Sig(BYTES_VARIADIC)` | genuinely variable, **heterogeneous** | not a builtin — surface sugar over the `to-line` codec (below) |
| `help` | `Sig(HELP)` | 0-or-1, extras ignored today | split into `help` (arity 0, overview) and `explain NAME` (arity 1, lookup) — two **functions** (below) |
| `clear`, `reset` | `Sig(BYTES_VARIADIC)` | **nullary** | **function**, arity 0 |
| `from-*` codecs | `Sig(… Optional)` | optional / stdin-driven | **function** — optionality is a [[design/codecs|codecs]] concern, not arity |

Because every builtin is a function, no `Function | Handler` kind is recorded on
the registry at all: the distinction lives entirely in the handler stack
(`within` / `handle`), never in `CORE_BUILTINS`. The `Scheme`-versus-`Sig` split a
type rule already carries
([`core/src/typecheck/builtins.rs`](../../../core/src/typecheck/builtins.rs)) is a
representational choice in the checker, orthogonal to this and untouched — `range`
and the codecs are `Sig` functions, exactly as before.

## `help` splits into two functions

`help` only ever read `args[0]`
([`builtin_help`, `core/src/builtins/misc.rs`](../../../core/src/builtins/misc.rs)):
no argument listed everything, one argument looked a name up (exact, then a glob
fallback), and any further arguments were silently dropped. It was never the
variadic list its `Sig(HELP)` type advertised — only 0-or-1. But 0-or-1 under a
single name *is* variable arity, and variable arity under a name is the handler
convention; one function cannot be both nullary and unary. So `help` splits along
the seam its own body already had:

- **`help`** — arity 0 — the overview (Builtins, Prelude, Library), closing by
  naming `explain` so the second command is discoverable;
- **`explain NAME`** — arity 1, `Fun(String, F Unit)` — the single-name entry:
  doc, type, and source, with the glob fallback when no name matches exactly.

Both are ordinary functions. `help` stays the instinctive entry point and teaches
the second name; `explain map` reads as well as `help map` did. With this, nothing
argv-as-list is left in `CORE_BUILTINS` — `help` was the last builtin tied to the
handler convention.

## `echo` is sugar, not a primitive

`echo` looked like the hard case: it prints arguments of *different* types, and
ral has no top type to hold them — `Ty`
([`core/src/typecheck/ty.rs`](../../../core/src/typecheck/ty.rs)) has no ⊤, by
design, because presence and variation are data, never a hole in a type
([[invariants/optionality-via-variants|optionality-via-variants]]). But ral
already has the pieces that compose into exactly what `echo` does, so it needs no
primitive of its own:

- **`str`** is `∀α. α → String` (value form `any_to_string`,
  [`core/src/typecheck/builtins.rs`](../../../core/src/typecheck/builtins.rs)): it
  renders *one* value of any type, instantiated freshly at each call, so every
  argument is typed independently and nothing forces unlike arguments to share a
  type.
- **`intercalate " "`** joins a `List String` into one `String`, and **`to-line`**
  writes that string to the byte output
  ([`core/src/builtins/codecs.rs`](../../../core/src/builtins/codecs.rs)) — the
  value→bytes-on-stdout crossing the codecs own by design ([[design/codecs|codecs]]).

So `echo` is neither a function nor a handler: it is **surface sugar**, lowered in
the elaborator by rendering each argument *before* any list is formed. Each atom
of the surface arg list becomes a `String` — a plain word `w` as `str w`, a spread
`...xs` as `map str xs` — spliced into one `List String`, joined by `intercalate
" "`, and written by `to-line`. So `echo a ...$xs b` lowers to
`to-line (intercalate " " [str a, ...(map str $xs), str b])`. The spreadless case
is just `echo a b` ≈ `to-line "$a $b"` — the interpolation reading is a mnemonic,
not the mechanism.

This *dissolves* the exception instead of quarantining it:

- **No top type, no bespoke type rule.** `str 1` and `str "x"` are each `String`
  before either reaches the list, so nothing ever holds mixed types. "A handler is
  `Fun(List α, R)`" keeps its clean statement, asterisk-free.
- **Spreads need no special pleading.** `...xs` is a real `List α` value —
  homogeneous by construction — so `map str xs : List String` always typechecks.
  The danger the handler form courts, forcing *unlike literals* into one `List α`,
  never arises: a spread is not the argv.
- **Render-then-collect is why it must be sugar, not a prelude function.** Written
  as `echo = { |args| to-line (intercalate " " (map str args)) }`, `args` is the
  argv already packed into one `List α`, so `echo 1 "x"` breaks again — that is
  collect-then-render. Sugar renders each atom first and collects only then, which
  is also *why* `echo` never fit as a handler: a handler receives the argv as one
  list, but `echo` must render before the list exists.
- **A consistency win, not only a removal.** `echo` today pretty-prints maps via
  its own `format_args` ([`core/src/builtins/misc.rs`](../../../core/src/builtins/misc.rs)),
  diverging from the codec rendering; routed through `str`/`to-line` it renders
  identically to `to-string` and `"$x"`. One renderer, used everywhere.

Cost: `$echo` has no value form (sugar does not reify); reify `to-line`, or bind a
prelude `echo` if the name is wanted as a value. With `echo` sugared and `help`
split, nothing in `CORE_BUILTINS` is argv-as-list — the core ships only functions.

## Retiring "alias"

An alias was only ever the *persistent* install form of a ral handler — the same
`HandlerEntry`, distinguished from a `within` frame by lifetime alone
(`removable_by_unalias` on the frame; `install_alias`,
[`core/src/types/shell/scope.rs`](../../../core/src/types/shell/scope.rs)). Once
"handler" is the single name for variable arity, "alias" carries no
distinct meaning and is removed:

- **Surface.** `alias NAME { |args| … }` → `handle NAME { |args| … }`;
  `unalias NAME` → `unhandle NAME`; the rc / plugin `aliases:` map key →
  `handlers:` ([`ral/src/repl/config.rs`](../../../ral/src/repl/config.rs),
  [`ral/src/repl/plugin/`](../../../ral/src/repl/plugin/)).
- **Internals** (mechanical): `install_alias` → `install_handler`,
  `remove_alias` → `remove_handler`, `push_alias` → `push_handler`,
  `removable_by_unalias` → `removable_by_unhandle`; `apply_alias_arm` →
  `apply_handler_arm`, `infer_alias_arm` → `infer_handler_arm`,
  `alias_arm_scheme` → `handler_arm_scheme`, `alias_arm_body` →
  `handler_arm_body`; `alias_statement_shape` / `unalias_statement_shape` →
  `handle_statement_shape` / `unhandle_statement_shape`, with the AST, parser,
  and elaborator nodes ([`core/src/syntax/`](../../../core/src/syntax/),
  [`core/src/elaborator.rs`](../../../core/src/elaborator.rs)); the `Alias` /
  `Unalias` registry entries → `Handle` / `Unhandle`.
- **Docs.** `docs/SPEC.md` §9 and its §3.2 cross-references, `docs/PLUGINS.md`,
  `docs/TUTORIAL.md`, `docs/RATIONALE.md`, `docs/rc.example`, and example scripts
  (`kit/tasks.ral`).

This decision supersedes the *vocabulary* of
[[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]:
its content stands unchanged — every handler is still a lambda whose arity is
read off the surface form — but "alias" reads "persistent handler" throughout,
and its title follows.

Out of scope: `path_aliases`
([`core/src/path/lex.rs`](../../../core/src/path/lex.rs)) is filesystem path
aliasing, an unrelated sense of the word, untouched; the `exarch/` harness embeds
ral and names the surface keyword in its data files — a separate tree, flagged
but not changed here.

## The hard rule

There are functions and there are handlers, and nothing else. A function has
fixed arity *n* and is applied positionally. A handler takes one argument — the argv as `List α` (a
catch-all also takes the name) — is dispatched by name, and is installed by
`within` (scoped) or `handle` (persistent). No builtin is a handler: the core
ships only functions, and the two builtins that looked variadic resolve away —
`echo` is surface sugar lowering to `to-line`, and `help` splits into the nullary
`help` and the unary `explain`. There is no "alias," no variadic application, and
no exception.

## Known weaknesses and open choices

- **`echo`-as-sugar drops `$echo`.** Sugar does not reify, so the value form is
  gone; reify `to-line`, or add a prelude `echo` binding if the name is wanted as
  a value.
- **Splitting `help` changes its surface.** `help NAME` no longer works: the
  per-name lookup is now `explain NAME`, and bare `help` is the overview. Scripts
  and muscle memory calling `help foo` move to `explain foo`; `help`'s overview
  text names `explain`, so the path is discoverable.
- **The catch-all's type is unenforced.** Its `Fun(String, Fun(List α, B))` shape
  is the runtime calling convention, but `infer_within`
  ([`core/src/typecheck/scope.rs`](../../../core/src/typecheck/scope.rs)) infers
  the catch-all body only for its internal errors and leaves both parameters
  free — so misusing `name` (anything but `String`) goes uncaught until runtime.
  Fix to do: pin it to `Fun(String, Fun(List α, B))`, exactly as `infer_try` pins
  its failure handler to `Fun(error-record, result)`.

## What this unlocks — and what it doesn't

The change pays for itself and clears a little ground past its own edits, but it
is not a keystone that topples a row of dominoes. Recorded honestly so the next
reader does not over-credit it.

**Follow-on deletions it enables:**

- **`ArgSig::Variadic` and its arg-checking arm** (the `Variadic` branch of the
  arg-policy match in [`core/src/typecheck/infer.rs`](../../../core/src/typecheck/infer.rs))
  lose their only users once `echo` is sugar and `help` is split: `BYTES_VARIADIC`
  and `HELP` are the sole `ArgSig::Variadic` entries, and nothing in host crates or
  plugins uses the variant. The whole policy arm becomes dead and can go.
- **`echo`'s `format_args`**
  ([`core/src/builtins/misc.rs`](../../../core/src/builtins/misc.rs)) is private and
  called only by `builtin_echo`, so it dies with `echo`.

**The real gain is an invariant, not a deletion.** With no variadic builtin,
[[invariants/fixed-arity|fixed-arity]] is exception-free: variable surface arity is
*always* a user handler (a list value), never a builtin. One concrete consequence
to chase: command-position spreads (`...$xs`) then matter only for handler
dispatch, so the argv-collection path may be narrowable to handler calls — to
confirm, not assume.

**What it does *not* unlock — so no one banks on it:**

- **Not one renderer everywhere.** Dropping `format_args` unifies `echo`'s *output*
  on `str` / `to-line`, but `pretty_print` persists as the *display* renderer (it is
  `pub use`-exported and recursive — the REPL's) and is exactly what `echo` diverged
  toward. Reconciling display with `str` / `to-line` is a separate question with a
  real tension (multi-line pretty maps for humans vs flat for pipes); this decision
  only opens it.
- **Not uniform reification.** Removing two variadic entries barely dents the
  command-only set (~20 `value: None` sigs — codecs, `to-*`, terminal-control,
  `chdir`), which is command-only for reasons unrelated to variadicity. The
  first-class/second-class split stays.
- **Not a `Scheme`/`Sig` merge.** The split is orthogonal to function-vs-handler
  (`range` is a `Sig` function) and is untouched.
- **Not a smaller handler stack.** `run_handler`, `within` / `handle`, and
  `HandlerArity` are *user*-handler machinery; there was never a
  native-handler-in-stack path to remove. "No builtin is a handler" is conceptual
  clarity here, not code reduction.

Cite: [[design/builtins|builtins]], [[design/name-resolution|name-resolution]],
[[invariants/fixed-arity|fixed-arity]],
[[internals/handler-dispatch|handler-dispatch]],
[[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]];
`docs/SPEC.md` §3.2, §9, §16, §17.
