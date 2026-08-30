---
status: active
---

# A coercion is syntax, or it is not a coercion

**The byte-to-value coercion the checker inserts is two kernel nodes,
`Decode(Capture(M))`, and no part of it is a name.** A translation whose meaning
the ambient session can change is not a translation: the elaborated program
would mean one thing in the type system and another in the shell that runs it.
So the coercion resolves nothing, binds nothing, and reads nothing out of scope.

## What it was

The kernel/surface split made `capture` yield exact bytes and had the checker
compose the lossy reading step after it — the right architecture, wrongly
spelled. `captured_string` built

```text
capture M to __captured . __decode-captured $__captured
```

an ordinary `Bind` onto an ordinary name-dispatched command. Every surface
mechanism the two spellings drag along came with them, and the checker cannot
vouch for any of it. Four consequences, found together on 2026-08-11:

| the surface mechanism inherited | what it cost |
| --- | --- |
| head resolution consults scope, then handler frames, then builtins | `alias __decode-captured { \|_\| return 42 }` made every captured `let` an `Int` the checker had typed `String` — unsoundness by rebinding; `{ \|_\| return "" }` blanked them all |
| `Bind` installs its binder in the current scope | `let __captured = precious` was overwritten and read back as `Bytes`; every top-level byte-routed `let` left a live `$__captured` behind (scope reflection saw it, ~3× peak residency); the surface's PATH-shadow check fired on the synthesized binder, so an executable named `__captured` broke every capture with a baffling error |
| a builtin is reachable with a spread argument list, which static arity cannot see | `__decode-captured ...$[]` indexed `args[0]` and panicked; the run died at the `catch_unwind` boundary with exit 101, not as a catchable ral error |
| a value is looked up out of scope by cloning, and read as bytes by cloning again | two copies of the captured buffer on the shell's hottest path, where the inline decode had been zero-copy |

They are one fact found four times: **a checker-synthesized term written in
surface machinery inherits surface semantics, and the type system stands behind
none of it.** Alias vetting does not close the hole — `pin_arm_to_head` pins an
arm's *route*, never the value type a coercion promises.

## The line

**Whatever the checker writes into a program is syntax; whatever the user writes
is theirs to redefine.** `CompKind::Decode(Arc<Comp>)` is a term former, so its
denotation is fixed where the annotator writes it:

- there is no name, so scope bindings, aliases, and `within [handlers: …]`
  frames have nothing to intercept;
- there is no binder, so nothing leaks into a scope, nothing is observable after
  the `let`, and no surface check for shadowing or PATH collision applies to a
  term the user did not write;
- there is no argument list, so there is no arity to get wrong and no spread to
  slip past it;
- `eval_decode` moves the buffer out of the capture's value and decodes it in
  place, so the coercion copies nothing.

The user's own spelling of the same composite, `M | from-string`, is untouched
and still means whatever their session says `from-string` means. That is the
distinction: they wrote the name, so the name is theirs.

## Why this keeps the kernel/surface split intact

`capture` still yields exact bytes and the checker still *composes* the reading
step, which is the whole content of Phase 2 — everything lossy or partial is a
term the operational semantics reads, not behaviour buried in the capture node.
What changes is only which language the composed step is written in: kernel
syntax rather than a surface call. The paper's claim survives in the stronger
form,

> the operational semantics consults only explicit syntax, and every implicit
> surface behaviour is justified by typed elaboration into terms whose meaning
> no session can change.

The kernel's term formers are now `_to_`, `_؛_`, `_∣_`, `exec`, `capture`,
`decode`, `rec`. `decode : F[Value] Bytes → F[Value] String` is the one partial
former, and it is where the Agda model carries the failure clause.

> **2026-08-30.** `_؛_` is no longer a former: the kernel has one bind,
> `_to_`, at either route, and `M ؛ N` is `M to shift N` (`Core.Derived`).
>
> **Superseded in its facts, not its decision (2026-08-19).** The decision above
> stands: the coercion is syntax, and `Decode(Capture(M))` is what the checker
> writes. What has moved is everything this page says about the *model* below.
> `dev/agda` has since gained the failure layer and the encoder, so the former
> list also carries `halt`, `try`, `guard` and `encode`; the decoder answers
> `Str ⊎ Err` rather than `Maybe Str`, because a decoder says *why* a word does
> not read; and failure is no longer stuckness. `decode-stuck` is deleted, the
> kernel has handlers, and a decode that cannot read raises a signal that climbs
> the stack until a `try` catches it or the run ends at the `abort` leaf. The
> paragraph below records what was true on 2026-08-12 and is kept for the
> record; `dev/agda/README.md` is the current account.

The model carries it as of 2026-08-12. `dev/agda` gained a `String` base type
with `str` as its introduction form, a `decode` former over a *computation* —
the shape of `CompKind::Decode`, so the decomposition is a theorem rather than
a convention — and a decoder `dec : 𝔹* → Maybe Str` as a parameter of the
machine, not of the syntax: a typing rule cannot know whether a decode will
succeed. Failure is **stuckness**, not a state: the kernel has no handlers, so
there is nowhere to send a failure, and `decode-stuck` says the state has no
successor under *any* label — which is what distinguishes it from an `exec`
waiting on the world. `Obs` grew a second valueless leaf, `abort`, distinct
from the truncation leaf `undef`, and `_≐_` grew the two fields that preserve
it; `≉-abort-loop` proves a failing decode is not observationally a hang, which
is what ral's behaviour demands and what makes the leaf more than decoration.

One gap is owned rather than modelled: ral flushes a non-empty capture buffer
to the nearest visible stream when the body of a capture fails, and the frozen
machine reports none of it, so an aborting observation is faithful only up to
that flush. No theorem depends on the difference — every conclusion about
escaping bytes asks for a `Terminal` first — and modelling it needs a drain of
the buffers above the failure at the instant it happens.

## The alternative that was refused

Reserving the name — a blacklist forbidding `__decode-captured` as an alias,
handler, or binding — was rejected. It is not compositional: the meaning of an
elaborated program would still be a function of the ambient environment, merely
one the environment is now forbidden to exercise. Every future coercion would
have to be added to the list, and a user's program would fail for naming
something they have no reason to know about.

## In the code

- `CompKind::Capture` / `CompKind::Decode` ([[map/core/ir|ir]]), composed by
  `annotate.rs`'s one constructor `captured_string`
  ([[map/core/typecheck|typecheck]]); `eval_capture` / `eval_decode` in
  `core/src/evaluator/comp.rs` ([[map/core/evaluator|evaluator]]).
- The `__decode-captured` builtin, its type rule, and its arity hole are gone;
  the reading lives in `eval_decode`, which owns
  `strip_trailing_newline` + `decode_utf8_strict` and returns a catchable error
  on invalid UTF-8, still naming `| from-bytes`.
- `core/tests/capture_coercion.rs` is the regression suite: an alias, a handler
  frame, and a user binding of each old name, and the once-panicking spread
  call.
- `docs/SPEC.md` §17.5 gives the `Capture` and `Decode` rules.
