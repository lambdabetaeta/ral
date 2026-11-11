use super::*;

/// Decode a `make_command_line` buffer, dropping its trailing NUL.
fn decode(wide: &[u16]) -> String {
    let (last, body) = wide.split_last().expect("non-empty command line");
    assert_eq!(*last, 0, "make_command_line must NUL-terminate");
    String::from_utf16(body).expect("well-formed UTF-16")
}

/// A surrogate pair against the arg boundary and against an escaped
/// embedded quote — the two places careless `u16`-unit walking in the
/// backslash-doubling would split one.
#[test]
fn non_bmp_survives_adjacent_to_escaped_quote() {
    let q = '"';
    let program = std::ffi::OsString::from(r"C:\ral.exe");
    let arg = std::ffi::OsString::from(format!("🎉{q}🎊 quoted 🎊{q}🎉"));
    let cmd = make_command_line(program.as_os_str(), std::slice::from_ref(&arg)).unwrap();
    let decoded = decode(&cmd);
    let expected_payload = format!("🎉\\{q}🎊 quoted 🎊\\{q}🎉");
    assert_eq!(
        decoded,
        format!("\"{}\" \"{expected_payload}\"", program.to_str().unwrap())
    );
}

#[test]
fn a_nul_argument_preserves_its_argv_position() {
    let error =
        make_command_line(OsStr::new("tool"), &["okay".into(), "bad\0arg".into()]).unwrap_err();
    assert_eq!(error.step, crate::fact::PrepareStep::Argv);
    assert_eq!(
        error.subject,
        crate::fact::PrepareSubject::Argument(2usize.into())
    );
    assert_eq!(
        error.cause,
        crate::fact::PrepareCause::Invalid(crate::fact::PrepareInvalid::Nul)
    );
}
