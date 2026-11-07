use super::*;

mod inheritance;

fn decode_wide_null(v: &[u16]) -> String {
    let (last, body) = v.split_last().expect("non-empty");
    assert_eq!(*last, 0, "wide_null must NUL-terminate");
    String::from_utf16(body).expect("well-formed UTF-16")
}

#[test]
fn wide_null_round_trips_non_ascii_and_non_bmp() {
    let s = "🎉𝔘𝔫𝔦𝔠𝔬𝔡𝔢-café-日本語";
    let encoded = wide_null(OsStr::new(s)).unwrap();
    assert_eq!(decode_wide_null(&encoded), s);
}

/// `environment_block` layers `edits` over the live environment, so
/// the assertion hunts the block for the one injected entry.
#[test]
fn environment_block_round_trips_a_non_ascii_non_bmp_value() {
    let key = super::super::env_key(OsStr::new("RAL_UNICODE_TEST_VAR"));
    let value = OsString::from("🎉𝔘𝔫𝔦𝔠𝔬𝔡𝔢-café-日本語");
    let edits = std::collections::BTreeMap::from([(key, EnvEdit::Set(value.clone()))]);
    let block = environment_block(&edits).unwrap();
    // Entries are NUL-terminated, plus one extra NUL closing the whole
    // block: strip that, then split on the per-entry NULs.
    let (_, body) = block.split_last().expect("non-empty");
    let text = String::from_utf16(body).expect("well-formed UTF-16");
    let expected = format!("RAL_UNICODE_TEST_VAR={}", value.to_str().unwrap());
    assert!(
        text.split('\u{0}').any(|entry| entry == expected),
        "expected entry {expected:?} not found in environment block"
    );
}

/// Only `CreateProcessW`'s own failure is the program's: a batch image and a
/// NUL in an argument are ral's refusals.
#[test]
fn only_create_process_s_failure_is_the_program_s() {
    let refused = |mut launch: Launch| {
        matches!(
            launch.spawn(PgidPolicy::Inherit),
            Err(Unstarted::Refused(_))
        )
    };
    assert!(refused(Launch::new(r"C:\ral-no-such-dir\tool.bat")));
    let mut nul = Launch::new(r"C:\Windows\System32\cmd.exe");
    nul.arg("a\0b");
    assert!(refused(nul));
    assert!(matches!(
        Launch::new(r"C:\ral-no-such-dir\tool.exe").spawn(PgidPolicy::Inherit),
        Err(Unstarted::Program(_))
    ));
}
