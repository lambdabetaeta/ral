use super::NonEmpty;

#[test]
fn non_empty_refuses_an_empty_array() {
    let error = serde_json::from_str::<NonEmpty<i32>>("[]").unwrap_err();
    assert!(error.to_string().contains("at least one"));
}
