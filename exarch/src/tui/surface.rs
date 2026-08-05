//! Two grouping accumulators for surfaced content that would otherwise land as
//! many one-off blocks: [`PatchBuf`] coalesces consecutive same-`(id, path)`
//! diff hunks into one `▎ diff`, [`ObservationBuf`] buckets read/exec/grep
//! events by kind.  Both are session-keyed, so a session change flushes and
//! reopens rather than merging two sessions into one block.  `with_viewport`
//! in `app.rs` flushes both before handing out a live [`Viewport`], so a
//! pending group always lands before whatever follows it on the rail.

use std::collections::HashMap;

use super::viewport::Viewport;
use crate::bus::AgentId;
use crate::bus::card::{Hunk, ObservationKind};
use ral_core::types::Observed;

pub(super) struct SurfaceBuffer {
    patch_buf: Option<PatchBuf>,
    observation_buf: Option<ObservationBuf>,
}

struct PatchBuf {
    id: AgentId,
    path: String,
    hunks: Vec<Hunk>,
}

/// Buckets are deduped and order-independent — the user does not care in what
/// order a burst interleaved.  Holding the typed [`Observed`] fact rather than
/// rendered spans lets flush reuse [`crate::bus::card`]'s group helpers; the
/// envelope (site, time, principal) carries nothing a group needs, so only the
/// fact is kept.
struct ObservationBuf {
    id: AgentId,
    reads: Vec<String>,
    execs: Vec<Observed>,
    greps: Vec<Observed>,
}

impl SurfaceBuffer {
    pub(super) fn new() -> Self {
        Self {
            patch_buf: None,
            observation_buf: None,
        }
    }
    /// Extend the open buffer on a matching `(id, path)`, else flush and reopen,
    /// so consecutive edits to one file render as one `▎ diff <path>` block.
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

    fn flush_patch_buf(&mut self, viewports: &mut HashMap<AgentId, Viewport>) {
        let Some(buf) = self.patch_buf.take() else {
            return;
        };
        if let Some(vp) = viewports.get_mut(&buf.id) {
            vp.push_patch(buf.path, buf.hunks);
        }
    }

    /// Bucket a read/exec/grep, flushing first on a session change.  Not routed
    /// through `with_viewport`, whose flush would empty the buffer this fills.
    pub(super) fn absorb_observation(
        &mut self,
        viewports: &mut HashMap<AgentId, Viewport>,
        id: AgentId,
        event: Observed,
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
            Observed::Read { path } => {
                if !buf.reads.contains(&path) {
                    buf.reads.push(path);
                }
            }
            Observed::Command { ref argv, .. } => {
                let dup = buf
                    .execs
                    .iter()
                    .any(|e| matches!(e, Observed::Command { argv: a, .. } if a == argv));
                if !dup {
                    buf.execs.push(event);
                }
            }
            grep @ Observed::Grep { .. } => {
                if !buf.greps.contains(&grep) {
                    buf.greps.push(grep);
                }
            }
            Observed::Write { .. } => {
                unreachable!("a write is landed as its own card, never bucketed")
            }
            Observed::Capability { .. } => {
                unreachable!("a capability check renders as its own card, never bucketed")
            }
            Observed::Worker { .. } => {
                unreachable!("a worker birth is landed as its own card, never bucketed")
            }
            Observed::Act { .. } => {
                unreachable!("an `Act` never reaches the rail from the engine seam")
            }
        }
    }

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

    /// The shared commit boundary: io first, so an io group lands ahead of a
    /// diff buffered before it.  A group whose viewport is gone is dropped.
    pub(super) fn flush_surfaces(&mut self, viewports: &mut HashMap<AgentId, Viewport>) {
        self.flush_observations(viewports);
        self.flush_patch_buf(viewports);
    }
    /// Discard both buffers unrendered — `/clear` wipes the scrollback they
    /// would have landed in.
    pub(super) fn clear(&mut self) {
        self.patch_buf = None;
        self.observation_buf = None;
    }
}
