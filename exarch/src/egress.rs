//! The network policy and the durable trail behind every CONNECT
//! tunnel: opened once per front-end process, host side, and inherited
//! verbatim by every child.
//!
//! `guest-net`'s proxy is the only writer of records.

#[cfg(any(test, feature = "test-util"))]
use ral_core::own::TestDir;
use ral_core::own::{OwnDir, OwnError};
use ral_core::path::XdgKind;
use ral_core::sync::LockExt as _;
use std::fs::File;
use std::io::{self, Write};
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

/// Past this, the next write rotates the live file aside to [`ROTATED`] first.
const ROTATE_AT_BYTES: u64 = 32 * 1024 * 1024;

/// The ledger's name in the app's state directory.
pub const LEDGER: &str = "net-audit.jsonl";

/// What the ledger is renamed to once it outgrows [`ROTATE_AT_BYTES`].
const ROTATED: &str = "net-audit.jsonl.1";

/// The file every process on one ledger locks across the whole
/// check-size/rotate/write sequence, so two racing the size check cannot each
/// rotate and destroy the other's history.
const ROTATION_LOCK: &str = "net-audit.jsonl.lock";

/// The durable, append-only network ledger, one compact JSON object per line:
/// an append-mode `write()` lands atomically at the end, so concurrent
/// processes on one app's ledger never interleave records.
#[derive(Clone)]
pub struct AuditLog(Arc<Mutex<LedgerFile>>);

/// What [`AuditLog`] locks, with poison kept (`lock_strict`): the guard spans
/// the rotation that swaps `file` and the append that follows it, so a panic
/// between them leaves the handle and the bytes on disk disagreeing; and an
/// unauditable proxy must stop proxying rather than resume writing into a
/// ledger nobody can vouch for.
struct LedgerFile {
    /// The directory the ledger and its rotation lock are in.
    state: OwnDir,
    file: File,
    /// Held, between processes as well as threads, across a rotation and the
    /// write after it: on every host, since only a lock on a file reaches
    /// another process.
    rotation: File,
    /// The directory this ledger has to itself, when a test double made it:
    /// dropped with the log, taking the ledger and its rotation lock along.
    /// A real ledger lives under XDG state and owns no directory.
    #[cfg(any(test, feature = "test-util"))]
    _scratch: Option<TestDir>,
}

impl LedgerFile {
    /// The ledger and its rotation lock in `state`, made where absent.
    fn open(state: OwnDir) -> Result<Self, OwnError> {
        let file = state.append(LEDGER)?;
        let rotation = state.open_write(ROTATION_LOCK)?;
        Ok(Self {
            state,
            file,
            rotation,
            #[cfg(any(test, feature = "test-util"))]
            _scratch: None,
        })
    }
}

impl AuditLog {
    /// Open this app's ledger at `$XDG_STATE_HOME/<app>/net-audit.jsonl`.
    ///
    /// # Errors
    /// Returns `Err` if the state directory, the ledger or its rotation lock
    /// cannot be made or opened.
    pub fn open(app: crate::app::App) -> Result<Self, OwnError> {
        Ok(Self::over(LedgerFile::open(app.own(XdgKind::State)?)?))
    }

    fn over(ledger: LedgerFile) -> Self {
        Self(Arc::new(Mutex::new(ledger)))
    }

