use super::*;

#[test]
fn percentages_keep_their_unit_and_bars_show_the_same_consumed_share() {
    for value in [0, 50, 100] {
        let r = CardReadout {
            value,
            max: Some(100),
            unit: Some("% used".into()),
        };
        let spans = readout_spans(&r, Col::default());
        assert_eq!(spans[0].content, format!("{value}% used"));
        assert_eq!(spans[2].content, "█".repeat(value as usize / 10));
        assert_eq!(spans[3].content, "░".repeat(10 - value as usize / 10));
        assert_eq!(spans[2].style.fg, Some(LIME));
        assert_eq!(spans[3].style.fg, Some(SLATE));
    }
}
