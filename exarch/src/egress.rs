//! The IT-set network policy and the durable trail behind every CONNECT
//! tunnel: opened once per front-end process, host side, and inherited
//! verbatim by every child.
//!
//! `guest-net`'s proxy is the only writer of records.

use std::io::Write;
use std::sync::{Arc, Mutex};

/// One CONNECT attempt's verdict, then, on close, what it carried.
#[derive(Clone, Copy)]
pub enum Record<'a> {
    Tunnel {
        host: &'a str,
        addr: Option<std::net::SocketAddr>,
        allowed: bool,
        note: Option<&'a str>,
        up: Option<u64>,
        down: Option<u64>,
    },
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Past this, the next write rotates the live file aside to `.1` first.
const ROTATE_AT_BYTES: u64 = 32 * 1024 * 1024;

/// The durable, append-only network ledger, one compact JSON object per line:
/// an append-mode `write()` lands atomically at the end, so concurrent
/// processes on one app's ledger never interleave records.
#[derive(Clone)]
pub struct AuditLog(Arc<Mutex<LedgerFile>>);

struct LedgerFile {
    path: std::path::PathBuf,
    file: std::fs::File,
    #[cfg(unix)]
    rotation_lock: std::fs::File,
    /// The directory this ledger has to itself, when a test double made it:
    /// dropped with the log, taking the ledger and its rotation lock along.
    /// A real ledger lives under XDG state and owns no directory.
    _scratch: Option<tempfile::TempDir>,
}

/// Held across the whole check-size/rotate/write sequence, between processes
/// as well as threads, so two racing the size check cannot each rotate and
/// destroy the other's history. Locks a dup'd fd so the borrow of the
/// `LedgerFile` it came from ends before `record` mutates it.
#[cfg(unix)]
struct RotationGuard(std::fs::File);

#[cfg(unix)]
impl RotationGuard {
    fn take(file: &std::fs::File) -> std::io::Result<Self> {
        let dup = file.try_clone()?;
        rustix::fs::flock(&dup, rustix::fs::FlockOperation::LockExclusive)?;
        Ok(Self(dup))
    }
}

#[cfg(unix)]
impl Drop for RotationGuard {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock);
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:net-audit] opens the per-installation network audit ledger under XDG state, appended to once per tunnel; infra bookkeeping, not turn-time model data I/O."
)]
fn open_audit_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:net-audit] rotates the network audit ledger once it exceeds its size cap."
)]
fn rotate(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let rotated = path.with_extension("jsonl.1");
    std::fs::rename(path, &rotated)?;
    open_audit_file(path)
}

impl AuditLog {
    fn at(path: &std::path::Path) -> std::io::Result<Self> {
        Self::at_in(path, None)
    }

    fn at_in(path: &std::path::Path, scratch: Option<tempfile::TempDir>) -> std::io::Result<Self> {
        let file = open_audit_file(path)?;
        #[cfg(unix)]
        let rotation_lock = open_audit_file(&path.with_extension("jsonl.lock"))?;
        Ok(Self(Arc::new(Mutex::new(LedgerFile {
            path: path.to_path_buf(),
            file,
            #[cfg(unix)]
            rotation_lock,
            _scratch: scratch,
        }))))
    }

    /// Open this app's ledger at `$XDG_STATE_HOME/<app>/net-audit.jsonl`.
    ///
    /// # Errors
    /// Returns `Err` if the ledger cannot be created or opened.
    pub fn open(app: crate::bootstrap::App) -> std::io::Result<Self> {
        let path = app
            .xdg_dir(ral_core::path::basedir::XdgKind::State)
            .join("net-audit.jsonl");
        Self::at(&path)
    }

    /// A ledger at a caller-chosen path, for a test that reads it back.  The
    /// caller owns the directory, and so decides when the ledger goes.
    ///
    /// # Panics
    /// Panics if the file cannot be created.
    #[must_use]
    pub fn for_test(path: &std::path::Path) -> Self {
        Self::at(path).expect("test audit ledger")
    }

