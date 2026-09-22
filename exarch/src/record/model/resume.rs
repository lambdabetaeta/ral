//! Resume from disk: quarantine a torn tail, then fold `record.jsonl`
//! into a fresh [`Context`] through the same [`Context::step`] the live
//! path runs.

use super::Context;
use crate::record::log::Log;
use crate::record::{Forensic, Record, Recorded};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// The bytes past the last complete JSONL line — a torn write from a session
/// that did not shut down cleanly.
struct CrashTail {
    bytes: Vec<u8>,
    complete_len: u64,
}

/// Scanned backwards from the end, a window at a time, rather than forwards
/// through the whole file: the tail is one torn record long however long the
/// log behind it is.
const CRASH_SCAN_WINDOW: u64 = 8 * 1024;

#[allow(
    clippy::disallowed_methods,
    reason = "[silent:model-fold-crash-scan] scans record.jsonl's tail for a torn trailing write before resume folds it; output infra, not turn-time data I/O"
)]
fn find_crash_tail(path: &Path) -> io::Result<Option<CrashTail>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut complete_len = 0;
    let mut cursor = len;
    while cursor > 0 {
        let window = cursor.saturating_sub(CRASH_SCAN_WINDOW);
        let mut chunk = vec![0; usize::try_from(cursor - window).unwrap_or(usize::MAX)];
        let _ = file.seek(SeekFrom::Start(window))?;
        file.read_exact(&mut chunk)?;
        if let Some(last) = chunk.iter().rposition(|byte| *byte == b'\n') {
            complete_len = window + last as u64 + 1;
            break;
        }
        cursor = window;
    }
    if complete_len == len {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    let _ = file.seek(SeekFrom::Start(complete_len))?;
    let _ = file.read_to_end(&mut bytes)?;
    Ok(Some(CrashTail {
        bytes,
        complete_len,
    }))
}

/// Move a torn tail to `record.jsonl.crash` and trim the live file back to
/// its last complete line.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:model-fold-crash-quarantine] sidecars and trims a torn record.jsonl tail before resume folds it; output infra, not turn-time data I/O"
)]
fn quarantine_tail(path: &Path, tail: &CrashTail) -> io::Result<()> {
    let mut sidecar_name = path.file_name().unwrap_or_default().to_os_string();
    sidecar_name.push(".crash");
    let sidecar = path.with_file_name(sidecar_name);
    let mut quarantine = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&sidecar)?;
    quarantine.write_all(&tail.bytes)?;
    quarantine.flush()?;
    quarantine.sync_all()?;

    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(tail.complete_len)?;
    file.sync_all()?;
    eprintln!(
        "exarch: quarantined {} bytes from {} in {} before trimming the live log",
        tail.bytes.len(),
        path.display(),
        sidecar.display()
    );
    Ok(())
}

/// Fold `record.jsonl` into a fresh [`Context`]: quarantine a torn tail, read
/// the session's identity off the file's first record, then hand every
/// protocol record to [`Context::step`].
///
/// There is nothing to compare the result with, and nothing to check it
/// against: the law is that the fold refuses what it cannot admit, and what
/// guards a read-back is `read_at`'s digest check, at the moment of the
/// read, where the risk is.
///
/// The pass is a stream rather than a collection, so what a resume holds is
/// the structure it is building and never the log it is building it from.
///
/// Returns the structure alongside the `(model, label)` pair the head record
/// identifies the session by — `AgentLog::resume` reads its own identity from
/// here rather than re-deriving it, since a session's identity is a fact about
/// its first record, not a second thing to keep in step.
///
/// # Errors
/// Returns an error if the file cannot be read, cannot be quarantined, has no
/// `SessionStarted { session_id: 0, parent: None }` head record, or holds a
/// protocol record the fold refuses.
pub fn resume(path: &Path) -> io::Result<(Context, String, String)> {
    if let Some(tail) = find_crash_tail(path)? {
        quarantine_tail(path, &tail)?;
    }
    let mut context = Context::new(path.to_path_buf());
    let mut identity = None;
    for record in Log::read(path)? {
        let record = record?;
        if identity.is_none() {
            identity = Some(match record.value() {
                Record::Forensic(Forensic::SessionStarted {
                    session_id: 0,
                    parent: None,
                    model,
                    label,
                    ..
                }) => (model.clone(), label.clone()),
                Record::Forensic(Forensic::SessionStarted {
                    session_id, parent, ..
                }) => {
                    return Err(io::Error::other(format!(
                        "cannot resume {}: the first record starts session {session_id:?} with parent {parent:?}; expected SessionStarted {{ session_id: 0, parent: None }} — is this a child log?",
                        path.display()
                    )));
                }
                other @ (Record::Protocol(_) | Record::Display(_) | Record::Forensic(_)) => {
                    return Err(io::Error::other(format!(
                        "cannot resume {}: the first record is {other:?}; expected SessionStarted {{ session_id: 0, parent: None }} — is this a copied child log?",
                        path.display()
                    )));
                }
            });
        }
        let Record::Protocol(protocol) = record.value() else {
            continue;
        };
        context
            .step(Recorded::new(record.locus().clone(), protocol.clone()))
            .map_err(|refusal| io::Error::other(refusal.to_string()))?;
    }
    let Some((model, label)) = identity else {
        return Err(io::Error::other(format!(
            "cannot resume {}: no complete session records were found; is the file truncated?",
            path.display()
        )));
    };
    Ok((context, model, label))
}
