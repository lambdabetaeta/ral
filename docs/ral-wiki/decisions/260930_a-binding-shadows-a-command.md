---
status: accepted
generated_at_commit: 8d868e18
supersedes: decisions/260622_deny-binding-path-commands
---

# A binding shadows a command

**`let dd = …` shadows the command `dd` lexically, at every scope, and `^dd`
reaches the binary on `PATH`.** Resolution of a bare head is one lexical
question, bindings first, and no host state enters it.

## What was decided

- **No refusal.** The session-scope check — a `let` or recursive definition
  naming an executable on the effective `PATH` was refused before its
  right-hand side ran — is gone, with its hint. Every binder now behaves as
  nested `let`s and lambda parameters always did.
- **Nothing else changes.** The checker resolves a head against bindings
  before commands, the runtime's `resolve` does the same, and the
  elaborator's `is_bound` already chose `App(Force)` for a bound head. The
  refusal was the lone place where the host's `PATH` was consulted to decide
  whether a binding was legal.
- **`^name` is the escape**, as it has been since
  [[decisions/260911_an-external-is-a-byte-operation|an-external-is-a-byte-operation]].

## Why

A script's meaning must not depend on its host. The old check made the same
source bind on one machine and be refused on another, and again on the same
machine after an install; it fired on `let b` where a home directory's `bin`
held a `b`. The source said "machine-dependent" of itself as a known
weakness. A static rule that never reads the filesystem has none of these
faults, and a bound name that hides a command is visible in the source.

Two phases no longer reason about one name: the decision was lexical (the
elaborator) but the denial was dynamic (I/O), and the depth proxy
(`at_session_scope`) that scoped the denial is not needed either.

## What it costs

A session binding can hide a command the author forgot existed, such as
`let test = …` silently replacing `test`. Reading `$test` still shows the
binding, `explain name` reports what it shadows, and `^test` names the
command. Hiding a prelude function or builtin was already legal.
