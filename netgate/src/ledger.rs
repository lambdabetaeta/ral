//! The proxy alone writes the durable CONNECT ledger shared by its workers.

#[cfg(any(test, feature = "test-util"))]
use ral_core::own::TestDir;
use ral_core::own::{OwnDir, OwnError};
use ral_core::sync::LockExt as _;
use std::fs::File;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

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

const ROTATE_AT_BYTES: u64 = 32 * 1024 * 1024;

pub const LEDGER: &str = "net-audit.jsonl";

const ROTATED: &str = "net-audit.jsonl.1";

const ROTATION_LOCK: &str = "net-audit.jsonl.lock";

/// Append-only JSON lines; rotation and append are locked across processes.
#[derive(Clone)]
pub struct AuditLog(Arc<Mutex<LedgerFile>>);

/// Strict poison: a panic during rotation leaves the file handle untrustworthy.
struct LedgerFile {
    state: OwnDir,
    file: File,
    /// Held across rotation and append, between processes as well as threads.
    rotation: File,
    /// Keeps a test ledger's directory alive until its files close.
    #[cfg(any(test, feature = "test-util"))]
    _scratch: Option<TestDir>,
}

impl LedgerFile {
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
    /// # Errors
    /// Reports failure to open the ledger or its rotation lock.
    pub fn open(state: OwnDir) -> Result<Self, OwnError> {
        Ok(Self::over(LedgerFile::open(state)?))
    }

    fn over(ledger: LedgerFile) -> Self {
        Self(Arc::new(Mutex::new(ledger)))
    }

    /// # Panics
    /// If the ledger cannot be created; the caller must keep `state` alive.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(state: &OwnDir) -> Self {
        Self::over(LedgerFile::open(OwnDir::clone(state)).expect("test audit ledger"))
    }

    /// # Panics
    /// If the scratch directory or ledger cannot be created.
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

    /// # Errors
    /// Poison, rotation or append failure must close the gate.
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

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> TestDir {
        OwnDir::for_test(&format!("ledger-{tag}")).expect("test ledger scratch")
    }

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

    // Independent mutexes leave only the rotation file lock to serialise writers.
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
