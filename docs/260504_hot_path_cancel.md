 Plan: Make ral interruptible inside hot Rust loops

 Context

 Today, signal::check(shell) is only polled at expression boundaries inside
 the evaluator (Bind, Chain, Seq, the tail-call trampoline) and inside
 race's polling loop. No check exists inside the evaluator's own
 eval_list loop or inside any of the collection builtins (map, each,
 fold, filter, range, sort-list-by).

 Concrete consequence (from the user's session): a tail-recursive mmap over
 range 1 100000 runs O(n²) because every recursion does
 [!{$f $x}, ...$acc], evaluated by eval_list. The work is now in two tight
 Rust loops: the per-element for elem in elems in eval_list, and the
 items.extend(arc.iter().cloned()) inside the spread branch when the Arc
 isn't unique. Neither yields, so SIGINT sits queued and Ctrl+C is ignored.

 The fix is purely additional poll points. Quadratic loops are a separate user
 problem; a shell that ignores Ctrl+C is a much worse footgun than the perf
 itself.

 Reuses the existing infrastructure:
 - signal::check(shell) at core/src/signal.rs:61 — single atomic load + a
 cancel-scope walk; ~ns. Already returns EvalSignal::Error("interrupted", 130) shaped exactly the way every other call site expects.
 - &mut Shell is threaded through every builtin and through eval_list, so
 no plumbing change is needed.

 Approach

 1. eval_list (the user's actual hot path)

 File: core/src/evaluator.rs:440-502

 - General case loop (lines 488-500): add signal::check(shell)? as the
 first statement inside for elem in elems. Per-iteration cost is one
 atomic load — negligible against the per-element value clone / eval_val.
 - Cons clone fallback (line 458): replace
 v.extend(arc.iter().cloned()) with a chunked extend that polls every
 INTERRUPT_POLL_CHUNK elements (see helper below). This is the path that
 blows up on [x, ...big_xs] when big_xs is shared.
 - Snoc clone fallback (line 478): same chunked extend before the final
 v.push(x).
 - General-case spread clone (line 495): same chunked extend.

 Add a small private helper at the bottom of the file (or a new
 core/src/evaluator/poll.rs):

 /// Number of elements between cooperative `signal::check` polls inside
 /// bulk Rust-level clone/extend loops.  Chosen so the per-chunk overhead
 /// (one atomic load + a cancel-scope walk) is amortised across enough
 /// `Value::clone`s that it's lost in the noise, while keeping worst-case
 /// Ctrl+C latency well under a frame on a modern machine.
 pub(crate) const INTERRUPT_POLL_CHUNK: usize = 1024;

 pub(crate) fn extend_clone_polled(
     dst: &mut Vec<Value>,
     src: &[Value],
     shell: &Shell,
 ) -> Result<(), EvalSignal> {
     for chunk in src.chunks(INTERRUPT_POLL_CHUNK) {
         signal::check(shell)?;
         dst.extend(chunk.iter().cloned());
     }
     Ok(())
 }

 Note: takes &Shell, since signal::check only needs read access. The
 three call sites in eval_list need shell to remain reborrowable, which
 is fine since eval_val calls have already finished by the time we hit the
 extend.

 2. Hot collection builtins

 File: core/src/builtins/collections.rs

 Add crate::signal::check(shell)?; as the first statement of the loop body
 in:

 - builtin_each (line 22, the for item in &items)
 - builtin_map (line 40)
 - builtin_filter (line 103)
 - builtin_fold (line 188)
 - builtin_sort_by (line 141, inside the .map(|item| { ... }) closure
 — easiest to refactor to a plain for loop that pushes into keyed)
 - builtin_range (line 174, the while i < end)

 For builtins that already invoke call_value(func, ..., shell) per element
 (map / each / filter / fold / sort-by), the per-iteration check is dwarfed
 by the user-function call. For range, it's the only work — but a 100k
 range is the realistic upper bound for interactive use, so the cost is
 ~µs total.

 builtin_each/builtin_map use the iterate_audited wrapper that takes a
 closure returning (Value, Option<EvalSignal>). The check inside the loop
 turns the existing match call_value(...) into:

 for item in &items {
     if let Err(e) = crate::signal::check(shell) {
         return (Value::list(out), Some(e));
     }
     match call_value(func, std::slice::from_ref(item), shell) {
         Ok(v) => out.push(v),
         Err(e) => return (Value::list(out), Some(e)),
     }
 }

 (Mirror that pattern in builtin_each.)

 3. What is not in scope here

 - String / bytes builtins, regex, hashmap operations — these have similar
 loops but are bounded by input size and not part of the failing
 reproducer. Worth a follow-up commit; not in this one.
 - Quadratic algorithm shape ([...$acc, x]) — that's user error, not
 something this change addresses. Documenting map / fold in TUTORIAL.md
 as the idiomatic alternative is a separate doc PR.
 - Signal preemption (vs polling). Cooperative is what ral already does
 everywhere; sticking with it keeps the change one-screen.

 - core/src/builtins/collections.rs — add signal::check(shell)? at the
 top of each iteration body in each, map, filter, fold,
 sort-list-by, range.


 sort-list-by, range.
     │ add the helper + constant.                                                                                                          │
     │ - core/src/builtins/collections.rs — add signal::check(shell)? at the                                                               │
     │ top of each iteration body in each, map, filter, fold,                                                                              │
     │ sort-list-by, range.                                                                                                                │
     │                                                                                                                                     │
     │ No public API changes. No new crate dependencies.                                                                                   │
     │                                                                                                                                     │
     │ Verification                                                                                                                        │
     │                                                                                                                                     │
     │ All commands inside Docker (docker exec shell-dev …) per CLAUDE.md.                                                                 │
     │                                                                                                                                     │
     │ 1. Compile + existing tests still pass.                                                                                             │
     │ docker exec shell-dev cargo build                                                                                                   │
     │ docker exec shell-dev cargo test | tail -20                                                                                         │
     │ 2. Reproducer from the session — quadratic mmap stays interruptible.                                                                │
     │                                                                                                                                     │
     │ 2. In an interactive docker exec -it shell-dev ./target/debug/ral session:                                                          │
     │ let mmap = { |f xs|                                                                                                                 │
     │   let go = { |acc xs|                                                                                                               │
     │     if !{is-empty $xs} { $acc }                                                                                                     │
     │     else { let [x, ...xs] = $xs; go [...$acc, !{$f $x}] $xs }                                                                       │
     │   }                                                                                                                                 │
     │   go [] $xs                                                                                                                         │
     │ }                                                                                                                                   │
     │ mmap { |x| $[$x + 1] } !{range 1 100000}                                                                                            │
     │ 2. Press Ctrl+C while it's spinning. Expected: prompt returns within                                                                │
     │ ~100 ms with interrupted (status 130). Without this change, it hangs                                                                │
     │ indefinitely.                                                                                                                       │
     │ 3. Big single spread is also interruptible. Construct a long shared                                                                 │
     │ list and trigger the snoc clone fallback:                                                                                           │
     │ let big = !{range 1 1000000}                                                                                                        │
     │ let _alias = $big                                                                                                                   │
     │ [...$big, 0]                                                                                                                        │
     │ 3. The second binding keeps the Arc non-unique, forcing the clone path.                                                             │
     │ Ctrl+C during construction should unwind promptly.                                                                                  │
     │ 4. map over a big range is interruptible.                                                                                           │
     │ map { |x| $[$x * 2] } !{range 1 1000000}                                                                                            │
     │ 4. Ctrl+C should unwind within a chunk's worth of work.                                                                             │
     │ 5. No measurable regression on small lists. Re-run an existing benchmark                                                            │
     │ or just time a few representative scripts to confirm overhead is in the                                                             │
     │ noise. (One atomic load per 1024 elements in the bulk path; one per                                                                 │
     │ user-callback iteration in the builtins.)                                                                                           │
     ╰─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯

