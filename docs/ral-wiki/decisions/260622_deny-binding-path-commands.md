---
status: active
---

# Deny `let` bindings that shadow a PATH command

**A name reachable on `PATH` resolves to that command in head position, so a
top-level `let` (or recursive definition) may not capture it.** ral keeps the
command namespace you type at the prompt disjoint from the value namespace `let`
introduces: binding `git`, `sort`, `factor`, … to a value would silently shadow
the command, the exact data/command collision the language exists to remove.

## Decision

`eval_bind` and the `slot = None` arm of `eval_letrec`
([`core/src/evaluator/comp.rs`](../../../core/src/evaluator/comp.rs)) call
`check_pattern_shadow` / `check_path_shadow`
([`core/src/evaluator/pattern.rs`](../../../core/src/evaluator/pattern.rs)): if
the bound name resolves through `Shell::locate_command` (the effective `PATH`,
honouring `within [env: PATH]`), the bind is refused before its RHS runs.
Reachability is the pure filesystem question, independent of whether the active
grant would admit running the command.

## Scope, and what it is not

- **Top-level only.** The guard fires only at the persisted session scope
  (`Env::at_session_scope`). A `let` inside a block or lambda body, and the
  prelude, are exempt — local names like `test` / `dir` stay usable. This is a
  pragmatic carve-out, *not* "values and commands are disjoint" applied
  uniformly: a nested `let git` still shadows within its block by design.
- **Parameters are never checked.** Lambda parameters bind through
  `assign_pattern` in the trampoline — a different call path the guard does not
  touch. Separation is by site, not by depth.
- **PATH layer only.** Prelude functions, builtins, and coreutils remain
  shadowable; only the outermost, least-owned namespace layer is protected,
  where collisions are accidental and silent.

## Known weaknesses

- **Runtime, not compile-time.** The shadowing *decision* is lexical (the
  elaborator's `is_bound`); the *denial* is dynamic, because PATH probing is I/O
  and the elaborator is pure. Two phases reason about one name.
- **`at_session_scope` is a depth proxy** (`scopes.len() == 2`). It encodes the
  persisted frame positionally and the compiler cannot defend it; a real session
  marker would be sturdier.
- **Validity is machine- and time-dependent.** Installing a command later does
  not retroactively invalidate an existing binding, and the same source may bind
  or refuse on different hosts.

Cite: [[design/name-resolution|name-resolution]] (the layer a head name
resolves to), [[design/cbpv|cbpv]] (values vs commands).
