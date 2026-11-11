---
status: active
---

# Handlers are lambdas; the calling convention is fixed by surface form

A handler body is *applied*, never merely forced, so its arity is part
of its contract. **Every per-name handler, scoped (`within [handlers: [name: …]]`)
or persistent, must be a unary lambda `{ |args| … }`, and every catch-all
(`within [handler: …]`) must be a binary lambda `{ |name args| … }`. The calling
convention is chosen by the surface position — per-name versus catch-all —
not inferred from the value's runtime shape, and arity is validated at install
time: a bare block `{ … }`, a non-lambda value, or a lambda of the wrong arity is
rejected with a clear error.**

> *Amended 2026-10-23*: arity is the arm's type, not its spelling. The hard
> rule now reads: every per-name handler takes one parameter,
> the argv; a catch-all takes two, the name and the argv; anything else — a
> value that is no block, or a block whose type takes other parameters — is
> refused where it is written and again where it is installed. A bare block
> is refused when its type takes no parameter, as `{ echo hi }`'s does, not
> because it is bare: `{ forward 'git' }`, whose body is a function of the
> argv, is an arm. The convention stays positional; what changed is the
> evidence (`Inferencer::arm`), since a block's shape cannot say what a
> computed or polymorphic block takes (`let w = wrap $h`).

## Why arity cannot be inferred from the value

A per-name handler is invoked with the command's argument list; a catch-all is
invoked with the name *and* the argument list. Currying ([[design/cbpv|cbpv]],
`docs/SPEC.md` §6.5) makes the two indistinguishable by shape after the fact: a
binary lambda applied to a single argument does not error — it returns the inner
unary closure. So a binary lambda installed where a unary handler is expected
would not fault; it would hand the body a **partial-application closure** as the
intercepted command's result — a silent wrong answer, exactly the
[[decisions/260614_structural-bug-prevention|structural-bug-prevention]] failure
mode where a category error survives as a plausible-looking value. Pinning the
convention to the surface form and checking arity at install time makes that bad
state unconstructable rather than a runtime surprise.

## What changes

- **`Nullary` is removed.** A bare block `{ … }` previously installed as a
  zero-argument handler, treated as nullary at dispatch. That form is gone: a
  bare block in per-name or catch-all position became an install-time
  error. The accepted values were unary and binary lambdas; the amendment
  above admits computed arms by type.
- **The convention is positional.** The per-name slot demands unary; the
  catch-all slot demands binary; persistent handler installation and the
  rc/plugin `handlers:` maps (`docs/SPEC.md` §15.3) demand unary. Dispatch
  ([[internals/handler-dispatch|handler-dispatch]]) invokes the matched entry
  under the convention its install site fixed, never by inspecting the stored
  value.
- **Arity is checked once, at install.** `within`-frame installation,
  `install_handler`, and the rc/plugin handler loaders all reject a malformed value
  before it can reach lookup, so no wrong-arity entry is ever dispatched.

## The hard rule

A per-name handler takes the argv; a catch-all takes the name and argv.
The arm's type must implement that convention, whether written as a lambda
or computed. A wrong-arity arm is rejected where it is written and again
where it is installed, with a diagnostic naming the expected arity.

See also [[design/effects-handlers|effects-handlers]],
[[internals/handler-dispatch|handler-dispatch]],
[[decisions/260614_structural-bug-prevention|structural-bug-prevention]],
[[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]];
the surface forms are `docs/SPEC.md` §9.4 (handlers) and §15.3 (persistent handlers).
