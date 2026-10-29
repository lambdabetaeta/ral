use super::NonEmpty;

#[test]
fn non_empty_serializes_as_an_array_and_back() {
    let elements = NonEmpty {
        first: 1,
        rest: vec![2, 3],
    };
    let json = serde_json::to_string(&elements).unwrap();
    assert_eq!(json, "[1,2,3]");
    assert_eq!(
        serde_json::from_str::<NonEmpty<i32>>(&json).unwrap(),
        elements
    );
}

#[test]
fn non_empty_refuses_an_empty_array() {
    let error = serde_json::from_str::<NonEmpty<i32>>("[]").unwrap_err();
    assert!(error.to_string().contains("at least one"));
}

#[test]
fn non_empty_iterators_yield_first_then_rest() {
    let elements = NonEmpty {
        first: 1,
        rest: vec![2, 3],
    };
    assert_eq!(elements.iter().copied().collect::<Vec<_>>(), [1, 2, 3]);
    assert_eq!(
        (&elements).into_iter().copied().collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(elements.into_iter().collect::<Vec<_>>(), [1, 2, 3]);
}
