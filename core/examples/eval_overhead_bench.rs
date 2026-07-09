//! Per-thunk-call evaluator overhead.
//!
//! Times `Shell::child_of(&captured, &mut parent)` + `child.return_to(&mut parent)`
//! — the two operations that bracket every same-thread thunk body — as a
//! function of `Context.env_overrides` size.
//!
//! Pre-migration this was the dominant cost behind O(n²)-ish recursive
//! prelude code: each call deep-cloned a `HashMap` of 50–200 process-env
//! entries, and n recursive calls compounded to O(n²).  Post-migration
//! (env_vars + aliases + module-cache as `imbl::HashMap`) the snapshot is
//! an Arc-bump per field — the per-call wall-clock should be flat in N.
//!
//! Run:
//!     docker exec shell-dev bash -c \
//!       'cd /work && cargo run --release --example eval_overhead_bench'

use ral_core::Shell;
use ral_core::io::TerminalState;
use ral_core::types::Env;
use std::time::Instant;

const ITERS: u32 = 200_000;

fn populate_env(shell: &mut Shell, n: usize) {
    let pairs = (0..n).map(|i| (format!("RAL_BENCH_VAR_{i:05}"), format!("val_{i}")));
    shell.extend_env(pairs);
}

fn time_thunk_bracket(shell: &mut Shell, captured: &Env, iters: u32) -> std::time::Duration {
    let t0 = Instant::now();
    for _ in 0..iters {
        let mut child = Shell::child_of(captured, shell);
        child.return_to(shell);
    }
    t0.elapsed()
}

fn main() {
    let captured = Env::new();
    let sizes = [0usize, 10, 100, 1_000, 10_000];

    println!("\n══════════════════════════════════════════════════════════════════");
    println!("Per-thunk-call bracket: child_of + return_to");
    println!("Each row = {ITERS} iterations against an env_vars of size N.");
    println!("══════════════════════════════════════════════════════════════════");
    println!("{:>9}  {:>14}  {:>12}", "N", "total", "ns/iter");

    for &n in &sizes {
        // Fresh shell per row so prior populations don't leak.
        let mut shell = Shell::new(TerminalState::default());
        populate_env(&mut shell, n);

        // One warm-up pass to settle the page cache and avoid first-iter noise.
        let _ = time_thunk_bracket(&mut shell, &captured, 1_000);

        let dt = time_thunk_bracket(&mut shell, &captured, ITERS);
        let ns_per = dt.as_nanos() as f64 / f64::from(ITERS);
        println!("{:>9}  {:>14.3?}  {:>12.1}", n, dt, ns_per);
    }
    println!();
    println!("Expectation post-imbl::HashMap: ns/iter is roughly flat across N");
    println!("(structural sharing — clone is an Arc bump, not a deep copy).");
}
