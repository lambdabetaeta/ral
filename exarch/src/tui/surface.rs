//! Grouping accumulator for surfaced content that would otherwise land as
//! many one-off blocks: [`PatchBuf`] coalesces consecutive same-`(id, path)`
//! diff hunks into one `▎ diff` block, [`ObservationBuf`] buckets consecutive
//! read/exec/grep events by kind.  Both are session-keyed, so a session
//! change flushes the live buffer and opens a fresh one rather than merging
//! across sessions.  Kept as two independent buffers, not one, since they
//! group on different keys and flush in a fixed relative order
//! ([`SurfaceBuffer::flush_surfaces`]).  Every other content path funnels
//! through `with_viewport` (`app.rs`), which commits both before handing the
//! caller a live [`Viewport`] — so a pending group always lands before
//! whatever follows it on the rail.

use std::collections::HashMap;

use super::viewport::Viewport;
use crate::bus::AgentId;
use crate::bus::card::{Hunk, IoEvent, ObservationKind};

pub(super) struct SurfaceBuffer {
    patch_buf: Option<PatchBuf>,
    observation_buf: Option<ObservationBuf>,
}

/// Accumulator backing [`SurfaceBuffer::patch_buf`].
struct PatchBuf {
    id: AgentId,
    path: String,
    hunks: Vec<Hunk>,
}

/// Accumulator backing [`SurfaceBuffer::observation_buf`].  Buckets consecutive observation
/// surfaces (read / exec / grep) by kind, deduped and order-independent (the
/// user does not care about interleave order); flushed as one block per
/// non-empty bucket.  The exec/grep buckets keep the typed [`IoEvent`] rather
/// than pre-rendered spans so flush-time rendering can reuse the exact
/// `io_card` span idioms via [`crate::bus::card`]'s per-kind group helpers.  Writes
/// are not buffered — a write is a barrier landed standalone as its own card.
struct ObservationBuf {
    id: AgentId,
    /// Read paths, first-seen order, deduped.
    reads: Vec<String>,
    /// `Exec` events, deduped by `argv`.
    execs: Vec<IoEvent>,
    /// `Grep` events, deduped by `(scope, pattern)`.
    greps: Vec<IoEvent>,
}

impl SurfaceBuffer {
    pub(super) fn new() -> Self {
        Self {
            patch_buf: None,
            observation_buf: None,
        }
    }
    /// Absorb a single-`diff` card's hunks into [`SurfaceBuffer::patch_buf`], or
    /// flush + open a fresh buffer when the path or session changes.
    /// Consecutive same-`(id, path)` diff cards append their hunks into one
    /// buffer so they later render as a single `▎ diff <path>` block of
    /// located hunks — the way a unified diff presents several changes to
    /// one file.
    pub(super) fn absorb_patch(
        &mut self,
        viewports: &mut HashMap<AgentId, Viewport>,
        id: AgentId,
        path: String,
        hunks: Vec<Hunk>,
    ) {
        let same = self
            .patch_buf
            .as_ref()
            .is_some_and(|b| b.id == id && b.path == path);
        if same {
            let buf = self.patch_buf.as_mut().expect("same-path implies Some");
            buf.hunks.extend(hunks);
        } else {
            self.flush_surfaces(viewports);
            self.patch_buf = Some(PatchBuf { id, path, hunks });
        }
    }

    /// Commit any pending [`PatchBuf`] as one `▎ diff` block.  Called at
    /// every commit boundary that isn't another single-`diff` card
    /// targeting the same `(id, path)`: the `push_chrome`-like paths,
    /// the streaming token / boundary paths, session death, and `/clear`.
    fn flush_patch_buf(&mut self, viewports: &mut HashMap<AgentId, Viewport>) {
        let Some(buf) = self.patch_buf.take() else {
            return;
        };
        if let Some(vp) = viewports.get_mut(&buf.id) {
            vp.push_patch(buf.path, buf.hunks);
        }
    }

