use super::*;

fn tr(stdout: &str, stderr: &str, value: Option<&str>, exit: i32) -> shell_eval::ToolResult {
    shell_eval::ToolResult {
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
        value: value.map(str::to_string),
        exit,
    }
}

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
fn render_keeps_canonical_section_order() {
    let r = tr("abc\n", "err\n", Some("v"), 0);
    assert_eq!(
        render(&r),
        "STDOUT:\nabc\n\nSTDERR:\nerr\n\nVALUE:\nv\n\nEXIT: 0",
    );
}

#[test]
fn render_strips_ansi_from_streams() {
    let r = tr("\x1b[31mred\x1b[0m\n", "\x1b[1;33mwarn\x1b[0m\n", None, 1);
    let out = render(&r);
    assert!(out.contains("STDOUT:\nred\n"));
    assert!(out.contains("STDERR:\nwarn\n"));
    assert!(!out.contains('\x1b'));
}

#[test]
fn render_caps_each_section_independently() {
    let stdout = "noise\n".repeat(20_000);
    let stderr = "ERROR: division by zero at line 42\n".to_string();
    let out = render(&tr(&stdout, &stderr, None, 1));
    assert!(out.contains(&format!("STDERR:\n{stderr}")));
    assert!(out.contains("elided"));
}

#[test]
fn render_value_gets_more_room_than_stdout() {
    // 12_000 sits between the two caps.
    let body = "x".repeat(12_000);
    assert!(!render(&tr("", "", Some(&body), 0)).contains("elided"));
    assert!(render(&tr(&body, "", None, 0)).contains("elided"));
}

#[test]
fn clip_passes_short_input_through() {
    assert_eq!(clip("hi", 1024), "hi");
}

#[test]
fn clip_elides_oversize_inline_with_nudge() {
    let body = "y".repeat(STDOUT_CAP * 2);
    let out = clip(&body, STDOUT_CAP);
    // Nothing spills to a file the model could read the rest from.
    assert!(out.contains("elided"));
    assert!(out.contains("narrow the output"));
    assert!(!out.contains("full at "));
    assert!(out.len() <= STDOUT_CAP + ELISION_NUDGE.len() + 64);
}

#[test]
fn clip_measures_the_visible_text() {
    let raw = "\x1b[31m".repeat(2000) + "tiny";
    assert!(raw.len() > 1024);
    assert_eq!(clip(&raw, 1024), "tiny");
    let churn = "spinner\r".repeat(100_000) + "done.  ";
    assert!(churn.len() > STDOUT_CAP);
    assert_eq!(clip(&churn, STDOUT_CAP), "done.  ");
}

#[test]
fn clip_strips_ansi_from_oversize_input() {
    let body = format!("\x1b[31m{}\x1b[0m", "y".repeat(STDOUT_CAP * 2));
    let out = clip(&body, STDOUT_CAP);
    assert!(!out.contains('\x1b'));
    assert!(out.contains("elided"));
}
