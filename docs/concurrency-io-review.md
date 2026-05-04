# Concurrency / IO design review

Notes on the post-mux design (commit `e23f291`): `Sink::LineFramed` +
`Sink::External`, fixed-arity `_spawn` / `_watch`, `watch` demoted to a
prelude alias.

## What the design gets right

- **Line framing lives at the sink layer.** `Sink::LineFramed { inner,
  prefix, pending }` (core/src/io.rs:217-221) makes framing a property
  of *where bytes go* rather than a separate subsystem.  It composes
  naturally with `Tee`, `Buffer`, `Null`; deleting `core/src/mux.rs`
  (230 lines) for a single enum variant is a clear net win.
- **Frontend boundary is clean.** `Sink::External(Arc<dyn ExternalWrite>)`
  plus the `ExternalWrite` trait (core/src/io.rs:189-191) lets the REPL
  inject rustyline's `ExternalPrinter` at `env.io.stdout` without core
  depending on rustyline.  Nothing else in the pipeline has to know.
- **Atomicity story is honest.**  Each `LineFramed` write emits one
  `prefix + line + '\n'` via a single `inner.write_bytes` call.  For
  `Terminal` that call goes through `std::io::Stdout`'s reentrant
  lock; for `External` it goes through the adapter's own mutex.  No
  bespoke synchronisation.
- **`try_clone` resets `pending` per clone** (core/src/io.rs:432-438).
  Two threads writing partial lines must not share a carry buffer;
  this is the correct call.
- **Surface reduction.**  `_fork` / `_fork_watched` → `_spawn` (arity 1)
  / `_watch` (arity 2); `watch` demoted from keyword to one-line
  prelude alias over `_watch`.  No lost expressiveness, one fewer
  special parse path.

## Sharp edges worth knowing

1. **`wire_command_stdout` with `inherit_tty=true`** (core/src/io.rs:364)
   dups fd 1 straight into the child, bypassing any `LineFramed` or
   `External` framing above it.  Today's guard is
   `matches!(self, Terminal | Stderr)`, so it's safe, but any future
   `Sink` variant that wants inherit-style behaviour must remember to
   opt out of framing.  Easy to get wrong in a future change.
2. **Watched stderr goes to parent's *stdout*** (with `[label:err]`
   prefix), not parent's stderr.  Matches the previous mux behaviour
   and produces one totally-ordered stream, but it's surprising —
   `watch "x" { cmd 2>&1 }` is now redundant.  Worth an explicit line
   in SPEC §13.5 if not already there.
3. **Watched handles replay nothing on `_await`.**  Bytes flow live
   through `LineFramed`; the handle's buffer stays empty.
   Intentional, but means `spawn` and `watch` have visibly different
   `await` semantics — users should be told.
4. **`Sink` is up to 8 variants.**  Every new sink variant touches
   roughly five match sites (`write_bytes`, `flush_pending`,
   `as_stdio`, `needs_pump`, `wire_command_stdout`, `try_clone`,
   `with_child_stdout`).  Still tractable; if the enum approaches ~12
   variants, consider a `Box<dyn SinkOps>` trait object.
5. **No backpressure on watched writers.**  A tight-loop child spams
   the terminal.  Matches `bash &`, but don't be surprised when
   someone hits it.

## Invariants that must not quietly break

- `LineFramed::pending` is per-clone, never shared.  Sharing would
  interleave partial lines between threads.
- `flush_pending` must be called on every `LineFramed` sink at
  end-of-life (the `pump` thread does this in core/src/io.rs:398).
  Panicking pump threads drop the trailing partial line; acceptable
  but recorded here so nobody "fixes" the no-op flush later.
- `Sink::External`'s trait object is `Send + Sync`; callers assume
  `Sink` stays `Send` as long as every variant's contents are `Send`.
