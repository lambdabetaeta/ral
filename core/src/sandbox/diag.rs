//! Sandbox diagnostics: harvest kernel-reported sandbox denials and
//! attribute them to the current tool call's descendant tree.
//!
//! Kernel sandbox enforcement (Seatbelt on macOS, the seccomp BPF
//! filter inside the bwrap envelope on Linux) reports denied syscalls
//! into a system log keyed by `(comm, pid)`.  This module reads that
//! log over a tool call's wall window, keeps only lines whose PID was
//! observed in the call's descendant tree, and appends them to the
//! call's stderr between marker lines.
//!
//! Each platform's [`platform`] submodule supplies a reader
//! for the kernel-log window plus a small parser pair that recognises
//! a denial line and extracts its attributed PID.  The harvester
//! [`append_denials_for_failed_call`] composes them with the PID set
//! produced by [`DescendantTracker`].  Platforms without a denial
//! source (everything except macOS and Linux) get a stub `platform`
//! module that reports no denials.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[cfg(target_os = "macos")]
#[path = "sandbox_diag/macos.rs"]
mod platform;
#[cfg(target_os = "linux")]
#[path = "sandbox_diag/linux.rs"]
mod platform;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use std::time::Duration;
    pub(super) fn read_window(_: Duration) -> Option<String> {
        None
    }
    pub(super) fn is_denial_line(_: &str) -> bool {
        false
    }
    pub(super) fn extract_pid(_: &str) -> Option<u32> {
        None
    }
}

/// Read the kernel sandbox-denial log over the tool call's wall
/// window, keep only lines attributed to the descendant tree, and
/// append them to `stderr_bytes` between marker lines.
///
/// No-op when `descendant_pids` is empty (i.e. unrestricted policy,
/// or polling found nothing) or when the platform has no denial
/// source.  Concurrent denials from processes outside this turn's
/// subtree are filtered out by the PID set.
pub(crate) fn append_denials_for_failed_call(
    elapsed: Duration,
    descendant_pids: &HashSet<u32>,
    stderr_bytes: &mut Vec<u8>,
) {
    if descendant_pids.is_empty() {
        return;
    }
    let Some(text) = platform::read_window(elapsed) else {
        return;
    };
    let denials: Vec<&str> = text
        .lines()
        .filter(|l| platform::is_denial_line(l))
        .filter(|l| platform::extract_pid(l).is_some_and(|p| descendant_pids.contains(&p)))
        .collect();
    if denials.is_empty() {
        return;
    }
    stderr_bytes.extend_from_slice(b"\n--- kernel-reported sandbox denials during this turn ---\n");
    for line in &denials {
        stderr_bytes.extend_from_slice(line.as_bytes());
        stderr_bytes.push(b'\n');
    }
    stderr_bytes.extend_from_slice(b"--- end sandbox denials ---\n");
}

/// Background sampler that records every PID descended from this
/// process for the lifetime of a tool call.  Kernel denial records
/// carry only `(comm, pid)`, so a post-hoc PID set is the only way
/// to attribute a denial to "our" subprocess tree rather than to a
/// concurrent system service that happened to be running during the
/// same wall second.
///
/// Tick cadence is ~100 ms — fast enough to catch typical child
/// lifetimes (rustc, ld, git, cargo build scripts) and slow enough
/// that the polling cost stays inside single-digit-percent CPU even
/// during long calls.  Sub-100 ms processes that fork-exec-exit in
/// one tick window are missed; in practice they don't have time to
/// hit sandbox-relevant syscalls anyway.
pub(crate) struct DescendantTracker {
    pids: Arc<Mutex<HashSet<u32>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl DescendantTracker {
    pub(crate) fn start() -> Self {
        let root_pid = std::process::id();
        let pids: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let pids = pids.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let live = sample_descendants(root_pid);
                    if !live.is_empty() {
                        let mut p = pids.lock().unwrap();
                        for pid in live {
                            p.insert(pid);
                        }
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            })
        };
        Self {
            pids,
            stop,
            handle: Some(handle),
        }
    }

    /// Stop the polling thread, join it, and return the union of
    /// every descendant PID observed across all ticks.
    pub(crate) fn finish(mut self) -> HashSet<u32> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.pids.lock().unwrap().clone()
    }
}

impl Drop for DescendantTracker {
    fn drop(&mut self) {
        // Belt-and-braces against an early return from the caller
        // skipping the explicit `finish`: signal the thread to stop
        // so it doesn't outlive its caller.  We deliberately don't
        // join here — Drop must not block on a slow `ps`.
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// One sample of the descendant set: shell out to `/bin/ps` for every
/// live `(pid, ppid)` pair, then BFS from `root` through the inverted
/// parent map.  Returns descendants only — `root` itself is excluded.
fn sample_descendants(root: u32) -> Vec<u32> {
    let Ok(out) = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid="])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut children_of: HashMap<u32, Vec<u32>> = HashMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(pid_s) = parts.next() else {
            continue;
        };
        let Some(ppid_s) = parts.next() else {
            continue;
        };
        let Ok(pid) = pid_s.parse::<u32>() else {
            continue;
        };
        let Ok(ppid) = ppid_s.parse::<u32>() else {
            continue;
        };
        children_of.entry(ppid).or_default().push(pid);
    }
    let mut seen = HashSet::new();
    let mut frontier = vec![root];
    while let Some(p) = frontier.pop() {
        if let Some(children) = children_of.get(&p) {
            for &c in children {
                if seen.insert(c) {
                    frontier.push(c);
                }
            }
        }
    }
    seen.into_iter().collect()
}
