use super::*;

const CAP: usize = 10_000;

#[test]
fn head_tail_keeps_both_ends_aligned_to_newlines() {
    const CAP: usize = 16 * 1024;
    let head = "FIRST_LINE\n".repeat(2000);
    let tail = "LAST_LINE\n".repeat(2000);
    let input = format!("{head}{}{tail}", "X".repeat(50_000));
    let out = head_tail(&input, CAP, "").unwrap();
    assert!(out.contains("FIRST_LINE") && out.contains("LAST_LINE"));
    assert!(out.contains("\n... [elided") && out.contains("] ...\n"));
    // The head drops its trailing newline, so the banner is not preceded
    // by a blank line.
    assert!(!out.contains("\n\n... [elided"));
    assert!(!out.contains(&"X".repeat(1000)));
    assert!(out.len() <= CAP + 64);
}

#[test]
fn handles_utf8_at_cut_boundary() {
    let input = "λ".repeat(20_000);
    assert!(head_tail(&input, 16 * 1024, "").unwrap().contains("elided"));
}

#[test]
fn clip_passes_short_input_through() {
    assert_eq!(clip("hi", 1024), "hi");
}

#[test]
fn clip_elides_oversize_inline_with_nudge() {
    let body = "y".repeat(CAP * 2);
    let out = clip(&body, CAP);
    // Nothing spills to a file the model could read the rest from.
    assert!(out.contains("elided"));
    assert!(out.contains("narrow the output"));
    assert!(!out.contains("full at "));
    assert!(out.len() <= CAP + ELISION_NUDGE.len() + 64);
}

#[test]
fn clip_measures_the_visible_text() {
    let raw = "\x1b[31m".repeat(2000) + "tiny";
    assert!(raw.len() > 1024);
    assert_eq!(clip(&raw, 1024), "tiny");
    let churn = "spinner\r".repeat(100_000) + "done.  ";
    assert!(churn.len() > CAP);
    assert_eq!(clip(&churn, CAP), "done.  ");
}

#[test]
fn clip_strips_ansi_from_oversize_input() {
    let body = format!("\x1b[31m{}\x1b[0m", "y".repeat(CAP * 2));
    let out = clip(&body, CAP);
    assert!(!out.contains('\x1b'));
    assert!(out.contains("elided"));
}
