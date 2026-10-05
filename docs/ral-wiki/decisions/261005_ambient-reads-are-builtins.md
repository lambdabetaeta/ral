---
status: accepted
generated_at_commit: f52a58a9
verified_at_commit: f52a58a9
anchors: [ambient, home_dir, CompKind::Tilde, variable_val, pure_int, pure_strs, pure_string_map]
---

# Ambient reads are builtins

**The shell's ambient state is read by six nullary builtins — `cwd`, `env`,
`args`, `user`, `home`, `nproc` — and by nothing else. `$CWD`, `$ENV`,
`$ARGS`, `$USER`, `$HOME` and `$NPROC` are gone, so `$NAME` once more means
one thing: a lexical variable. `$SCRIPT` stays, a compile-time literal of the
file and the one reserved name. `~` abbreviates `home`.**

## Context

`$CWD` and its five siblings were spelled as variables and were not. The
elaborator hoisted each into `Observe(Register)`, a computation reading the
dynamic `Context`, typed and total, exactly as it hoists `!{…}`: the right
semantics — a store read is an effect, and `within [env: …]` was honoured —
under the wrong syntax. `$` meant "look up in ρ" except for six capitalised
strings told apart by a table (`reserved_register`); `$env` was an undefined
variable while `$ENV` was a map, and the wiki wrote `$env` in five places;
`let CWD = …` was refused. The set had no admission rule — one store cell,
one dynamic map and two of its projections, one program constant, and
`$NPROC`, a machine fact that is not in `Context` at all — and it churned:
`$STATUS` admitted then cut, `$HOME` removed then readmitted, `$1` promised in
SPEC §13 and never built. And `cwd` was already a builtin whose body was
`Register::Cwd`'s, verbatim.

## What was decided

- **Six natives, one each.** `cwd :: F String`, `env :: F (Map String)`,
  `args :: F [String]`, `user :: F String`, `home :: F String`,
  `nproc :: F Int`. Each is a computation: `let [a, b] = args` binds what it
  returns; `!{env}[HOME]` and `"!{cwd}/x"` splice it. Their bodies are the
  former `observe` arms, moved, so the meaning is unchanged: the effective
  environment under any `within [env: …]` overlay, the logical working
  directory, `PWD` and `OLDPWD` absent from `env`.
- **Ordinary names.** They are natives like `cd` and `warn`: a lexical binding
  of the same name shadows one in its scope, and `^env`, `^nproc` run the
  coreutils programs. Shadowing those two is a replacement, not a collision —
  the same name, a typed value instead of a line of text — and what
  `/usr/bin/env` does that ral's `env` does not, `env VAR=x cmd`, is the idiom
  `within [env: …]` exists to retire.
- **`$NAME` is a variable, always.** `reserved_register` and the binding
  refusal are deleted; ALL-CAPS names bind like any other.
- **`$SCRIPT` is the one reserved name.** It is a literal the elaborator bakes
  from the file it compiles — lexical by construction, nothing a builtin can
  be. `let SCRIPT = …` is refused, which it was not before: `$SCRIPT` silently
  ignored the binding.
- **`~` keeps its own IR node.** `CompKind::Tilde(TildePath)` replaces
  `Observe(Register)`: a value-position `~/x` is the home directory with the
  suffix appended. It does not desugar to a call of the *name* `home`, which a
  `let home = …` in scope would capture; a `~/bin/tool` head stays
  `CommandName::TildePath`. One sentence names an unset `HOME`, shared by `~`
  and `home`.
- **No `$1`.** The positional forms do not exist and are not added;
  `let [first, second] = args` is the destructuring.

## Alternatives considered

- **Keep the names and make ALL-CAPS the sort** — a capitalised `$NAME`
  always a store read and never bindable, on Harper's assignables-are-not-
  variables. It keeps `"$CWD/x"` and turns the table into a definition, at the
  price of a case rule and of every ALL-CAPS binder. Declined: the general
  mechanism already present — a nullary native, spliced — answers the same
  need with no rule at all.
- **A shared prefix** (`shell-cwd`, `get-env`, …) to keep clear of `PATH`.
  Declined: it taxes six names to avoid two deliberate replacements; core's
  compounds are noun-verb (`to-json`, `abbreviate-home`), not a family
  prefix; and `^name` already exists for a shadowed program.
- **`environ` and `parallelism`** for the two colliders alone. Declined for
  guessability: the obvious name is the one the model and the human type
  first.
- **Desugar `~` to `home`.** Unhygienic, as above.

## What is deleted

`ir::Register`, `CompKind::Observe`, `evaluator/observe.rs` (its arms become
`builtins/ambient.rs`), `reserved_register` and the reserved-name refusal,
the typechecker's "`$ENV[FOO]`" hint (now "`!{env}[FOO]`"), and the `$1`
sentences in SPEC §13 and on `Context`.

## Where

`core/src/builtins/ambient.rs`, `core/src/builtins.rs`,
`core/src/typecheck/builtins.rs` (`pure_int`, `pure_strs`,
`pure_string_map`), `core/src/ir.rs` (`CompKind::Tilde`),
`core/src/elaborator.rs` (`variable_val`), `core/src/evaluator/machine.rs`,
`core/src/typecheck/infer.rs`, `docs/SPEC.md` §10.1–10.2.

Amends [[decisions/260531_env-is-dynamic-only|env-is-dynamic-only]] (the
environment is read through `env`),
[[decisions/261005_one-tilde-rule|one-tilde-rule]] (`$HOME` is not a
register; `~` abbreviates `home`) and
[[decisions/260826_the-evaluator-steps-closures|the-evaluator-steps-closures]]
(the one store read left in the IR is `~`). See [[design/scoping|scoping]].
