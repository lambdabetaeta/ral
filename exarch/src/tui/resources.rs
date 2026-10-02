//! The `/resources` rows for the accumulators the frontend owns — scrollback,
//! views, the bus — appended to the agent's own card at the render boundary,
//! since only this thread may read them.

use super::DEMOTE_IDLE;
use crate::agent::resources::{Policy, ProbeRow};

/// The probed agent's scrollback: its figures beside the one window that bounds
/// them all, in one struct so a figure cannot drift from its bound.
#[derive(Clone, Copy)]
pub(super) struct ScrollbackFigures {
    /// Scrollback blocks resident, one per reader's atom.
    pub blocks: u64,
    /// Rendered rows those blocks put on screen, as of the last paint.
    pub rows: u64,
    /// Those rows' summed text bytes.
    pub bytes: u64,
    /// `record::BLOCKS_WINDOW` — the view fold's resident-row window, which is
    /// the only thing that bounds the three figures above: a block leaves the
    /// screen when the row it was built from leaves the fold.
    pub window: u64,
}

/// The fleet's view counts: per-agent views the frontend holds, split
/// live/dead, plus the live-agent tab count.
#[derive(Clone, Copy)]
pub(super) struct ViewFigures {
    pub live: u64,
    /// Views whose agent has died — lingering, or already tombstoned down to
    /// (id, status, log path) once past `tui::LINGER`.
    pub dead: u64,
    pub agents: u64,
}

/// The presentation bus's two probe figures.
///
/// Neither carries a cap: the transport's one enforced number is a
/// *per-entry* text cap (`bus::MERGE_TEXT_CAP`), a different axis from either
/// aggregate, so it is named in `bus.bytes`'s note rather than faked into
/// `cap`.
#[derive(Clone, Copy)]
pub(super) struct BusFigures {
    /// Queue entries — a merged run and a reserved kind each count as one.
    pub depth: u64,
    /// Resident merged `Token`/`Thinking` text bytes.  `State` coalesces by
    /// replacement and carries no text, so it weighs nothing here.
    pub bytes: u64,
}

/// The rows for the accumulators the frontend owns.  Pure in its figures, so
/// the row shapes are checkable without a terminal.
pub(super) fn frontend_rows(
    scrollback: ScrollbackFigures,
    views: ViewFigures,
    bus: BusFigures,
) -> Vec<ProbeRow> {
    vec![
        ProbeRow::new(
            "scrollback.blocks",
            scrollback.blocks,
            None,
            Policy::Evict,
            Some(format!(
                "one per reader's atom; bounded by the view fold's {}-row window",
                scrollback.window
            )),
        ),
        ProbeRow::new(
            "scrollback.rows",
            scrollback.rows,
            None,
            Policy::Evict,
            Some("what those blocks render to at the readable width".to_string()),
        ),
        ProbeRow::new(
            "scrollback.bytes",
            scrollback.bytes,
            None,
            Policy::Evict,
            Some("no byte cap of its own; bounded indirectly by the fold's row window".to_string()),
        ),
        ProbeRow::new(
            "views.live",
            views.live,
            None,
            Policy::Unbounded,
            Some("one per live agent".to_string()),
        ),
        ProbeRow::new(
            "views.dead",
            views.dead,
            None,
            Policy::Evict,
            Some("tombstoned (id, status, log path) once past LINGER".to_string()),
        ),
        ProbeRow::new(
            "bus.depth",
            bus.depth,
            None,
            Policy::Coalesce,
            Some("entries; a same-class run off one agent merges into its tail".to_string()),
        ),
        ProbeRow::new(
            "bus.bytes",
            bus.bytes,
            None,
            Policy::Evict,
            Some(format!(
                "resident merged token/thinking/phase text; each run elides past {} KiB",
                crate::bus::MERGE_TEXT_CAP / 1024
            )),
        ),
        ProbeRow::new(
            "fleet.agents",
            views.agents,
            None,
            Policy::Reap,
            Some("the frontend's tab view; the spawn tree is the authority".to_string()),
        ),
        ProbeRow::new(
            "agents.demote",
            DEMOTE_IDLE.as_secs(),
            None,
            Policy::Warn,
            Some("a parked child demotes to a compact matrix row at this idle age".to_string()),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every frontend row wears its policy, and none of them fakes a ceiling:
    /// the view fold's row window is what bounds the scrollback, so the rows it
    /// bounds name it in their note rather than claim a `cap` of their own.
    #[test]
    fn frontend_rows_state_decided_policies_and_name_the_one_window() {
        let rows = frontend_rows(
            ScrollbackFigures {
                blocks: 3,
                rows: 120,
                bytes: 4096,
                window: 1000,
            },
            ViewFigures {
                live: 2,
                dead: 1,
                agents: 2,
            },
            BusFigures {
                depth: 5,
                bytes: 777,
            },
        );
        let by_name = |n: &str| {
            rows.iter()
                .find(|r| r.name == n)
                .unwrap_or_else(|| panic!("row {n} must be emitted"))
        };
        assert_eq!(by_name("scrollback.blocks").current, 3);
        assert_eq!(by_name("scrollback.rows").current, 120);
        assert_eq!(by_name("scrollback.bytes").current, 4096);
        assert_eq!(by_name("views.live").current, 2);
        assert_eq!(by_name("views.dead").current, 1);
        assert_eq!(by_name("fleet.agents").current, 2);
        assert_eq!(by_name("bus.depth").current, 5);
        assert_eq!(by_name("bus.bytes").current, 777);
        assert!(
            by_name("scrollback.blocks")
                .note
                .as_deref()
                .is_some_and(|n| n.contains("1000-row window")),
            "the blocks row names the one window that bounds it"
        );
        assert!(
            rows.iter().all(|row| row.cap.is_none()),
            "no frontend row enforces a cap of its own"
        );
        assert_eq!(by_name("scrollback.blocks").policy, Policy::Evict);
        assert_eq!(by_name("bus.depth").policy, Policy::Coalesce);
        assert!(
            by_name("bus.bytes")
                .note
                .as_deref()
                .is_some_and(|n| n.contains("KiB")),
            "the bus bytes row must name the per-run elision cap"
        );
    }
}
