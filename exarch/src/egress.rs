//! The per-process rate budget and durable audit trail behind every
//! host-side network access — bundled as [`Egress`], threaded through
//! [`crate::agent::Agent`] exactly as `disk_warn_bytes` is.
//!
//! Runs entirely host-side, in the exarch/synod process itself, never inside
//! a jailed or virtualised guest.
//!
//! [`AuditLog`] and [`RateLimiter`] are `Arc`-backed and constructed once
//! at the trunk's own launch, then inherited verbatim by every spawned
//! child (`Agent::fork_with`) — a spawn shares its parent's IT policy, audit
//! trail, and rate budget, never a fresh one of its own.

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A fixed-window per-minute counter, shared by a process tree (the trunk
/// and every fork) — not machine-wide: two fleets running on one machine
/// each get their own budget.
#[derive(Clone)]
pub struct RateLimiter(Arc<Mutex<Window>>);

struct Window {
    started: Instant,
    count: u32,
    cap: u32,
    span: Duration,
}

impl RateLimiter {
    /// A limiter admitting `cap` requests per minute.
    #[must_use]
    pub fn new(cap: u32) -> Self {
        Self::with_window(cap, Duration::from_mins(1))
    }

    /// [`Self::new`] with an explicit window span, for tests that cannot
    /// afford to wait a real minute for the window to roll.
    #[must_use]
    pub fn with_window(cap: u32, span: Duration) -> Self {
        Self(Arc::new(Mutex::new(Window {
            started: Instant::now(),
            count: 0,
            cap,
            span,
        })))
    }

    /// Take one slot from the current window if any remain, rolling to a
    /// fresh window first if the current one has elapsed.
    ///
    /// # Panics
    /// Panics if the internal lock is poisoned — only reachable after a
    /// prior panic while holding it.
    pub fn try_take(&self) -> bool {
        let mut w = self.0.lock().expect("rate limiter poisoned");
        if w.started.elapsed() >= w.span {
            w.started = Instant::now();
            w.count = 0;
        }
        if w.count >= w.cap {
            return false;
        }
        w.count += 1;
        true
    }
}

impl PartialEq for RateLimiter {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// One audit-ledger line.
///
/// A `Name` is a DNS answer, a `Connect` is a TCP accept, a `Request` is one
/// HTTP request/response — the three gates
/// [`crate::net_policy::NetPolicy`] gets consulted at, each worth its own
/// record shape rather than a single lowest-common-denominator one.
#[derive(Clone, Copy)]
pub enum Record<'a> {
    Name {
        name: &'a str,
        allowed: bool,
        addr: Option<Ipv4Addr>,
    },
    Connect {
        name: Option<&'a str>,
        addr: IpAddr,
        port: u16,
        allowed: bool,
        note: Option<&'a str>,
    },
    Request {
        method: &'a str,
        url: &'a str,
        host: &'a str,
        allowed: bool,
        status: Option<u16>,
        bytes: Option<u64>,
        note: Option<&'a str>,
    },
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The durable, append-only network ledger.
///
/// One compact JSON object per line (NDJSON), never `AgentLog`'s
/// pretty-printed multi-line JSON — an append-mode file's single `write()`
/// places its bytes atomically at the file's current end, so concurrent
/// exarch/synod processes sharing one machine can append to the same file
/// without interleaving each other's records, provided each record is
/// written in one syscall. Every access is recorded, allowed or refused,
/// before the caller answers — a `"kind"` field on each line discriminates
/// [`Record`]'s three shapes.
#[derive(Clone)]
pub struct AuditLog(Arc<Mutex<std::fs::File>>);

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:net-audit] opens the per-installation network audit ledger under XDG state, appended to once per name/connect/request; infra bookkeeping, not turn-time model data I/O."
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

impl AuditLog {
    fn at(path: &std::path::Path) -> std::io::Result<Self> {
        Ok(Self(Arc::new(Mutex::new(open_audit_file(path)?))))
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

    /// Append one record. A poisoned lock (only reachable after a prior
    /// panic mid-write) drops the record rather than panicking again —
    /// losing one audit line is the honest outcome, not a second crash.
    pub fn record(&self, r: Record<'_>) {
        let mut line = match r {
            Record::Name {
                name,
                allowed,
                addr,
            } => serde_json::json!({
                "kind": "name",
                "ts": now_secs(),
                "name": name,
                "allowed": allowed,
                "addr": addr.map(|a| a.to_string()),
            }),
            Record::Connect {
                name,
                addr,
                port,
                allowed,
                note,
            } => serde_json::json!({
                "kind": "connect",
                "ts": now_secs(),
                "name": name,
                "addr": addr.to_string(),
                "port": port,
                "allowed": allowed,
                "note": note,
            }),
            Record::Request {
                method,
                url,
                host,
                allowed,
                status,
                bytes,
                note,
            } => serde_json::json!({
                "kind": "request",
                "ts": now_secs(),
                "method": method,
                "url": url,
                "host": host,
                "allowed": allowed,
                "status": status,
                "bytes": bytes,
                "note": note,
            }),
        }
        .to_string();
        line.push('\n');
        let Ok(mut f) = self.0.lock() else { return };
        let _ = f.write_all(line.as_bytes());
    }
}

impl PartialEq for AuditLog {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// The bundle a session threads everywhere `disk_warn_bytes` is threaded.
///
/// The IT-set policy, the durable audit ledger, and the shared rate budget.
/// Cheap to clone (three `Arc`s); every spawned child inherits the same
/// three instances verbatim — one policy, one ledger, one budget per
/// process tree.
#[derive(Clone)]
pub struct Egress {
    pub policy: Arc<crate::net_policy::NetPolicy>,
    pub audit: AuditLog,
    pub limiter: RateLimiter,
}

impl PartialEq for Egress {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.policy, &other.policy)
            && self.audit == other.audit
            && self.limiter == other.limiter
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
        let limiter = RateLimiter::new(policy.rate_per_minute);
        Ok(Self {
            policy: Arc::new(policy),
            audit: AuditLog::open(app).map_err(|e| format!("network audit ledger: {e}"))?,
            limiter,
        })
    }

    /// A test double: a permissive policy (fixed for the identity-seat and
    /// wire-seat tests that use it), a throwaway ledger under a fresh
    /// per-call path, and a generous rate budget.
    #[must_use]
    pub fn for_test() -> Self {
        let policy = crate::net_policy::NetPolicy {
            allow: vec![
                crate::net_policy::Rule {
                    host: "example.com".to_string(),
                    methods: vec!["GET".to_string(), "HEAD".to_string()],
                },
                crate::net_policy::Rule {
                    host: "*.example.org".to_string(),
                    methods: vec!["GET".to_string(), "HEAD".to_string()],
                },
            ],
            max_response_bytes: 1_048_576,
            rate_per_minute: 1_000,
            search: true,
        };
        let limiter = RateLimiter::new(policy.rate_per_minute);
        let path = std::env::temp_dir().join(format!(
            "exarch-egress-test-{}-{}.jsonl",
            std::process::id(),
            crate::agent::fresh_id()
        ));
        Self {
            policy: Arc::new(policy),
            audit: AuditLog::for_test(&path),
            limiter,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// N requests within the window succeed, the next refuses, and a
    /// later request succeeds again once the window has rolled — polled
    /// with a generous deadline rather than a single fixed sleep, since
    /// this host's timing jitter is high.
    #[test]
    fn rate_limiter_refuses_over_cap_then_resets_after_its_window() {
        let limiter = RateLimiter::with_window(2, Duration::from_millis(200));
        assert!(limiter.try_take());
        assert!(limiter.try_take());
        assert!(
            !limiter.try_take(),
            "a third request within the window must be refused"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if limiter.try_take() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the limiter never reset after its window elapsed"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "exarch-egress-test-{}-{tag}.jsonl",
            std::process::id()
        ))
    }

    /// Two successive records land as two independently-parseable JSON
    /// lines — an allowed request and a blocked one alike.
    #[test]
    fn audit_log_records_two_independently_parseable_lines() {
        let path = tmp_path("audit-two-lines");
        let _ = std::fs::remove_file(&path);
        let log = AuditLog::for_test(&path);
        log.record(Record::Request {
            method: "GET",
            url: "https://a.example/",
            host: "a.example",
            allowed: true,
            status: Some(200),
            bytes: Some(12),
            note: None,
        });
        log.record(Record::Request {
            method: "GET",
            url: "https://b.example/",
            host: "b.example",
            allowed: false,
            status: None,
            bytes: None,
            note: Some("refused: not on the allowlist"),
        });
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
}
