//! `fetch-url`'s host-side mechanics.
//!
//! URL validation, the capped blocking HTTP client, the per-process rate
//! budget, and the durable audit trail — bundled as [`Egress`], threaded
//! through [`crate::agent::Agent`] exactly as `disk_warn_bytes` is, and read
//! only by [`crate::fleet::desk::ExarchDesk::handle`]'s `` `fetch-url `` arm.
//!
//! Runs entirely host-side, in the exarch/synod process itself, never inside
//! a jailed or virtualised guest: the desk answers every enquiry from the
//! host process, so a fetch never touches the guest's own network posture.
//!
//! [`AuditLog`] and [`FetchLimiter`] are `Arc`-backed and constructed once
//! at the trunk's own launch, then inherited verbatim by every spawned
//! child (`Agent::fork_with`) — a spawn shares its parent's IT policy, audit
//! trail, and rate budget, never a fresh one of its own.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The fixed total-request timeout — connect, TLS, and the whole read,
/// bounded together, a host-side constant regardless of the caller's own
/// turn budget: a `fetch-url` answer is a start receipt, a ledger read, a
/// confirmation, a verdict, never a long-lived stream.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Parse `raw` as an absolute URL and require an `http`/`https` scheme.
///
/// The mitigations (allowlist, caps, audit) are scheme-independent, and a
/// bundled webpki-only trust store has no private-CA story that would let
/// an HTTPS-only rule serve any internal, non-public-CA endpoint.
///
/// # Errors
/// Returns `Err` naming `raw` if it does not parse as a URL, or names the
/// scheme if it is neither `http` nor `https`.
pub fn validate_fetch_url(raw: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw)
        .map_err(|e| format!("fetch-url: '{raw}' is not a valid URL: {e}"))?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        other => Err(format!(
            "fetch-url: '{raw}' uses the '{other}' scheme — only http and https URLs may be fetched"
        )),
    }
}

/// A blocking client bound to exarch's bundled trust store
/// ([`crate::provider::tls::config`]), with a total [`FETCH_TIMEOUT`] — never
/// the model-API transport's own client, whose idle-timeout-only liveness
/// policy has no total cap and is the wrong shape for a bounded answer.
///
/// Redirects are never followed: the allowlist admitted the URL asked for,
/// and following a redirect would fetch an address it never checked.
///
/// # Panics
/// Panics if the preconfigured rustls client fails to build — unreachable
/// in practice, since the TLS config is exarch's own known-good default.
pub fn fetch_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .use_preconfigured_tls(crate::provider::tls::config())
        .redirect(reqwest::redirect::Policy::none())
        .timeout(FETCH_TIMEOUT)
        .build()
        .expect("preconfigured rustls reqwest blocking client")
}

/// `GET url`, streamed through a `cap`-byte ceiling: never more than
/// `cap + 1` bytes are ever buffered before an over-cap response is
/// rejected.
///
/// # Errors
/// Returns `Err` if the request fails, the response redirects or its status
/// is not success, or the body exceeds `cap` bytes.
pub fn fetch(
    client: &reqwest::blocking::Client,
    url: &reqwest::Url,
    cap: u64,
) -> Result<Vec<u8>, String> {
    let response = client
        .get(url.clone())
        .send()
        .map_err(|e| format!("fetch-url: could not reach {url}: {e}"))?;
    if response.status().is_redirection() {
        return Err(format!(
            "fetch-url: {url} answered with a redirect (HTTP {}); redirects are not followed, \
             since where they lead was never checked against your IT department's approved list",
            response.status()
        ));
    }
    if !response.status().is_success() {
        return Err(format!(
            "fetch-url: {url} answered with HTTP {}",
            response.status()
        ));
    }
    let mut body = Vec::new();
    response
        .take(cap.saturating_add(1))
        .read_to_end(&mut body)
        .map_err(|e| format!("fetch-url: reading the response from {url} failed: {e}"))?;
    if body.len() as u64 > cap {
        return Err(format!(
            "fetch-url: the response from {url} is larger than the {cap}-byte limit your IT \
             department has set for this assistant"
        ));
    }
    Ok(body)
}

/// A fixed-window per-minute counter, shared by a process tree (the trunk
/// and every fork) — not machine-wide: two fleets running on one machine
/// each get their own budget.
#[derive(Clone)]
pub struct FetchLimiter(Arc<Mutex<Window>>);

struct Window {
    started: Instant,
    count: u32,
    cap: u32,
    span: Duration,
}

