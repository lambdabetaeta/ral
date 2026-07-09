//! Compare list-representation costs:
//!   A. recursive destructure-and-cons over `Arc<Vec<V>>`  (current prelude pattern)
//!   B. same algorithm over `imbl::Vector<V>`              (persistent variant)
//!   C. iterative one-pass map over `&[V]`                 (current builtin pattern)
//!   D. iterative one-pass map over `imbl::Vector<V>`      (builtin if we switched repr)
//!
//! Each scenario is capped by both element count and a wall-clock timeout.

use imbl::Vector;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
enum V {
    Int(i64),
    String(String),
}

const ELEM_TIMEOUT: Duration = Duration::from_secs(5);

fn make_strings(n: usize) -> Vec<V> {
    (0..n)
        .map(|i| V::String(format!("/some/path/segment/number-{i:08}")))
        .collect()
}

fn upper_v(v: &V) -> V {
    match v {
        V::Int(i) => V::Int(*i + 1),
        V::String(s) => V::String(s.to_uppercase()),
    }
}

// ── A: recursive destructure + cons over Arc<Vec<V>> ────────────────────────
//
// Mirrors `let [h, ...t] = $xs; return [!{f $h}, ...!{map $f $t}]`.
// `tl` clones the tail slice (`pattern.rs:52`), `cons` allocates fresh Vec
// (the COW path doesn't fire because the source is still bound).
fn rec_map_arc_vec(arc: &Arc<Vec<V>>) -> Arc<Vec<V>> {
    if arc.is_empty() {
        return Arc::new(Vec::new());
    }
    let head = upper_v(&arc[0]);
    let tail: Arc<Vec<V>> = Arc::new(arc[1..].to_vec()); // destructure tail clone
    let mapped_tail = rec_map_arc_vec(&tail);
    let mut out = Vec::with_capacity(mapped_tail.len() + 1);
    out.push(head);
    out.extend((*mapped_tail).iter().cloned());
    Arc::new(out)
}

// ── B: recursive destructure + cons over imbl::Vector<V> ────────────────────
//
// `head + tail.skip(1)` is O(log n). `push_front` is O(1) amortised.
fn rec_map_imbl(v: &Vector<V>) -> Vector<V> {
    if v.is_empty() {
        return Vector::new();
    }
    let head = upper_v(v.front().unwrap());
    let tail = v.skip(1);
    let mut mapped_tail = rec_map_imbl(&tail);
    mapped_tail.push_front(head);
    mapped_tail
}

// ── C: iterative one-pass over &[V]  (current builtin) ──────────────────────
fn iter_map_slice(xs: &[V]) -> Vec<V> {
    let mut out = Vec::with_capacity(xs.len());
    for v in xs {
        out.push(upper_v(v));
    }
    out
}

// ── D: iterative one-pass over imbl::Vector<V> ──────────────────────────────
fn iter_map_imbl(v: &Vector<V>) -> Vector<V> {
    let mut out = Vector::new();
    for x in v {
        out.push_back(upper_v(x));
    }
    out
}

/// Run `body` once with a timeout watchdog. Returns Some(elapsed) on success,
/// None if the elapsed time exceeded the cap. We can't preempt the function,
/// so this only catches "took too long, skip larger sizes" — the call still
/// completes, but we won't try anything bigger.
fn timed<F, T>(label: &str, n: usize, cap: Duration, body: F) -> Option<Duration>
where
    F: FnOnce() -> T,
{
    let t = Instant::now();
    let _ = body();
    let dt = t.elapsed();
    println!("  {label:<40} n={n:>7}  {dt:>10.3?}");
    if dt > cap { None } else { Some(dt) }
}

fn make_ints(n: usize) -> Vec<V> {
    (0..n).map(|i| V::Int(i as i64)).collect()
}

// E. Tight-inner-loop fold over &[V]:  per-element work is a single i64 add,
//    so iteration overhead dominates.  Worst case for any non-Vec representation.
fn iter_fold_slice(xs: &[V]) -> i64 {
    let mut acc: i64 = 0;
    for v in xs {
        if let V::Int(i) = v {
            acc = acc.wrapping_add(*i);
        }
    }
    acc
}

// F. Same fold over imbl::Vector — iteration via the persistent cursor.
fn iter_fold_imbl(v: &Vector<V>) -> i64 {
    let mut acc: i64 = 0;
    for x in v {
        if let V::Int(i) = x {
            acc = acc.wrapping_add(*i);
        }
    }
    acc
}

struct Alive([bool; 6]);

fn run_pass(name: &str, ints: bool) {
    // n=100_000 omitted: scenario A stack-overflows at that depth before
    // the cap check can fire.
    let sizes = [10usize, 100, 1_000, 10_000];
    let mut alive = Alive([true; 6]);
    let element = if ints { "V::Int(i)" } else { "V::String(...)" };

    println!("\n══════════════════════════════════════════════════════════════════");
    println!("PASS: {name}");
    println!("element type: {element}");
    println!("cap {ELEM_TIMEOUT:?} per scenario");
    println!("══════════════════════════════════════════════════════════════════");

    for &n in &sizes {
        let raw = if ints { make_ints(n) } else { make_strings(n) };
        let arc = Arc::new(raw.clone());
        let imv: Vector<V> = raw.iter().cloned().collect();
        println!("── n = {n} ──");

        let labels = [
            "A. recursive  Arc<Vec<V>>",
            "B. recursive  imbl::Vector",
            "C. iterative  &[V] map     ",
            "D. iterative  imbl  map    ",
            "E. tight-fold &[V]         ",
            "F. tight-fold imbl         ",
        ];

        let runs: [Box<dyn FnOnce()>; 6] = [
            Box::new(|| {
                let _ = rec_map_arc_vec(&arc);
            }),
            Box::new(|| {
                let _ = rec_map_imbl(&imv);
            }),
            Box::new(|| {
                let _ = iter_map_slice(&raw);
            }),
            Box::new(|| {
                let _ = iter_map_imbl(&imv);
            }),
            Box::new(|| {
                let _ = iter_fold_slice(&raw);
            }),
            Box::new(|| {
                let _ = iter_fold_imbl(&imv);
            }),
        ];

        for (i, run) in runs.into_iter().enumerate() {
            if !alive.0[i] {
                continue;
            }
            if timed(labels[i], n, ELEM_TIMEOUT, run).is_none() {
                alive.0[i] = false;
                println!("    ({} exceeded cap; skipping larger n)", &labels[i][..1]);
            }
        }

        println!();
        if !alive.0.iter().any(|x| *x) {
            println!("all scenarios exceeded cap; stopping");
            break;
        }
    }
}

fn main() {
    run_pass("strings", false);
    run_pass("ints", true);
}
