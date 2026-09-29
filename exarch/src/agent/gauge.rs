//! Standing conditions: a reading climbs a ladder of rungs, each told once
//! per excursion; a fall below a rung re-arms it.

pub(crate) mod ration;

/// The level a gauge's reading has last been weighed at.
#[derive(Default)]
pub(crate) struct Latch(u32);

impl Latch {
    /// The highest rung of `ladder` that `level` newly reaches, recorded; a
    /// lower level lowers the latch, re-arming the rungs above it.
    pub(crate) fn climb(&mut self, ladder: &[u32], level: u32) -> Option<u32> {
        let told = std::mem::replace(&mut self.0, level);
        ladder
            .iter()
            .copied()
            .rfind(|&rung| told < rung && rung <= level)
    }

    /// A one-rung ladder: whether `over` newly holds.
    fn cross(&mut self, over: bool) -> bool {
        self.climb(&[1], u32::from(over)).is_some()
    }
}

/// A climbed rung, as each audience hears it.
pub(crate) struct Warning {
    pub(crate) user: Option<String>,
    /// The breadcrumb cause and the reminder body.
    pub(crate) model: Option<(String, String)>,
}

/// One stateless reading of the context-pressure gauge, taken by the caller.
/// `Unknown` (a stale token measure under a known window) must neither warn
/// nor re-arm: a stale measure relieves nothing.
pub(crate) enum Pressure {
    Over {
        /// The rendered detail, e.g. "173000 of 200000 tokens".
        detail: String,
        /// The turns the next boundary's cut would take, `None` when nothing
        /// is old enough to shed.
        planned: Option<Vec<u64>>,
    },
    Under,
    Unknown,
}

/// Every ladder an agent climbs, at its latch.
#[derive(Default)]
pub(crate) struct Gauges {
    pressure: Latch,
    disk: Latch,
    pub(crate) ration: ration::Ration,
}

impl Gauges {
    /// The cut is announced before it happens, so the model can leave its
    /// future self a line.
    pub(crate) fn pressure(&mut self, reading: &Pressure) -> Option<Warning> {
        match reading {
            Pressure::Unknown => None,
            Pressure::Under => {
                self.pressure.cross(false);
                None
            }
            Pressure::Over { detail, planned } => self.pressure.cross(true).then(|| Warning {
                user: None,
                model: Some((
                    "context pressure".into(),
                    pressure_message(detail, planned.as_deref()),
                )),
            }),
        }
    }

    pub(crate) fn disk(&mut self, total: u64, ceiling: u64) -> Option<Warning> {
        self.disk.cross(total > ceiling).then(|| Warning {
            user: Some(format!(
                "disk: session log + scratch is {} KiB, over the {} KiB warn ceiling \
                 — forensic records are never rotated or deleted automatically; \
                 clean up by hand",
                total / 1024,
                ceiling / 1024
            )),
            model: None,
        })
    }
}

/// Shown once the gauge crosses its soft line.  With nothing old enough to
/// shed there is no cut to announce, and the reading alone is the whole message.
fn pressure_message(detail: &str, planned: Option<&[u64]>) -> String {
    match planned {
        Some(turns @ [first, ..]) => {
            let runs = crate::record::model::runs(turns);
            let last_plus_one = turns[turns.len() - 1] + 1;
            format!(
                "Context pressure: {detail}. At the next turn boundary, turns {runs} will \
                 leave your context; they stay readable with `exarch-transcript`. To leave your \
                 future self a line, run `exarch-context `evict [turns: !{{range {first} \
                 {last_plus_one}}}, note: '…']` now — a prompt whose exchange is still in \
                 hand stays on its own; otherwise nothing is required of you."
            )
        }
        _ => format!("Context pressure: {detail}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LADDER: &[u32] = &[50, 90];

    fn over() -> Pressure {
        Pressure::Over {
            detail: "400 of 500 tokens".into(),
            planned: Some(vec![1, 2, 3, 4, 5, 6, 7]),
        }
    }

    #[test]
    fn a_latch_tells_each_rung_once_and_skips_one_already_passed() {
        let mut latch = Latch::default();
        assert_eq!(latch.climb(LADDER, 49), None);
        assert_eq!(latch.climb(LADDER, 50), Some(50));
        assert_eq!(latch.climb(LADDER, 70), None);
        assert_eq!(latch.climb(LADDER, 95), Some(90));
        assert_eq!(latch.climb(LADDER, 96), None);
        assert_eq!(
            Latch::default().climb(LADDER, 95),
            Some(90),
            "a first reading past two rungs tells the higher alone"
        );
    }

    #[test]
    fn a_fall_below_a_rung_rearms_it() {
        let mut latch = Latch::default();
        assert_eq!(latch.climb(LADDER, 95), Some(90));
        assert_eq!(latch.climb(LADDER, 60), None);
        assert_eq!(latch.climb(LADDER, 91), Some(90));
        assert_eq!(latch.climb(LADDER, 10), None);
        assert_eq!(latch.climb(LADDER, 55), Some(50));
    }

    /// The reminder carries both the reading and the turns the next boundary
    /// would cut.
    #[test]
    fn pressure_names_the_cut_and_offers_the_note() {
        let (cause, body) = Gauges::default()
            .pressure(&over())
            .and_then(|w| w.model)
            .expect("pressure due should remind");
        assert_eq!(cause, "context pressure");
        assert!(body.contains("400 of 500 tokens"), "{body}");
        assert!(
            body.contains("turns 1–7 will leave your context")
                && body.contains("`exarch-context `evict [turns: !{range 1 8}"),
            "must name the cut and offer the note: {body}"
        );
    }

    /// Nothing old enough to shed: the reading stands alone, with no cut to
    /// announce and no note to offer.
    #[test]
    fn pressure_without_a_planned_cut_states_the_reading_alone() {
        let body = pressure_message("400 of 500 tokens", None);
        assert!(body.contains("400 of 500 tokens"), "{body}");
        assert!(
            !body.contains("evict") && !body.contains("will leave your context"),
            "with no cut planned there is nothing to announce: {body}"
        );
    }

    /// A stale token measure (`Unknown`) neither warns nor re-arms a warning
    /// still owed.
    #[test]
    fn stale_measure_neither_warns_nor_rearms() {
        let mut gauges = Gauges::default();
        assert!(gauges.pressure(&over()).is_some());
        for _ in 0..3 {
            assert!(
                gauges.pressure(&Pressure::Unknown).is_none(),
                "an unknown reading must neither re-fire nor re-arm"
            );
        }
        assert!(
            gauges.pressure(&over()).is_none(),
            "Unknown left the latch told"
        );
        assert!(gauges.pressure(&Pressure::Under).is_none());
        assert!(
            gauges.pressure(&over()).is_some(),
            "a genuine Under reading re-arms; the next Over then fires"
        );
    }

    #[test]
    fn disk_tells_the_user_alone() {
        let mut gauges = Gauges::default();
        assert!(gauges.disk(10 * 1024, 64 * 1024).is_none());
        let warning = gauges
            .disk(2048 * 1024, 64 * 1024)
            .expect("a crossing warns");
        assert!(warning.model.is_none());
        let line = warning.user.expect("the user is told");
        assert!(line.contains("disk") && line.contains("2048 KiB"), "{line}");
    }
}
