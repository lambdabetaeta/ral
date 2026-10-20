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