    /// A ledger in `state`, for a test that reads it back.  The caller owns the
    /// directory, and so decides when the ledger goes.
    ///
    /// # Panics
    /// Panics if the ledger cannot be created.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(state: &OwnDir) -> Self {
        Self::over(LedgerFile::open(OwnDir::clone(state)).expect("test audit ledger"))
    }

    /// A ledger in a directory of its own, for a test double with no reason to
    /// name one.  Both the ledger and its rotation lock go when the log does.
    ///
    /// # Panics
    /// Panics if the directory or the file cannot be created.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn in_scratch() -> Self {
        let scratch = OwnDir::for_test("egress").expect("test ledger scratch");
        let state = OwnDir::clone(&scratch);
        let ledger = LedgerFile {
            _scratch: Some(scratch),
            ..LedgerFile::open(state).expect("test audit ledger")
        };
        Self::over(ledger)
    }

    /// Append one record. `Err` means the gate must close: an unauditable
    /// proxy does not proxy.
    ///
    /// # Errors
    /// Returns `Err` on a poisoned lock, a failed rotation, or a failed write.
    pub fn record(&self, r: Record<'_>) -> io::Result<()> {
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
        let mut ledger = self.0.lock_strict()?;
        let LedgerFile {
            state,
            file,
            rotation,
            ..
        } = &mut *ledger;
        rotation.lock()?;
        let written = (|| -> io::Result<()> {
            if file.metadata()?.len() + line.len() as u64 > ROTATE_AT_BYTES {
                state.rename(LEDGER, ROTATED)?;
                *file = state.append(LEDGER)?;
            }
            file.write_all(line.as_bytes())
        })();
        let unlocked = rotation.unlock();
        drop(ledger);
        written.and(unlocked)
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
    pub fn open(app: crate::app::App) -> Result<Self, String> {
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
    #[cfg(any(test, feature = "test-util"))]
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

    /// [`Self::for_test`] with the provider's own web search denied — the
    /// policy verdict a searchless fixture is built from.
    #[cfg(any(test, feature = "test-util"))]
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
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    /// A ledger directory of this test's own.  The guard must be held:
    /// dropping it deletes the directory, and the ledger, the rotation lock
    /// and any rotated `.jsonl.1` with it.
    fn tmp(tag: &str) -> TestDir {
        OwnDir::for_test(&format!("egress-{tag}")).expect("test ledger scratch")
    }

    /// A ledger as it stands, in lines.
    fn lines_of(state: &OwnDir, name: &str) -> Vec<String> {
        let bytes = state.read(name).expect("ledger readable");
        String::from_utf8(bytes)
            .expect("ledger is text")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn audit_log_records_two_independently_parseable_lines() {
        let state = tmp("audit-two-lines");
        let log = AuditLog::for_test(&state);
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
        let lines = lines_of(&state, LEDGER);
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
        let state = tmp("audit-rotate");
        // Through a write handle, before the ledger opens: an append-mode
        // handle carries no write access on Windows, which refuses `set_len`.
        state
            .open_write(LEDGER)
            .map_err(io::Error::from)
            .and_then(|f| f.set_len(ROTATE_AT_BYTES))
            .expect("oversized ledger");
        let log = AuditLog::for_test(&state);
        log.record(Record::Tunnel {
            host: "a.example",
            addr: None,
            allowed: true,
            note: None,
            up: None,
            down: None,
        })
        .expect("write must succeed after rotation");
        assert!(
            state.stat(ROTATED).is_some(),
            "the oversized file must be rotated aside"
        );
        assert_eq!(
            lines_of(&state, LEDGER).len(),
            1,
            "the fresh file holds only the new record"
        );
    }

    /// Two `AuditLog` handles on one directory stand in for two racing
    /// processes: each has its own mutex, so only the lock on the rotation
    /// file can serialise them.
    #[test]
    fn rotation_under_cross_process_contention_loses_no_record() {
        let state = tmp("audit-race");
        state
            .open_write(LEDGER)
            .map_err(io::Error::from)
            .and_then(|f| f.set_len(ROTATE_AT_BYTES - 1))
            .expect("a ledger one byte short of its cap");
        let log_a = AuditLog::for_test(&state);
        let log_b = AuditLog::for_test(&state);

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

        let mut combined = lines_of(&state, LEDGER).join("\n");
        if state.stat(ROTATED).is_some() {
            combined.push_str(&lines_of(&state, ROTATED).join("\n"));
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
        let state = tmp("audit-poisoned");
        let log = AuditLog::for_test(&state);
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
