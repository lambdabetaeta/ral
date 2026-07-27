//! The durable audit trail behind every host-side CONNECT tunnel.
//!
//! Bundled as [`Egress`], threaded through [`crate::agent::Agent`] exactly
//! as `disk_warn_bytes` is. Runs entirely host-side; constructed once at
//! trunk launch and inherited verbatim by every spawned child — a spawn
//! shares its parent's IT policy and audit trail, never a fresh one.

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

/// Rotation threshold: past this, the next write rotates the live file to
/// `.1` first. 32 MiB holds weeks of tunnel records.
const ROTATE_AT_BYTES: u64 = 32 * 1024 * 1024;

/// The durable, append-only network ledger.
///
/// One compact JSON object per line: an append-mode file's single `write()`
/// lands atomically at the end, so concurrent exarch/synod processes can
/// share the file without interleaving records. Rotation is additionally
/// guarded by an `flock` on a sibling lock file (Unix only), so two
/// processes racing the size check cannot each rotate and destroy the
/// other's history.
#[derive(Clone)]
pub struct AuditLog(Arc<Mutex<LedgerFile>>);

struct LedgerFile {
    path: std::path::PathBuf,
    file: std::fs::File,
    #[cfg(unix)]
    rotation_lock: std::fs::File,
}

/// Holds `rotation_lock` exclusively for the check-size/rotate/write
/// sequence, across processes as well as threads. Unlocked on every path via
/// `Drop`, including error returns. Locks a dup'd fd so the borrow of the
/// `LedgerFile` it comes from ends before `record` needs to mutate it.
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
        let file = open_audit_file(path)?;
        #[cfg(unix)]
        let rotation_lock = open_audit_file(&path.with_extension("jsonl.lock"))?;
        Ok(Self(Arc::new(Mutex::new(LedgerFile {
            path: path.to_path_buf(),
            file,
            #[cfg(unix)]
            rotation_lock,
        }))))
    }

    /// Open this app's ledger at `$XDG_STATE_HOME/<app>/net-audit.jsonl`,
    /// once per process, append-mode.
    ///
    /// # Errors
    /// Returns `Err` if the ledger cannot be created or opened.
    pub fn open(app: crate::bootstrap::App) -> std::io::Result<Self> {
        let path = app
            .xdg_dir(ral_core::path::basedir::XdgKind::State)
            .join("net-audit.jsonl");
        Self::at(&path)
    }

    /// A ledger at a caller-chosen path, for tests.
    ///
    /// # Panics
    /// Panics if the file cannot be created — a test fixture failure, not a
    /// runtime condition to recover from.
    #[must_use]
    pub fn for_test(path: &std::path::Path) -> Self {
        Self::at(path).expect("test audit ledger")
    }

    /// Append one record. `Err` means the gate must close: an unauditable
    /// proxy does not proxy. A poisoned lock (only reachable after a prior
    /// panic mid-write) is also `Err`.
    ///
    /// # Errors
    /// Returns `Err` if the ledger's lock is poisoned, if rotation fails, or
    /// if the write itself fails.
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
        // Cross-process rotation guard; non-Unix falls back to the mutex
        // above, which only serialises threads within this process.
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

/// The IT-set policy and the durable audit ledger, threaded everywhere
/// `disk_warn_bytes` is. Cheap to clone; children inherit both verbatim.
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
    /// Load the real IT policy and open this app's real audit ledger —
    /// the one production call site, at trunk launch.
    ///
    /// # Errors
    /// Returns `Err` if the policy fails to load or the audit ledger cannot
    /// be opened, naming which of the two failed.
    pub fn open(app: crate::bootstrap::App) -> Result<Self, String> {
        let policy = crate::net_policy::load()?;
        Ok(Self {
            policy: Arc::new(policy),
            audit: AuditLog::open(app).map_err(|e| format!("network audit ledger: {e}"))?,
        })
    }

    /// A test double: a permissive policy (`example.com` and `a.example`),
    /// and a throwaway ledger under a fresh per-call path.
    ///
    /// # Panics
    /// Panics if either fixture host fails to parse — it cannot, since both
    /// are literal valid host names.
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
        let path = std::env::temp_dir().join(format!(
            "exarch-egress-test-{}-{}.jsonl",
            std::process::id(),
            crate::agent::fresh_id()
        ));
        Self {
            policy: Arc::new(policy),
            audit: AuditLog::for_test(&path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "exarch-egress-test-{}-{tag}.jsonl",
            std::process::id()
        ))
    }

    /// Two successive records land as two independently-parseable JSON
    /// lines — an allowed tunnel and a refused one alike.
    #[test]
    fn audit_log_records_two_independently_parseable_lines() {
        let path = tmp_path("audit-two-lines");
        let _ = std::fs::remove_file(&path);
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

    /// Once the ledger exceeds its cap, the next write rotates the old
    /// contents to `.1` and keeps appending to a fresh file.
    #[test]
    fn a_ledger_that_exceeds_its_cap_rotates_and_keeps_appending() {
        let path = tmp_path("audit-rotate");
        let rotated = path.with_extension("jsonl.1");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&rotated);
        let log = AuditLog::at(&path).expect("test ledger");
        {
            let ledger = log.0.lock().unwrap();
            ledger.file.set_len(ROTATE_AT_BYTES).unwrap();
        }
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

    /// Two independent `AuditLog` handles on the same path — standing in for
    /// two racing processes, since each has its own in-process mutex — write
    /// across the cap boundary concurrently. The `flock` must serialise the
    /// check-size/rotate/write sequence between them so every record lands
    /// exactly once across the live file and its rotated `.1`.
    #[test]
    fn rotation_under_cross_process_contention_loses_no_record() {
        let path = tmp_path("audit-race");
        let rotated = path.with_extension("jsonl.1");
        let lock = path.with_extension("jsonl.lock");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&rotated);
        let _ = std::fs::remove_file(&lock);

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

    /// A record that cannot be written reports `Err` rather than being
    /// silently dropped — including when the ledger's lock is poisoned by
    /// an earlier panic.
    #[test]
    fn a_record_that_cannot_be_written_reports_err() {
        let path = tmp_path("audit-poisoned");
        let _ = std::fs::remove_file(&path);
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
