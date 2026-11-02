use super::super::encode::encode_edit;
use super::super::testkit::{card_value, int, list, map_value, s, variant};
use super::*;

fn mark(label: &str, fields: Vec<(&str, FOValue)>) -> FOValue {
    variant(label, map_value(fields))
}
/// The record shape [`decode_row`] lifts back into a [`Row`].
fn seg_row(tag: &str, text: &str) -> FOValue {
    map_value(vec![
        ("tag", s(tag)),
        ("segs", list(vec![map_value(vec![("text", s(text))])])),
    ])
}

#[test]
fn drops_non_card_but_lifts_bare_mark() {
    assert!(value_to_card(&s("nope")).is_none());
    assert!(
        value_to_card(&variant("bogus", map_value(vec![]))).is_none(),
        "an unknown top-level variant is not a card"
    );
    let bare = mark("diff", vec![("path", s("a.rs"))]);
    let Card(marks) = value_to_card(&bare).expect("a bare diff lifts");
    assert_eq!(marks.len(), 1);
    assert!(matches!(&marks[0], Mark::Diff { .. }));
}

/// The sibling marks of an unknown one still render.
#[test]
fn unknown_mark_degrades_to_plain_text() {
    let v = card_value(vec![
        mark("text", vec![("spans", list(vec![]))]),
        mark("wormhole", vec![("x", int(1))]),
    ]);
    let Card(marks) = value_to_card(&v).expect("card decodes");
    assert_eq!(marks.len(), 2);
    assert!(matches!(&marks[1], Mark::Text { .. }), "unknown → text");
}

/// The one mark of a decoded one-mark card.
fn only(mark: FOValue) -> Mark {
    let Card(mut marks) = value_to_card(&card_value(vec![mark])).expect("a card decodes");
    assert_eq!(marks.len(), 1);
    marks.remove(0)
}

fn bytes_of(bytes: FOValue) -> Vec<u8> {
    match only(mark("raw", vec![("bytes", bytes)])) {
        Mark::Raw { bytes } => bytes,
        other => panic!("not a raw mark: {other:?}"),
    }
}

#[test]
fn raw_bytes_read_from_bytes_a_string_or_a_list_of_bytes() {
    assert_eq!(
        bytes_of(FOValue::Bytes {
            value: vec![0xff, 1]
        }),
        [0xff, 1]
    );
    assert_eq!(bytes_of(s("hi")), b"hi");
    assert_eq!(bytes_of(list(vec![int(1), int(255)])), [1, 255]);
    assert_eq!(bytes_of(int(7)), b"");
}

/// A list entry that is no byte is dropped, not wrapped.
#[test]
fn a_list_entry_that_is_no_byte_is_dropped() {
    let items = vec![int(1), int(256), int(-1), s("x"), int(2)];
    assert_eq!(bytes_of(list(items)), [1, 2]);
}

fn measure_of(fields: Vec<(&str, FOValue)>) -> Mark {
    only(mark("measure", fields))
}

#[test]
fn a_measure_without_a_value_degrades_to_plain_text() {
    let Mark::Text { spans } = measure_of(vec![("label", s("n"))]) else {
        panic!("a readout needs its value");
    };
    assert!(matches!(spans.as_slice(), [Span { role: None, text }] if text == "measure"));
}

#[test]
fn a_magnitude_clamps_into_u32() {
    let value = |n| match measure_of(vec![("value", int(n))]) {
        Mark::Measure(m) => m.readout.value,
        other => panic!("not a measure: {other:?}"),
    };
    assert_eq!(value(-5), 0);
    assert_eq!(value(i64::from(u32::MAX) + 1), u32::MAX);
}

fn row_value(fields: Vec<(&str, FOValue)>) -> FieldVal {
    let row = map_value(fields);
    match only(mark("fields", vec![("rows", list(vec![row]))])) {
        Mark::Fields { mut rows } => rows.remove(0).value,
        other => panic!("not a fields mark: {other:?}"),
    }
}