    /// A ledger in a directory of its own, for a test double with no reason to
    /// name a path.  Both the ledger and its rotation lock go when the log does.
    ///
    /// # Panics
    /// Panics if the directory or the file cannot be created.
    #[must_use]
    pub fn in_scratch() -> Self {
        let scratch = tempfile::Builder::new()
            .prefix("exarch-egress-test-")
            .tempdir()
            .expect("test ledger scratch");
        let path = scratch.path().join("net-audit.jsonl");
        Self::at_in(&path, Some(scratch)).expect("test audit ledger")
    }

    /// Append one record. `Err` means the gate must close: an unauditable
    /// proxy does not proxy.
    ///
    /// # Errors
    /// Returns `Err` on a poisoned lock, a failed rotation, or a failed write.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:net-audit] checks the ledger's on-disk size before deciding whether to rotate it."
    )]
    pub fn record(&self, r: Record<'_>) -> std::io::Result<()> {
        let mut line = match r {
            Record::Tunnel {
                host,
                addr,
                allowed,
                note,
                up,
                down,
            } => serde_json::json!({
                "kind": "tunnel",
                "ts": now_secs(),
                "host": host,
                "addr": addr.map(|a| a.to_string()),
                "allowed": allowed,
                "note": note,
                "up": up,
                "down": down,
            }),
        }
        .to_string();
        line.push('\n');
        let mut ledger = self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("network audit ledger lock poisoned"))?;
        // Off Unix only the mutex above serialises, and only within this process.
        #[cfg(unix)]
        let _rotation_guard = RotationGuard::take(&ledger.rotation_lock)?;
        if ledger.file.metadata()?.len() + line.len() as u64 > ROTATE_AT_BYTES {
            ledger.file = rotate(&ledger.path)?;
        }
        ledger.file.write_all(line.as_bytes())
    }
}

impl PartialEq for AuditLog {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Policy and ledger travel together, into an `Agent` like `disk_warn_bytes`.
#[derive(Clone)]
pub struct Egress {
    pub policy: Arc<crate::net_policy::NetPolicy>,
    pub audit: AuditLog,
}

impl PartialEq for Egress {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.policy, &other.policy) && self.audit == other.audit
    }
}

impl Egress {
    /// The real policy and this app's real ledger, once per front-end launch.
    ///
    /// # Errors
    /// Returns `Err` naming whichever of the two failed.
    pub fn open(app: crate::bootstrap::App) -> Result<Self, String> {
        let policy = crate::net_policy::load()?;
        Ok(Self {
            policy: Arc::new(policy),
            audit: AuditLog::open(app).map_err(|e| format!("network audit ledger: {e}"))?,
        })
    }

    /// A test double: a permissive policy (`example.com`, `a.example`, search
    /// on — mirrored by `agent::build`'s fixture) and a throwaway ledger.
    ///
    /// # Panics
    /// Panics if either fixture host fails to parse; both are valid literals.
    #[must_use]
    pub fn for_test() -> Self {
        let policy = crate::net_policy::NetPolicy {
            hosts: [
                crate::net_policy::Host::parse("example.com").expect("valid fixture host"),
                crate::net_policy::Host::parse("a.example").expect("valid fixture host"),
            ]
            .into_iter()
            .collect(),
            search: true,
        };
        Self {
            policy: Arc::new(policy),
            audit: AuditLog::in_scratch(),
        }
    }

    /// [`Self::for_test`] with the provider's own web search denied — the IT
    /// verdict a searchless fixture is built from.
    #[must_use]
    pub fn for_test_without_search() -> Self {
        Self {
            policy: Arc::new(crate::net_policy::NetPolicy {
                hosts: std::collections::HashSet::new(),
                search: false,
            }),
            ..Self::for_test()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ledger path in a directory of this test's own.  The guard comes back
    /// with it and must be held: dropping it deletes the directory, and the
    /// ledger, the rotation lock and any rotated `.jsonl.1` with it.
    fn tmp_path(tag: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("exarch-egress-test-{tag}-"))
            .tempdir()
            .expect("test ledger scratch");
        let path = dir.path().join("net-audit.jsonl");
        (dir, path)
    }

    #[test]
    fn audit_log_records_two_independently_parseable_lines() {
        let (_dir, path) = tmp_path("audit-two-lines");
        let log = AuditLog::for_test(&path);
        log.record(Record::Tunnel {
            host: "a.example",
            addr: Some("93.184.216.34:443".parse().unwrap()),
            allowed: true,
            note: None,
            up: Some(120),
            down: Some(4096),
        })
        .expect("write must succeed");
        log.record(Record::Tunnel {
            host: "b.example",
            addr: None,
            allowed: false,
            note: Some("refused: not on the allowlist"),
            up: None,
            down: None,
        })
        .expect("write must succeed");
        let text = std::fs::read_to_string(&path).expect("audit file readable");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "each record is its own line");
        for line in &lines {
            let _: serde_json::Value =
                serde_json::from_str(line).expect("each line must be valid JSON on its own");
        }
        assert!(lines[0].contains("a.example") && lines[0].contains("true"));
        assert!(lines[1].contains("b.example") && lines[1].contains("false"));
    }

