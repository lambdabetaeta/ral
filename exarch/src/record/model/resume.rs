//! Resume from disk: quarantine a torn tail, then fold `record.jsonl`
//! into a fresh [`Context`] through the same [`Context::step`] the live
//! path runs.

use super::{Context, Model};
use crate::record::log::Log;
use crate::record::{Fold as _, Forensic, Record, Selection};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Move a torn tail to `record.jsonl.crash` and trim the live file back to
/// its last complete line.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:model-fold-crash-quarantine] sidecars and trims a torn record.jsonl tail before resume folds it; output infra, not turn-time data I/O"
)]
fn quarantine_tail(path: &Path) -> io::Result<()> {
    let complete_len = Log::complete_len(path)?;
    let mut file = File::open(path)?;
    let mut tail = Vec::new();
    let _ = file.seek(SeekFrom::Start(complete_len))?;
    let _ = file.read_to_end(&mut tail)?;
    if tail.is_empty() {
        return Ok(());
    }
    let mut sidecar_name = path.file_name().unwrap_or_default().to_os_string();
    sidecar_name.push(".crash");
    let sidecar = path.with_file_name(sidecar_name);
    let mut quarantine = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&sidecar)?;
    quarantine.write_all(&tail)?;
    quarantine.flush()?;
    quarantine.sync_all()?;

    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(complete_len)?;
    file.sync_all()?;
    ral_core::errln!(
        "exarch: quarantined {} bytes from {} in {} before trimming the live log",
        tail.len(),
        path.display(),
        sidecar.display()
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
/// # Errors
/// Returns an error if the file cannot be read, cannot be quarantined, is not
/// headed by a root's `SessionStarted { parent: None, .. }`, or holds a
/// protocol record the fold refuses.
pub fn resume(path: &Path) -> io::Result<(Context, Selection)> {
    quarantine_tail(path)?;
    let mut context = Context::new(path.to_path_buf());
    let mut selection = Selection::default();
    for (n, record) in Log::read(path)?.enumerate() {
        let record = record?;
        if n == 0 {
            match record.value() {
                Record::Forensic(Forensic::SessionStarted { parent: None, .. }) => {}
                Record::Forensic(Forensic::SessionStarted {
                    session_id, parent, ..
                }) => {
                    return Err(io::Error::other(format!(
                        "cannot resume {}: the first record starts session {session_id} with parent {parent:?}; expected a root, SessionStarted {{ parent: None, .. }}; is this a child log?",
                        path.display()
                    )));
                }
                other @ (Record::Protocol(_) | Record::Display(_) | Record::Forensic(_)) => {
                    return Err(io::Error::other(format!(
                        "cannot resume {}: the first record is {other:?}; expected a root, SessionStarted {{ parent: None, .. }}; is this a copied child log?",
                        path.display()
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
            path.display()
        )));
    }
    Ok((context, selection))
}
