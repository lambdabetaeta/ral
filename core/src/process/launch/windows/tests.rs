use super::*;

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
