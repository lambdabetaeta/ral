---
status: active
---

# Structural prevention of the reviewed bug classes

A wide review on 2026-06-14 (33 reviewers across `core`/`ral`/`exarch`, every
finding adversarially re-read against the real code) confirmed 73 defects. No
unsoundness in the type system, no memory-safety hole in safe Rust, no leak in
the evaluator core — but the *serious* bugs were not scattered. They collapse
into **nine recurring shapes**, and in every case the shape was writable because
a constraint that should live in a type (or a lint) lived only in a programmer's
head.

## The decision

**For each reviewed bug class, make the bad state unconstructable with a type;
add a lint only to guard the one sanctioned construction site.** This is the
discipline `clippy.toml` already applies to path handling — and two of the
serious bugs (`resolve-path` against the process cwd, the XDG `..` escape) slipped
through *gaps in lints that already exist*, which is the strongest evidence that
the lever is the right one and merely incomplete.

The structural rewrites are staged behind the immediate point-fixes: the review
landed the safe one-line corrections first; the types below are the durable
follow-ups, each tracked as its own slice.

## The nine shapes

1. **Check/use path mismatch — the path *authorised* ≠ the path *used*.** The XDG
   under-HOME guard compared an un-normalised prefix while the gate later
   collapsed `..`; `resolve-path` canonicalised against the process cwd while its
   gate lexed against the logical cwd. *Missing type:* one `ResolvedPath`
   (absolute, `.`/`..`-collapsed, logical-cwd-anchored), the sole input to both
   `capability::check_fs_*` and any canonicaliser, and a `NormalizedPrefix` for
   the frozen grant minted by the same lexer the check uses. Extends
   [[decisions/260605_capability-stage-collapse|capability-stage-collapse]] (the
   always-frozen prefix) and
   [[decisions/260601_xdg-resolver-consolidation|xdg-resolver-consolidation]]; the
   OS enforcer does not save us because it renders the same collapsed surface
   ([[design/two-enforcers|two-enforcers]], [[internals/capability-enforcement|capability-enforcement]]).

2. **Cooperative cancellation that never cancels.** `cancel`/`race` flip a flag
   but never cancel a scope, and `spawn_thread` hands the worker a *fresh*
   `CancelScope::root()` instead of `parent.cancel.child()`; the exarch walk
   builtins never poll `process::check`. *Missing shape:* `spawn_thread` requires
   a child scope and stores it on the handle, `CancelScope::root()` drops to
   `pub(crate)` (session-init only), and a `cancellable(walk)` adaptor polls per
   item — with the `WalkBuilder::build` ban as backstop. The machinery exists
   (`CancelScope::child` is already there); the wiring is absent. Completes
   [[decisions/260504_hot-path-cancellation|hot-path-cancellation]] for the
   builtin/worker sites it did not reach.

3. **A privileged fd/handle leaks into a confined child.** The IPC socket is held
   across the spawn that launches confined externals and is never re-armed
   CLOEXEC, so an external can forge the eval response; on Windows the confined
   child inherits *every* handle (`bInheritHandles=1`, no `HANDLE_LIST`) and the
   IPC pipe is squattable. *Missing types:* an RAII `IpcEndpoint` (CLOEXEC /
   non-inheritable at construction, fd lent only inside a scope), a
   `ConfinedSpawn` whose only constructor carries the explicit inherit allow-list
   and always installs `HANDLE_LIST`, and a `Tokened<ChildEvalResponse>` so a
   forged frame is a parse error. This is the unguarded sibling of
   [[decisions/260611_authenticated-confinement-marker|authenticated-confinement-marker]]:
   that decision authenticated the *request*; the *response* direction stayed
   trusted. The OS sandbox confines syscalls but does not revoke an already-open
   socket — [[design/two-enforcers|two-enforcers]], [[design/grant|grant]].

4. **An unchecked range that can invert → slice panic.** An untrusted plugin's
   `HighlightSpan{start:5,end:2}` reaches `&mut styles[5..2]` and panics the REPL
   on every keystroke. *Missing type:* a `Span { start, len }` (half-open, never
   inverted) whose only constructor orders-and-clamps once. The type *is* the fix.

5. **Treating a record as an ordered key-stream.** `_ed-set [text,cursor]` clamps
   the cursor against the *old* text because `Value::Map` iterates sorted and
   `"cursor" < "text"`; the bool decoders fold a `matches!` extractor with no type
   check. *Missing pattern:* decode a record into a typed struct by name and act
   in dependency order, with one strict `decode_bool` shared from `decode_net` —
   never a fold over entries for known fields.

