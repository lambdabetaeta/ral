//! Resume from disk: fold `record.jsonl`, then quarantine a torn tail.

use super::Context;
use crate::record::log::Log;
use crate::record::{
    AgentId, Control, Fold as _, Mail, Model, RECORD, Record, Refusal, Replay, Schedules,
};
use ral_core::own::OwnDir;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// Where a torn tail goes: beside the record it was cut from.
const CRASH: &str = "record.jsonl.crash";

/// Quarantine a torn tail: an unterminated write, or a last line whose sum fails.
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
        "exarch: quarantined a torn last line ({} bytes) from {} in {}, and trimmed the live log to its last whole commit",
        tail.len(),
        ral_core::path::shown(session.path().join(RECORD)),
        ral_core::path::shown(session.path().join(CRASH))
    );
    Ok(())
}

/// Fold `record.jsonl` into a fresh [`Context`], then quarantine a torn tail,
/// so a refused resume leaves the file as it found it. Check the first record
/// heads a root and hand records to [`Context::step`] and the control replay.
///
/// There is nothing to compare the result with, and nothing to check it
/// against: the law is that the fold refuses what it cannot admit, and what
/// guards a read-back is `read_at`'s sum check, at the moment of the
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
pub fn resume(sessions: &OwnDir, id: AgentId) -> io::Result<(Context, (Mail, Schedules, Model))> {
    let session = sessions.dir(&id.to_string())?;
    let path: PathBuf = session.path().join(RECORD);
    let refused = |refusal: Refusal| io::Error::other(refusal.to_string());
    let mut records = Log::read(session.open(RECORD)?, &path)?;
    let head = records.next().transpose()?.ok_or_else(|| {
        io::Error::other(format!(
            "cannot resume {}: no complete session records were found; is the file truncated?",
            ral_core::path::shown(&path)
        ))
    })?;
    let mut replay = Replay::open(&head).map_err(|refusal| match refusal {
        Refusal::Foreign { record, .. } => io::Error::other(format!(
            "cannot resume {}: the first record is {record:?}; expected a root, SessionStarted {{ parent: None, .. }}; is this a copied child log?",
            ral_core::path::shown(&path)
        )),
        Refusal::Unreadable(_) => refused(refusal),
    })?;
    if let Record::Control(Control::SessionStarted(started)) = head.value()
        && started.parent.is_some()
    {
        return Err(io::Error::other(format!(
            "cannot resume {}: the first record starts session {} with parent {:?}; expected a root, SessionStarted {{ parent: None, .. }}; is this a child log?",
            ral_core::path::shown(&path),
            started.session_id,
            started.parent
        )));
    }
    let mut context = Context::new(id, sessions.clone());
    Context::step(&mut context, head).map_err(refused)?;
    for record in records {
        let record = record?;
        replay.step(&record).map_err(refused)?;
        Context::step(&mut context, record).map_err(refused)?;
    }
    quarantine_tail(&session)?;
    Ok((context, replay.finish()))
}