    #[test]
    fn a_ledger_that_exceeds_its_cap_rotates_and_keeps_appending() {
        let (_dir, path) = tmp_path("audit-rotate");
        let rotated = path.with_extension("jsonl.1");
        // Through a write handle, before the ledger opens: an append-mode
        // handle carries no write access on Windows, which refuses `set_len`.
        std::fs::File::create(&path)
            .and_then(|f| f.set_len(ROTATE_AT_BYTES))
            .expect("oversized ledger");
        let log = AuditLog::at(&path).expect("test ledger");
        log.record(Record::Tunnel {
            host: "a.example",
            addr: None,
            allowed: true,
            note: None,
            up: None,
            down: None,
        })
        .expect("write must succeed after rotation");
        assert!(rotated.exists(), "the oversized file must be rotated aside");
        let text = std::fs::read_to_string(&path).expect("fresh ledger readable");
        assert_eq!(
            text.lines().count(),
            1,
            "the fresh file holds only the new record"
        );
    }

    /// Two `AuditLog` handles on one path stand in for two racing processes:
    /// each has its own mutex, so only the `flock` can serialise them — and
    /// only on Unix, where `RotationGuard` exists.
    #[cfg(unix)]
    #[test]
    fn rotation_under_cross_process_contention_loses_no_record() {
        let (_dir, path) = tmp_path("audit-race");
        let rotated = path.with_extension("jsonl.1");

        let log_a = AuditLog::at(&path).expect("test ledger a");
        let log_b = AuditLog::at(&path).expect("test ledger b");
        {
            let ledger = log_a.0.lock().unwrap();
            ledger.file.set_len(ROTATE_AT_BYTES - 1).unwrap();
        }

        const PER_THREAD: usize = 20;
        let race = |log: AuditLog, tag: &'static str| {
            std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    let marker = format!("{tag}-{i}");
                    log.record(Record::Tunnel {
                        host: "race",
                        addr: None,
                        allowed: true,
                        note: Some(&marker),
                        up: None,
                        down: None,
                    })
                    .expect("write must succeed under contention");
                }
            })
        };
        let ta = race(log_a, "a");
        let tb = race(log_b, "b");
        ta.join().unwrap();
        tb.join().unwrap();

        let mut combined = std::fs::read_to_string(&path).expect("live file readable");
        if rotated.exists() {
            combined.push_str(&std::fs::read_to_string(&rotated).expect("rotated file readable"));
        }
        for tag in ["a", "b"] {
            for i in 0..PER_THREAD {
                // Quoted: a bare `a-1` is also a substring of `a-10`.
                let marker = format!("\"note\":\"{tag}-{i}\"");
                assert_eq!(
                    combined.matches(&marker).count(),
                    1,
                    "record {tag}-{i} must appear exactly once"
                );
            }
        }
    }

    #[test]
    fn a_record_that_cannot_be_written_reports_err() {
        let (_dir, path) = tmp_path("audit-poisoned");
        let log = AuditLog::at(&path).expect("test ledger");
        let guard = log.clone();
        let _ = std::thread::spawn(move || {
            let _lock = guard.0.lock().unwrap();
            panic!("poison the ledger lock on purpose");
        })
        .join();
        let err = log.record(Record::Tunnel {
            host: "a.example",
            addr: None,
            allowed: true,
            note: None,
            up: None,
            down: None,
        });
        assert!(err.is_err(), "a poisoned lock must not be silently dropped");
    }
}