6. **REPL continuation by token-suffix heuristic, not the parser's own signal.** A
   comment-to-EOF dropped the `UnterminatedBalanced` signal; a trailing bare `=`
   was guessed incomplete, hanging the REPL. *Missing type:* a structured
   `Incompleteness` the parser returns, with `needs_continuation` reduced to
   `matches!(err, Incomplete(_))` — no last-token matching.
   ([[internals/surface-syntax|surface-syntax]].)

7. **Provider-error classification by scraping `Display` strings.** `from_genai`
   read status only from the streaming error shape, so compaction's 5xx/429 fell
   through to non-retryable. *Robust shape:* classify on genai's typed error
   *variants* and structured status, not the formatted string.
   ([[map/exarch/provider|provider]].)

8. **A safety guard compiled out in release.** The PWD/OLDPWD guard is a
   `debug_assert!`, so release builds let `within [env:[PWD:…]]` rewrite the
   shell's cwd. *Missing type:* PWD/OLDPWD are derived state, excluded as
   settable env keys at the `within [env:]` boundary — a hard error naming
   `cd`, never a silent drop; the `debug_assert` goes.
   Relates to [[decisions/260531_env-is-dynamic-only|env-is-dynamic-only]].

9. **A diagnostic source-identity that is never populated (a dead guard).** The
   cross-source caret guard keys on `SourceLoc.file == Some(f)`, but every
   production site sets `file: None`, so a `source`d module's runtime error draws
   its caret at the wrong bytes of the top-level script. *Missing type:* a
   non-optional source identity (or `(SourceId, Span)` resolved once at render)
   on locations that cross a module boundary. ([[map/core/diagnostics|diagnostics]].)

## Where

The full review record (per-finding evidence, the immediate fixes, the held
cross-crate dedups, and the four findings parked for human judgement) lives in
the verification transcripts of the 2026-06-14 review run. The durable content —
why these shapes recur and how a type forecloses each — is this page.

## Decisions

The open questions are resolved (2026-06-15). Slices land incrementally on
`main`, one commit per class, each gated on check + clippy + tests + the
cross-target checks for any cfg-gated code.

- **Class 6.** Drive continuation off a real `Incompleteness` signal, parsing on
  the REPL submit path (one parse per Enter — negligible).
- **Class 1.** Full sweep: `ResolvedPath` is the *sole* path input at every
  `capability::check_*` and canonicaliser, not a localised patch.
- **Class 3.** Adopt the full structural prevention — the RAII `IpcEndpoint`,
  the `Tokened<ChildEvalResponse>`, and the `ConfinedSpawn` handle-list builder —
  for the cleanest account, even though the `CLOEXEC` re-arm + Windows
  `HANDLE_LIST` alone close the confirmed forge (an external that never inherits
  the fd cannot write the channel). The re-arm, `HANDLE_LIST`, and the pipe
  squat-hardening (`FIRST_PIPE_INSTANCE` + a per-user DACL) are the immediate
  close; the types are the durable prevention.
- **Class 8.** Setting `PWD`/`OLDPWD` through `within [env: …]` is a hard error
  naming `cd`, never a silent drop; the `debug_assert` goes.
- **Class 9.** The full `(SourceId, Span)` rework — a location carries a source
  identity resolved once at render — not merely populating `SourceLoc.file`.
- **Held cross-crate dedups.** One shared user/home/`USER` resolver in
  `core::path`, which ral and exarch call directly.
- **The four uncertain.** `exarch-session:2` → an atomic, set-once (`static mut`
  retired); `typecheck-infer:4` → a defensive recursion bound even absent a
  reproducing program, matching the unifier's co-inductive discipline;
  `types-shell:2` → not a defect (OLDPWD = the effective cwd before the `cd`),
  documented only; `exarch-tui:3` → consolidate the five wrap loops behind one
  row-builder helper.

## A correction this implies

`elaborator-ir:1` (forward-declaration masking an earlier command-use of an
acyclic thunk-form `let`) falsifies the claim in
[[internals/surface-syntax|surface-syntax]] that "forward declarations cover only
the thunk-form bindings `group.rs` knots, so the shadow agrees with what
evaluation will see." Once the fix lands (forward-declaration scoped to the
recursive group), that internals page must be re-verified.

See also [[design/capability-freeze|capability-freeze]],
[[internals/capability-enforcement|capability-enforcement]],
[[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]].
