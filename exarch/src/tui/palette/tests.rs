use super::*;

#[allow(
    clippy::suboptimal_flops,
    reason = "u8-rounded colour math; mul_add adds no precision and obscures the standard lerp/luma formula"
)]
fn luma(c: Color) -> f32 {
    let Color::Rgb(r, g, b) = c else {
        unreachable!("test colours are RGB")
    };
    0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b)
}

/// The drain holds luminance, keeping the fidelity signal off the lightness
/// channel magnitude rides — within a rounding step, at any `t`.
#[test]
fn desaturate_holds_luminance() {
    for &c in &AGENT_HUES {
        let before = luma(c);
        assert!(
            (luma(desaturate(c, 0.45)) - before).abs() <= 1.0,
            "partial drain shifted luma of {c:?}"
        );
        let full = desaturate(c, 1.0);
        assert!(
            (luma(full) - before).abs() <= 1.0,
            "full drain shifted luma"
        );
        let Color::Rgb(r, g, b) = full else {
            unreachable!()
        };
        assert_eq!((r, r), (g, b), "full drain is grey");
    }
}

#[test]
fn desaturate_zero_is_identity() {
    assert_eq!(desaturate(AGENT_HUES[0], 0.0), AGENT_HUES[0]);
}
