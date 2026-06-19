---
status: active
---

# Handlers and aliases are lambdas; the calling convention is fixed by surface form

A handler or alias body is *applied*, never merely forced, so its arity is part
of its contract. **Every per-name handler (`within [handlers: [name: …]]`) and
every alias must be a unary lambda `{ |args| … }`, and every catch-all
(`within [handler: …]`) must be a binary lambda `{ |name args| … }`. The calling
convention is chosen by the surface position — per-name/alias versus catch-all —
not inferred from the value's runtime shape, and arity is validated at install
time: a bare block `{ … }`, a non-lambda value, or a lambda of the wrong arity is
rejected with a clear error.**

## Why arity cannot be inferred from the value

A per-name handler is invoked with the command's argument list; a catch-all is
invoked with the name *and* the argument list. Currying ([[design/cbpv|cbpv]],
`docs/SPEC.md` §4.5) makes the two indistinguishable by shape after the fact: a
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
  bare block in handler, catch-all, or alias position is now an install-time
  error. The only valid handler/alias values are unary and binary lambdas.
- **The convention is positional.** The per-name slot demands unary; the
  catch-all slot demands binary; the `alias NAME { |args| … }` builtin and the
  rc/plugin `aliases:` maps (`docs/SPEC.md` §9) demand unary. Dispatch
  ([[internals/handler-dispatch|handler-dispatch]]) invokes the matched entry
  under the convention its install site fixed, never by inspecting the stored
  value.
- **Arity is checked once, at install.** `within`-frame installation,
  `install_alias`, and the rc/plugin alias loaders all reject a malformed value
  before it can reach lookup, so no wrong-arity entry is ever dispatched.

## The hard rule

A per-name handler and every alias is `{ |args| … }`; a catch-all is
`{ |name args| … }`. Anything else — a bare block, a non-lambda, a unary
catch-all, a binary per-name handler — is rejected where it is installed, with a
diagnostic naming the expected arity. The convention is read off the surface
form, never off the value.

See also [[design/effects-handlers|effects-handlers]],
[[internals/handler-dispatch|handler-dispatch]],
[[decisions/260614_structural-bug-prevention|structural-bug-prevention]],
[[decisions/260606_alias-head-defines-its-modes|alias-head-defines-its-modes]];
the surface forms are `docs/SPEC.md` §3.2 (handlers) and §9 (aliases).