impl FetchLimiter {
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
        let mut w = self.0.lock().expect("fetch limiter poisoned");
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

impl PartialEq for FetchLimiter {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// The durable, append-only `fetch-url` ledger.
///
/// One compact JSON object per line (NDJSON), never `AgentLog`'s
/// pretty-printed multi-line JSON — an append-mode file's single `write()`
/// places its bytes atomically at the file's current end, so concurrent
/// exarch/synod processes sharing one machine can append to the same file
/// without interleaving each other's records, provided each record is
/// written in one syscall. Every fetch is recorded, allowed or refused,
/// before the desk answers.
#[derive(Clone)]
pub struct AuditLog(Arc<Mutex<std::fs::File>>);

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:fetch-audit] opens the per-installation fetch-url audit ledger under XDG state, appended to once per fetch; infra bookkeeping, not turn-time model data I/O."
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

    /// Open this app's ledger at `$XDG_STATE_HOME/<app>/fetch-audit.jsonl`,
    /// once per process, append-mode.
    ///
    /// # Errors
    /// Returns `Err` if the ledger cannot be created or opened.
    pub fn open(app: crate::bootstrap::App) -> std::io::Result<Self> {
        let path = app
            .xdg_dir(ral_core::path::basedir::XdgKind::State)
            .join("fetch-audit.jsonl");
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

    /// Append one record: `url`, the host it resolved to (blank if `url`
    /// never parsed), whether the fetch was allowed, the body size on
    /// success, and a note on refusal. A poisoned lock (only reachable after
    /// a prior panic mid-write) drops the record rather than panicking again
    /// — losing one audit line is the honest outcome, not a second crash.
    pub fn record(
        &self,
        url: &str,
        host: &str,
        allowed: bool,
        bytes: Option<u64>,
        note: Option<&str>,
    ) {
        let mut line = serde_json::json!({
            "ts": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            "url": url,
            "host": host,
            "allowed": allowed,
            "bytes": bytes,
            "note": note,
        })
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
    pub policy: Arc<crate::org_policy::OrgPolicy>,
    pub audit: AuditLog,
    pub limiter: FetchLimiter,
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
        let policy = crate::org_policy::load()?;
        let limiter = FetchLimiter::new(policy.rate_per_minute);
        Ok(Self {
            policy: Arc::new(policy),
            audit: AuditLog::open(app).map_err(|e| format!("fetch-url audit ledger: {e}"))?,
            limiter,
        })
    }

    /// A test double: a permissive policy (fixed for the identity-seat and
    /// wire-seat tests that use it), a throwaway ledger under a fresh
    /// per-call path, and a generous rate budget.
    #[must_use]
    pub fn for_test() -> Self {
        let policy = crate::org_policy::OrgPolicy {
            allow: vec!["example.com".to_string(), "*.example.org".to_string()],
            max_response_bytes: 1_048_576,
            rate_per_minute: 1_000,
        };
        let limiter = FetchLimiter::new(policy.rate_per_minute);
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

    #[test]
    fn validate_fetch_url_accepts_https() {
        let url = validate_fetch_url("https://example.com/path").expect("valid https url");
        assert_eq!(url.host_str(), Some("example.com"));
    }

    #[test]
    fn validate_fetch_url_rejects_a_non_http_scheme() {
        let err = validate_fetch_url("file:///etc/passwd").unwrap_err();
        assert!(err.contains("'file' scheme"), "got: {err}");
        assert!(err.contains("only http and https"), "got: {err}");
    }

    #[test]
    fn validate_fetch_url_rejects_a_malformed_url() {
        let err = validate_fetch_url("not a url").unwrap_err();
        assert!(err.contains("not a valid URL"), "got: {err}");
    }

    /// N requests within the window succeed, the next refuses, and a
    /// later request succeeds again once the window has rolled — polled
    /// with a generous deadline rather than a single fixed sleep, since
    /// this host's timing jitter is high.
    #[test]
    fn fetch_limiter_refuses_over_cap_then_resets_after_its_window() {
        let limiter = FetchLimiter::with_window(2, Duration::from_millis(200));
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
    /// lines — an allowed fetch and a blocked one alike.
    #[test]
    fn audit_log_records_two_independently_parseable_lines() {
        let path = tmp_path("audit-two-lines");
        let _ = std::fs::remove_file(&path);
        let log = AuditLog::for_test(&path);
        log.record("https://a.example/", "a.example", true, Some(12), None);
        log.record(
            "https://b.example/",
            "b.example",
            false,
            None,
            Some("refused: not on the allowlist"),
        );
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
