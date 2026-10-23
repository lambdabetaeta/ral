//! Resume from disk: quarantine a torn tail, then fold `record.jsonl`
//! into a fresh [`Context`] through the same [`Context::step`] the live
//! path runs.

use super::{Context, Model};
use crate::record::log::Log;
use crate::record::{AgentId, Fold as _, Forensic, RECORD, Record, Selection};
use ral_core::own::OwnDir;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// Where a torn tail goes: beside the record it was cut from.
const CRASH: &str = "record.jsonl.crash";

/// Move a torn tail to `record.jsonl.crash` and trim the live file back to
/// its last complete line.
fn quarantine_tail(session: &OwnDir) -> io::Result<()> {
    let mut file = session.open(RECORD)?;
    let complete = Log::complete_len(&mut file)?;
    let mut tail = Vec::new();
    let _ = file.seek(SeekFrom::Start(complete))?;
    let _ = file.read_to_end(&mut tail)?;
    if tail.is_empty() {
        return Ok(());
    }
    let mut sidecar = session.append(CRASH)?;
    sidecar.write_all(&tail)?;
    sidecar.flush()?;
    sidecar.sync_all()?;

    let live = session.open_write(RECORD)?;
    live.set_len(complete)?;
    live.sync_all()?;
    ral_core::errln!(
        "exarch: quarantined {} bytes from {} in {} before trimming the live log",
        tail.len(),
        ral_core::path::shown(session.path().join(RECORD)),
        ral_core::path::shown(session.path().join(CRASH))
    );
    Ok(())
}

/// Fold `record.jsonl` into a fresh [`Context`]: quarantine a torn tail, check
/// the file's first record heads a root, then hand every record to
/// [`Model::step`] and every forensic one to a [`Selection`].
///
/// There is nothing to compare the result with, and nothing to check it
/// against: the law is that the fold refuses what it cannot admit, and what
/// guards a read-back is `read_at`'s digest check, at the moment of the
/// read, where the risk is.
///
/// The pass is a stream rather than a collection, so what a resume holds is
/// the structure it is building and never the log it is building it from.
///
/// The session is `id`'s directory under `sessions`, the run's, which every
/// pointer the fold meets is read back from.
///
/// # Errors
/// Returns an error if the file cannot be read, cannot be quarantined, is not
/// headed by a root's `SessionStarted { parent: None, .. }`, or holds a
/// protocol record the fold refuses.
pub fn resume(sessions: &OwnDir, id: AgentId) -> io::Result<(Context, Selection)> {
    let session = sessions.dir(&id.to_string())?;
    let path: PathBuf = session.path().join(RECORD);
    quarantine_tail(&session)?;
    let mut context = Context::new(id, sessions.clone());
    let mut selection = Selection::default();
    for (n, record) in Log::read(session.open(RECORD)?, &path)?.enumerate() {
        let record = record?;
        if n == 0 {
            match record.value() {
                Record::Forensic(Forensic::SessionStarted { parent: None, .. }) => {}
                Record::Forensic(Forensic::SessionStarted {
                    session_id, parent, ..
                }) => {
                    return Err(io::Error::other(format!(
                        "cannot resume {}: the first record starts session {session_id} with parent {parent:?}; expected a root, SessionStarted {{ parent: None, .. }}; is this a child log?",
                        ral_core::path::shown(&path)
                    )));
                }
                other @ (Record::Protocol(_) | Record::Display(_) | Record::Forensic(_)) => {
                    return Err(io::Error::other(format!(
                        "cannot resume {}: the first record is {other:?}; expected a root, SessionStarted {{ parent: None, .. }}; is this a copied child log?",
                        ral_core::path::shown(&path)
                    )));
                }
            }
        }
        if let Record::Forensic(forensic) = record.value() {
            selection.step(forensic);
        }
        Model::step(&mut context, record)
            .map_err(|refusal| io::Error::other(refusal.to_string()))?;
    }
    if selection.model().is_none() {
        return Err(io::Error::other(format!(
            "cannot resume {}: no complete session records were found; is the file truncated?",
            ral_core::path::shown(&path)
        )));
    }
    Ok((context, selection))
}