#[test]
fn a_nested_measure_reads_as_the_rows_readout() {
    let nested = variant(
        "measure",
        map_value(vec![
            ("label", s("dropped")),
            ("value", int(3)),
            ("max", int(5)),
        ]),
    );
    let FieldVal::Readout(r) = row_value(vec![("label", s("cov")), ("value", nested)]) else {
        panic!("a nested measure is a readout");
    };
    assert_eq!((r.value, r.max), (3, Some(5)));
}

#[test]
fn a_bare_row_value_is_one_plain_span() {
    let val = row_value(vec![("label", s("tests")), ("value", s("42 passed"))]);
    assert!(
        matches!(val, FieldVal::Inline(spans) if matches!(spans.as_slice(), [Span { role: None, text }] if text == "42 passed"))
    );
}

#[test]
fn a_nested_measure_without_a_value_is_an_empty_row() {
    let nested = variant("measure", map_value(vec![("max", int(5))]));
    let val = row_value(vec![("label", s("cov")), ("value", nested)]);
    assert!(matches!(val, FieldVal::Inline(spans) if spans.is_empty()));
}

#[test]
fn a_string_span_is_plain_and_an_unknown_role_is_dropped() {
    let spans = list(vec![
        s("bare"),
        map_value(vec![("role", s("chartreuse")), ("text", s("odd"))]),
    ]);
    let Mark::Text { spans } = only(mark("text", vec![("spans", spans)])) else {
        panic!("a text mark");
    };
    assert!(spans.iter().all(|span| span.role.is_none()));
    assert_eq!(
        spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>(),
        ["bare", "odd"]
    );
}

#[test]
fn an_unknown_row_tag_degrades_to_context() {
    let hunk = map_value(vec![(
        "rows",
        list(vec![
            seg_row("bogus", "a"),
            map_value(vec![]),
            seg_row("add", "b"),
        ]),
    )]);
    let Mark::Diff { hunks, .. } = only(mark(
        "diff",
        vec![("path", s("a.rs")), ("hunks", list(vec![hunk]))],
    )) else {
        panic!("a diff mark");
    };
    assert!(matches!(
        hunks[0].rows.as_slice(),
        [Row::Context(_), Row::Context(_), Row::Add(_)]
    ));
}

fn an_edit() -> Diff {
    Diff::between("a\nb\n", "a\nc\nd\n")
}

#[test]
fn an_edit_round_trips_as_a_committed_change() {
    let diff = an_edit();
    let change = value_to_edit(&encode_edit("a.rs", &diff)).expect("an edit decodes");
    let got = change.diff.expect("an edit carries its diff");
    assert_eq!(change.path, "a.rs");
    assert_eq!(change.outcome, WriteOutcome::Committed);
    assert_eq!((got.added, got.removed), (diff.added, diff.removed));
    assert_eq!(got.hunks.len(), diff.hunks.len());
    let rows = |d: &Diff| {
        d.hunks
            .iter()
            .flat_map(|h| h.rows.iter().map(Row::text))
            .collect::<Vec<_>>()
    };
    assert_eq!(rows(&got), rows(&diff));
}

#[test]
fn only_a_complete_edit_decodes() {
    let FOValue::Variant { payload, .. } = encode_edit("a.rs", &an_edit()) else {
        unreachable!("an edit is a variant")
    };
    let record = *payload.expect("an edit carries a record");
    assert!(value_to_edit(&variant("card", record.clone())).is_none());
    assert!(value_to_edit(&s("edit")).is_none());
    for missing in ["path", "added", "removed"] {
        let FOValue::Map { entries } = record.clone() else {
            unreachable!("an edit's payload is a record")
        };
        let entries = entries.into_iter().filter(|(k, _)| k != missing).collect();
        assert!(
            value_to_edit(&variant("edit", FOValue::Map { entries })).is_none(),
            "an edit without `{missing}`"
        );
    }
}