    /// Bucket an observation `event` (read / exec / grep) into [`SurfaceBuffer::observation_buf`]
    /// by kind, deduped and order-independent (the user does not care about
    /// interleave order).  A session change flushes the in-flight buffer and
    /// opens a fresh one, so a cross-session burst never merges two sessions'
    /// surfaces into one block.  Unlike the `with_viewport` path, this
    /// accumulates directly: the shared [`SurfaceBuffer::flush_surfaces`] boundary is
    /// what would flush the very buffer being filled, so routing through it
    /// would defeat the grouping.  Writes never arrive here — the [`Kind::Io`]
    /// arm lands a write standalone as its own card, never buffered.
    pub(super) fn absorb_observation(
        &mut self,
        viewports: &mut HashMap<AgentId, Viewport>,
        id: AgentId,
        event: IoEvent,
    ) {
        if self.observation_buf.as_ref().is_some_and(|b| b.id != id) {
            self.flush_observations(viewports);
        }
        let buf = self.observation_buf.get_or_insert_with(|| ObservationBuf {
            id,
            reads: Vec::new(),
            execs: Vec::new(),
            greps: Vec::new(),
        });
        match event {
            IoEvent::Read { path } => {
                if !buf.reads.contains(&path) {
                    buf.reads.push(path);
                }
            }
            IoEvent::Exec {
                argv,
                outcome,
                status,
            } => {
                let dup = buf
                    .execs
                    .iter()
                    .any(|e| matches!(e, IoEvent::Exec { argv: a, .. } if *a == argv));
                if !dup {
                    buf.execs.push(IoEvent::Exec {
                        argv,
                        outcome,
                        status,
                    });
                }
            }
            grep @ IoEvent::Grep { .. } => {
                if !buf.greps.contains(&grep) {
                    buf.greps.push(grep);
                }
            }
            IoEvent::Write { .. } => {
                unreachable!("a write is landed as its own card, never bucketed")
            }
        }
    }

    /// Commit any pending [`ObservationBuf`] as one block *per non-empty kind*, in a
    /// fixed Read → Exec → Grep order, reusing the exact `io_card` span idioms
    /// via [`crate::bus::card`]'s per-kind group helpers.  No-op when the buffer is
    /// empty.  Called at every commit boundary that isn't another io surface in
    /// the same session, through the shared [`SurfaceBuffer::flush_surfaces`].
    #[allow(
        clippy::cast_possible_truncation,
        reason = "buffered-observation count for the run census"
    )]
    fn flush_observations(&mut self, viewports: &mut HashMap<AgentId, Viewport>) {
        let Some(buf) = self.observation_buf.take() else {
            return;
        };
        if let Some(vp) = viewports.get_mut(&buf.id) {
            use crate::bus::card::{execs_card, greps_card, reads_card};
            if let Some(card) = reads_card(&buf.reads) {
                vp.push_observation_card(card, ObservationKind::Read, buf.reads.len() as u32);
            }
            if let Some(card) = execs_card(&buf.execs) {
                vp.push_observation_card(card, ObservationKind::Exec, buf.execs.len() as u32);
            }
            if let Some(card) = greps_card(&buf.greps) {
                vp.push_observation_card(card, ObservationKind::Grep, buf.greps.len() as u32);
            }
        }
    }

    /// The shared external commit boundary: flush both grouping buffers, io
    /// first so an io group lands before any diff that the same boundary
    /// commits.  Every non-io, non-diff surface funnels here (the
    /// `with_viewport` chokepoint, plus session death, the streaming
    /// token, and the turn boundary), so the two separate buffers — keyed
    /// differently, never generalised into one — share only this boundary.
    pub(super) fn flush_surfaces(&mut self, viewports: &mut HashMap<AgentId, Viewport>) {
        self.flush_observations(viewports);
        self.flush_patch_buf(viewports);
    }
    pub(super) fn clear(&mut self) {
        self.patch_buf = None;
        self.observation_buf = None;
    }
}
