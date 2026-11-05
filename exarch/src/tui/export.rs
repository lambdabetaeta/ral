//! A session's `record.jsonl`, rendered as the scrollback read — for `/export`
//! and `exarch export`.  The log is replayed through the scrollback's own
//! [`Mirror`], and each block it lets go is handed to a [`Render`]er.

use super::block::Block;
use super::dial::Detail;
use super::mirror::Mirror;
use super::palette::AgentSlot;
use super::text::is_blank;
use crate::record::{Fold, Record, Recorded, Refusal, Snapshot};
use crate::site::{Silent, site};
use ral_core::own::create_named;
use std::io::{self, Write};
use std::path::Path;

/// The formats a session renders to; HTML joins here.
#[derive(Clone, Copy)]
pub enum Format {
    Plain,
}

trait Render {
    fn block(&mut self, block: &Block, agent: AgentSlot) -> io::Result<()>;
}

/// Plain text: each block forced whole — the script, diff and prose are on the
/// record even while reduced on screen — with blank runs collapsed as the
/// screen collapses them.
struct Plain<'a> {
    out: &'a mut dyn Write,
    prev_blank: bool,
}

impl Render for Plain<'_> {
    fn block(&mut self, block: &Block, agent: AgentSlot) -> io::Result<()> {
        for row in block.log_rows(agent) {
            let line = row.into_line();
            let blank = is_blank(&line);
            if blank && self.prev_blank {
                continue;
            }
            self.prev_blank = blank;
            for span in &line.spans {
                self.out.write_all(span.content.as_bytes())?;
            }
            self.out.write_all(b"\n")?;
        }
        Ok(())
    }
}

/// The replay's memo.  The first render error is kept and nothing renders
/// after it, since a half-written transcript is no worse for being shorter.
struct Export<'a> {
    mirror: Mirror,
    render: Box<dyn Render + 'a>,
    failed: Option<io::Error>,
}

impl Fold for Export<'_> {
    type Memo = Self;

    fn step(memo: &mut Self, record: Recorded<Record>) -> Result<(), Refusal> {
        let (_, gone) = memo.mirror.try_step(&record)?;
        if memo.failed.is_none() {
            memo.failed = gone
                .iter()
                .try_for_each(|block| memo.render.block(block, memo.mirror.agent))
                .err();
        }
        Ok(())
    }
}

/// Render a certified, bounded transcript to `out` as the scrollback reads.
///
/// # Errors
/// If the log will not read back, or `out` will not take the text.
pub(crate) fn export(snapshot: &Snapshot, out: &mut dyn Write, format: Format) -> io::Result<()> {
    let mut out = io::BufWriter::new(out);
    {
        let render: Box<dyn Render + '_> = match format {
            Format::Plain => Box::new(Plain {
                out: &mut out,
                prev_blank: true,
            }),
        };
        let mut memo = Export {
            mirror: Mirror::new(AgentSlot::default(), Detail::Summary),
            render,
            failed: None,
        };
        for record in snapshot.records()? {
            Export::step(&mut memo, record?).map_err(io::Error::other)?;
        }
        let Export {
            mirror,
            mut render,
            failed,
        } = memo;
        if let Some(error) = failed {
            return Err(error);
        }
        let agent = mirror.agent;
        for block in mirror.finish() {
            render.block(&block, agent)?;
        }
        drop(render);
    }
    out.flush()
}

/// [`export`] into a new file at `dest`; an existing one is never overwritten.
///
/// # Errors
/// As [`export`], or if `dest` exists or cannot be created.
pub(crate) fn export_to(snapshot: &Snapshot, dest: &Path, format: Format) -> io::Result<()> {
    export(
        snapshot,
        &mut site!(Silent::ExportDestination, create_named(dest))?,
        format,
    )
}

/// Resolve beneath a known application root, then lock before opening the source.
pub(crate) fn inactive_snapshot(target: &Path) -> io::Result<(Snapshot, crate::app::RunLock)> {
    use crate::app::APPS;
    use ral_core::path::XdgKind;
    let session_path = session_target(target)?;
    for app in APPS {
        let root = app.own(XdgKind::State)?;
        match root.within(session_path) {
            Ok(session) => return inactive_under(&root, &session),
            Err(ral_core::own::OwnError::Outside { .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(io::Error::other(UNMANAGED))
}

fn session_target(target: &Path) -> io::Result<&Path> {
    if target
        .file_name()
        .is_some_and(|name| name == crate::app::RECORD)
    {
        ral_core::path::spelled_parent(target).ok_or_else(|| io::Error::other(UNMANAGED))
    } else {
        Ok(target)
    }
}

fn inactive_under(
    root: &ral_core::own::OwnDir,
    held: &ral_core::own::OwnDir,
) -> io::Result<(Snapshot, crate::app::RunLock)> {
    use crate::app::{RECORD, RunLock};
    use crate::record::AgentId;
    let relative = ral_core::path::relative_key(
        &ral_core::path::spelled(held.path()).map_err(io::Error::other)?,
        &ral_core::path::spelled(root.path()).map_err(io::Error::other)?,
    )
    .ok_or_else(|| io::Error::other(UNMANAGED))?;
    let parts: Vec<_> = relative.split('/').collect();
    let [project_name, run_name, "sessions", session_name] = parts.as_slice() else {
        return Err(io::Error::other(UNMANAGED));
    };
    let id = session_name
        .parse::<u64>()
        .map_err(|_| io::Error::other(UNMANAGED))?;
    if id.to_string() != *session_name {
        return Err(io::Error::other(UNMANAGED));
    }
    let project = root.dir(project_name)?;
    let run = project.dir(run_name)?;
    let lock = RunLock::try_existing(&run).map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            io::Error::new(
                error.kind(),
                "this run is active; use /export in its running TUI, or end the run and retry",
            )
        } else {
            io::Error::new(
                error.kind(),
                format!("cannot establish the owning run's existing lock: {error}; {UNMANAGED}"),
            )
        }
    })?;
    #[cfg(test)]
    AFTER_EXPORT_LOCK.with(|hook| {
        if let Some(hook) = hook.take() {
            hook();
        }
    });
    let sessions = run.dir("sessions")?;
    let session = sessions.dir(session_name)?;
    if !session.same_directory(held) {
        return Err(io::Error::other(
            "the requested directory changed during resolution; export refused; retry",
        ));
    }
    let source = session.path().join(RECORD);
    let file = session.open_update(RECORD).map_err(|error| {
        io::Error::new(error.kind(), format!(
            "cannot establish durability for {}: {error}; inactive export requires an existing regular file with read/write permission",
            ral_core::path::shown(&source),
        ))
    })?;
    let snapshot = Snapshot::inactive(file, &source, AgentId::new(id))?;
    session.sync()?;
    sessions.sync()?;
    run.sync()?;
    project.sync()?;
    Ok((snapshot, lock))
}

const UNMANAGED: &str = "standalone export requires a verified inactive managed session: no owning-run lock and durability witness could be established; use a session directory or record.jsonl under the application's state directory, or /export in its running TUI";

#[cfg(test)]
thread_local! {
    static AFTER_EXPORT_LOCK: std::cell::Cell<Option<Box<dyn FnOnce()>>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
