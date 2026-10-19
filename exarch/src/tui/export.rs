//! A session's `record.jsonl`, rendered as the scrollback read — for `/export`
//! and `exarch export`.  The log is replayed through the scrollback's own
//! [`Mirror`], and each block it lets go is handed to a [`Render`]er.

use super::block::Block;
use super::dial::Detail;
use super::mirror::Mirror;
use super::palette::AgentSlot;
use super::text::is_blank;
use crate::record::{Fold, Record, Recorded, Refusal, replay};
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
        let (_, gone) = memo.mirror.step(&record);
        if memo.failed.is_none() {
            memo.failed = gone
                .iter()
                .try_for_each(|block| memo.render.block(block, memo.mirror.agent))
                .err();
        }
        Ok(())
    }
}

/// Render `record` (a `record.jsonl`) to `out` as the scrollback reads.
///
/// # Errors
/// If the log will not read back, or `out` will not take the text.
pub fn export(record: &Path, out: &mut dyn Write, format: Format) -> io::Result<()> {
    let mut out = io::BufWriter::new(out);
    let render: Box<dyn Render + '_> = match format {
        Format::Plain => Box::new(Plain {
            out: &mut out,
            prev_blank: true,
        }),
    };
    let memo = Export {
        mirror: Mirror::new(AgentSlot::default(), Detail::Summary),
        render,
        failed: None,
    };
    let Export {
        mirror,
        mut render,
        failed,
    } = replay::<Export>(record, memo).map_err(io::Error::other)?;
    if let Some(error) = failed {
        return Err(error);
    }
    let agent = mirror.agent;
    for block in mirror.finish() {
        render.block(&block, agent)?;
    }
    drop(render);
    out.flush()
}

/// [`export`] into a new file at `dest`; an existing one is never overwritten.
///
/// # Errors
/// As [`export`], or if `dest` exists or cannot be created.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:export] creates the user-chosen transcript file; output infra, not turn-time data I/O"
)]
pub fn export_to(record: &Path, dest: &Path, format: Format) -> io::Result<()> {
    export(record, &mut std::fs::File::create_new(dest)?, format)
}

#[cfg(test)]
mod tests;
