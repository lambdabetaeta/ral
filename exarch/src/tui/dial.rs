//! The disclosure dial: how much of a dialable part is shown.

/// How much of a dialable part is disclosed, low to high; `Ord` compares the
/// rungs.  A run of `ral` work and a diff reach [`Self::Tally`]; a
/// deliberation and an act floor at [`Self::Summary`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum Detail {
    /// The numbers alone: a run's `|>` effects counted on one line, a diff's
    /// header with its size bar and grain.
    Tally,
    /// The representative slice: the run's live tip, a deliberation's header,
    /// an act's row, a diff's first [`super::diff::DIFF_PEEK_ROWS`] rows.
    Summary,
    /// The whole thing: every call with its source, the whole deliberation,
    /// the whole payload, every hunk.
    Full,
}

/// A dialable part's rung, and the floor a cycle past [`Detail::Full`] wraps to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Dial {
    pub(super) at: Detail,
    pub(super) floor: Detail,
}

impl Dial {
    /// One rung up, the ceiling wrapping to the floor, so a dial walks every
    /// reachable rung rather than toggling the extremes.
    pub(super) fn cycle(&mut self) {
        self.at = match self.at {
            Detail::Tally => Detail::Summary,
            Detail::Summary => Detail::Full,
            Detail::Full => self.floor,
        };
    }
}

/// One of a [`super::block::Group`]'s two dialable parts — the `∴` deliberation, or the `▸`
/// run of work.  Every other block has the one dial, which answers to
/// [`Self::Run`].
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) enum Part {
    Thinking = 0,
    #[default]
    Run = 1,
}
