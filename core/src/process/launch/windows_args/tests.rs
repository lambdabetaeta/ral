use super::*;

/// Decode a `make_command_line` buffer, dropping its trailing NUL.
fn decode(wide: &[u16]) -> String {
    let (last, body) = wide.split_last().expect("non-empty command line");
    assert_eq!(*last, 0, "make_command_line must NUL-terminate");
    String::from_utf16(body).expect("well-formed UTF-16")
}

/// A plain argument takes `append_arg`'s no-quoting branch, isolating
/// the encode/decode round-trip from the quoting rules.
#[test]
fn plain_arg_round_trips_non_ascii_and_non_bmp() {
    let program = std::ffi::OsString::from(r"C:\Users\café\bin\日本語.exe");
    let arg = std::ffi::OsString::from("🎉𝔘𝔫𝔦𝔠𝔬𝔡𝔢-café-日本語");
    let cmd = make_command_line(program.as_os_str(), std::slice::from_ref(&arg)).unwrap();
    let decoded = decode(&cmd);
    assert_eq!(
        decoded,
        format!(
            "\"{}\" {}",
            program.to_str().unwrap(),
            arg.to_str().unwrap()
        )
    );
}

/// The same text through `append_arg`'s quoting branch.
#[test]
fn quoted_arg_round_trips_non_ascii_and_non_bmp() {
    let program = std::ffi::OsString::from(r"C:\Program Files\ral.exe");
    let arg = std::ffi::OsString::from("🎉 café 日本語 with spaces");
    let cmd = make_command_line(program.as_os_str(), std::slice::from_ref(&arg)).unwrap();
    let decoded = decode(&cmd);
    assert_eq!(
        decoded,
        format!(
            "\"{}\" \"{}\"",
            program.to_str().unwrap(),
            arg.to_str().unwrap()
        )
    );
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
