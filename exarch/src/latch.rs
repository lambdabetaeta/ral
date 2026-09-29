//! A reading climbs a ladder of rungs, each told once per excursion; a fall
//! below a rung re-arms it.

/// The level a reading has last been weighed at.
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
    pub(crate) fn cross(&mut self, over: bool) -> bool {
        self.climb(&[1], u32::from(over)).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LADDER: &[u32] = &[50, 90];

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
}
