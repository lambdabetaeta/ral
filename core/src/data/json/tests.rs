use super::*;
use crate::data::{Bytes, FOValue, Finite};
use crate::fact::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

fn structural<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, DecodeFailure> {
    let document = JsonDocument::parse(bytes).unwrap();
    decode_structural(document.root())
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Product {
    required: u64,
    optional: Option<bool>,
    #[serde(default)]
    defaulted: Vec<String>,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Strict {
    flag: bool,
}

#[derive(Debug, PartialEq, Deserialize)]
enum Choice {
    Empty,
    One(bool),
    Pair(u8, String),
    Product { enabled: bool },
}

#[test]
fn syntax_preserves_order_duplicates_numeric_lexemes_and_source_ranges() {
    let input = br#" {"n":-0,"n":1.00e+02,"big":340282366920938463463374607431768211455} "#;
    let document = JsonDocument::parse(input).unwrap();
    let root = document.root();
    let members: Vec<_> = root.members().unwrap().collect();
    assert_eq!(
        members.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
        ["n", "n", "big"]
    );
    assert_eq!(members[0].1.number(), Some("-0"));
    assert_eq!(members[1].1.number(), Some("1.00e+02"));
    assert_eq!(&input[members[1].1.byte_range()], b"1.00e+02");
    assert_eq!(root.source(), &input[1..input.len() - 1]);
}

#[test]
fn syntax_is_flat_at_arbitrary_nesting_and_checks_the_entire_input() {
    let input = format!("{}null{}", "[".repeat(20_000), "]".repeat(20_000));
    let document = JsonDocument::parse(input.as_bytes()).unwrap();
    assert_eq!(document.root().source(), input.as_bytes());
    drop(document);
    for input in [b"null true".as_slice(), b"[1,]", b"{", b"\"\\uD800\""] {
        assert!(JsonDocument::parse(input).is_err());
    }
    let error = JsonDocument::parse(b"[\n  \"\\uD800\"\n]").unwrap_err();
    assert_eq!(error.line, Count::from_u64(2));
    assert!(error.column.to_u64() >= 3);
}

#[test]
fn struct_destination_owns_duplicate_unknown_default_and_sequence_policy() {
    let decoded: Product = structural(br#"{"ignored":1,"required":9,"ignored":2}"#).unwrap();
    assert_eq!(
        decoded,
        Product {
            required: 9,
            optional: None,
            defaulted: vec![]
        }
    );
    assert_eq!(structural::<Product>(br"[9,null,[]]").unwrap(), decoded);
    assert_eq!(structural::<Product>(br"[9,null]").unwrap(), decoded);
    let duplicate = structural::<Product>(br#"{"required":9,"required":10}"#).unwrap_err();
    assert_eq!(duplicate.path, []);
    assert_eq!(
        *duplicate.cause,
        DecodeCause::DuplicateField("required".into())
    );
    assert_eq!(
        *structural::<Product>(b"{}").unwrap_err().cause,
        DecodeCause::MissingField("required".into())
    );
    let unknown = structural::<Strict>(br#"{"flag":true,"other":false}"#).unwrap_err();
    assert_eq!(unknown.path, [] as [DataStep; 0]);
    assert_eq!(
        *unknown.cause,
        DecodeCause::UnknownField(UnknownField {
            field: "other".into(),
            offered: vec!["flag".into()],
        })
    );
    assert_eq!(
        structural::<Strict>(br#"{"flag":true}"#).unwrap(),
        Strict { flag: true }
    );
    assert!(matches!(
        *structural::<Product>(b"[9]").unwrap_err().cause,
        DecodeCause::Length(_)
    ));
}

#[test]
fn maps_and_sets_retain_their_destination_replacement_and_collapse() {
    assert_eq!(
        structural::<BTreeMap<String, u8>>(br#"{"a":1,"a":2}"#).unwrap(),
        BTreeMap::from([("a".into(), 2)])
    );
    assert_eq!(
        structural::<BTreeSet<u8>>(b"[1,1,2]").unwrap(),
        BTreeSet::from([1, 2])
    );
    let value: serde_json::Value = structural(br#"{"a":1,"a":2}"#).unwrap();
    assert_eq!(value["a"], 2);
}

#[test]
fn nested_failures_have_field_index_and_payload_steps() {
    #[derive(Debug, Deserialize)]
    struct Envelope {
        choices: Vec<Choice>,
    }
    assert_eq!(
        structural::<Envelope>(br#"{"choices":[]}"#)
            .unwrap()
            .choices
            .len(),
        0
    );
    let error = structural::<Envelope>(br#"{"choices":[{"Product":{"enabled":3}}]}"#).unwrap_err();
    assert_eq!(
        error.path,
        [
            DataStep::Field("choices".into()),
            DataStep::Index(Count::from_u64(0)),
            DataStep::Payload("Product".into()),
            DataStep::Field("enabled".into())
        ]
    );
    assert_eq!(
        *error.cause,
        DecodeCause::JsonShape(JsonShapeFailure {
            expected: vec![JsonShape::Bool],
            found: JsonShape::Number
        })
    );
    let missing = structural::<Choice>(br#"{"Product":{}}"#).unwrap_err();
    assert_eq!(missing.path, [DataStep::Payload("Product".into())]);
    assert_eq!(*missing.cause, DecodeCause::MissingField("enabled".into()));
}

#[test]
fn enum_payload_and_tuple_cardinality_is_structured() {
    assert_eq!(structural::<Choice>(br#""Empty""#).unwrap(), Choice::Empty);
    assert_eq!(
        structural::<Choice>(br#"{"Empty":null}"#).unwrap(),
        Choice::Empty
    );
    assert_eq!(
        structural::<Choice>(br#"{"Pair":[3,"x"]}"#).unwrap(),
        Choice::Pair(3, "x".into())
    );
    assert_eq!(
        structural::<Choice>(br#"{"Product":[true]}"#).unwrap(),
        Choice::Product { enabled: true }
    );
    assert!(matches!(
        *structural::<Choice>(b"{}").unwrap_err().cause,
        DecodeCause::Length(_)
    ));
    assert!(matches!(
        *structural::<Choice>(br#"{"One":true,"Empty":null}"#)
            .unwrap_err()
            .cause,
        DecodeCause::Length(_)
    ));
    assert_eq!(
        *structural::<Choice>(br#""One""#).unwrap_err().cause,
        DecodeCause::MissingPayload("One".into())
    );
    assert_eq!(
        *structural::<Choice>(br#""Unknown""#).unwrap_err().cause,
        DecodeCause::UnknownTag(UnknownTag {
            tag: "Unknown".into(),
            offered: vec![
                "Empty".into(),
                "One".into(),
                "Pair".into(),
                "Product".into()
            ],
        })
    );
    for input in [b"[1]".as_slice(), b"[1,2,3]"] {
        let error = structural::<(u8, u8)>(input).unwrap_err();
        assert_eq!(
            *error.cause,
            DecodeCause::Length(DecodeLength {
                expected: CountRule::Exactly(Count::from_u64(2)),
                got: Count::from_u64(if input == b"[1]" { 1 } else { 3 }),
            })
        );
    }
}

#[test]
#[expect(
    clippy::string_lit_as_bytes,
    reason = "Each token exercises both the string and byte decoding APIs"
)]
fn primitive_numbers_preserve_pinned_acceptance_and_128_bit_precision() {
    macro_rules! agrees {
        ($ty:ty, $($token:literal),+ $(,)?) => {
            $(
                let pinned = serde_json::from_str::<$ty>($token);
                let typed = structural::<$ty>($token.as_bytes());
                assert_eq!(typed.is_ok(), pinned.is_ok(), "{} as {}", $token, stringify!($ty));
                if let (Ok(typed), Ok(pinned)) = (typed, pinned) { assert_eq!(typed, pinned); }
            )+
        };
    }
    agrees!(i8, "-129", "-128", "127", "128", "-0", "1.0", "1e0");
    agrees!(
        i64,
        "-9223372036854775808",
        "9223372036854775807",
        "9223372036854775808",
        "-0",
        "1e2"
    );
    agrees!(
        u64,
        "9223372036854775808",
        "18446744073709551615",
        "18446744073709551616",
        "-1",
        "-0",
        "1.0"
    );
    agrees!(
        i128,
        "-0",
        "1.0",
        "170141183460469231731687303715884105727",
        "170141183460469231731687303715884105728"
    );
    agrees!(
        u128,
        "340282366920938463463374607431768211455",
        "340282366920938463463374607431768211456",
        "-0"
    );
    agrees!(f32, "1e38", "1e39", "1e999");
    agrees!(f64, "1e308", "1e309", "1e-999", "-0");
    assert_eq!(
        structural::<f64>(b"-0").unwrap().to_bits(),
        (-0.0_f64).to_bits()
    );
    assert_eq!(
        structural::<u128>(b"340282366920938463463374607431768211455").unwrap(),
        u128::MAX
    );
    let error = structural::<i64>(b"1.00e+2").unwrap_err();
    assert_eq!(
        *error.cause,
        DecodeCause::JsonNumber(JsonNumberFailure {
            text: "1.00e+2".into(),
            target: JsonNumberTarget::I64,
            cause: JsonNumberFault::NotIntegerRepresentation,
        })
    );
    assert!(matches!(
        *structural::<f64>(b"1e999").unwrap_err().cause,
        DecodeCause::JsonNumber(JsonNumberFailure {
            cause: JsonNumberFault::NonFinite,
            ..
        })
    ));
}

#[test]
#[expect(
    clippy::zero_sized_map_values,
    reason = "The fixture tests numeric JSON map keys with null values"
)]
fn numeric_map_keys_preserve_the_original_escaped_token_rules() {
    for input in [
        r#"{"01":null}"#,
        r#"{"1e0":null}"#,
        r#"{"-0":null}"#,
        r#"{"\u0031":null}"#,
        r#"{"18446744073709551615":null}"#,
    ] {
        let pinned = serde_json::from_str::<BTreeMap<u64, ()>>(input);
        let typed = structural::<BTreeMap<u64, ()>>(input.as_bytes());
        assert_eq!(typed.is_ok(), pinned.is_ok(), "{input}");
        if let (Ok(typed), Ok(pinned)) = (typed, pinned) {
            assert_eq!(typed, pinned);
        }
    }
    let error = structural::<BTreeMap<u8, ()>>(br#"{"256":null}"#).unwrap_err();
    assert_eq!(error.path, [DataStep::Field("256".into())]);
    assert_eq!(
        *error.cause,
        DecodeCause::JsonMapKey(JsonMapKeyFailure {
            text: "256".into(),
            lexeme: r#""256""#.into(),
            target: JsonNumberTarget::U8
        })
    );
}

#[test]
fn borrowed_strings_retain_escape_buffer_acceptance() {
    assert_eq!(structural::<&str>(br#""plain""#).unwrap(), "plain");
    assert_eq!(
        structural::<String>(br#""escaped\n""#).unwrap(),
        "escaped\n"
    );
    assert_eq!(
        *structural::<&str>(br#""escaped\n""#).unwrap_err().cause,
        DecodeCause::JsonString(JsonStringFailure::BorrowedRequired)
    );
}

#[test]
fn finite_and_base64_leaf_failures_are_closed_and_lossless() {
    for bits in [
        f64::NAN.to_bits(),
        f64::INFINITY.to_bits(),
        f64::NEG_INFINITY.to_bits(),
    ] {
        assert_eq!(
            from_slice::<Finite>(bits.to_string().as_bytes()).unwrap_err(),
            JsonInputFailure::Decode(DecodeFailure::new(DecodeCause::FiniteBits(
                FiniteBitsFailure {
                    bits: Count::from_u64(bits)
                }
            )))
        );
    }
    for (text, expected) in [
        (
            "??",
            Base64Failure::InvalidByte(Base64FailureInvalidByte {
                index: Count::from_u64(0),
                byte: b'?',
            }),
        ),
        (
            "A",
            Base64Failure::InvalidLength(Base64FailureInvalidLength {
                length: Count::from_u64(1),
            }),
        ),
        (
            "Zh==",
            Base64Failure::InvalidLastSymbol(Base64FailureInvalidLastSymbol {
                index: Count::from_u64(1),
                byte: b'h',
            }),
        ),
        ("Zg", Base64Failure::InvalidPadding),
    ] {
        assert_eq!(
            *Bytes::from_base64(text).unwrap_err().cause,
            DecodeCause::Base64(expected)
        );
    }
    let bytes = Bytes::from(vec![0, 128, 255]);
    assert_eq!(
        from_slice::<Bytes>(&to_vec(&bytes).unwrap()).unwrap(),
        bytes
    );
    for value in [-0.0, 0.1, f64::MAX] {
        let finite = Finite::new(value).unwrap();
        assert_eq!(
            from_slice::<Finite>(&to_vec(&finite).unwrap())
                .unwrap()
                .get()
                .to_bits(),
            value.to_bits()
        );
    }
}

#[test]
fn raw_first_order_map_entries_stay_a_sequence_with_duplicate_keys() {
    let value = FOValue::Map {
        entries: vec![
            ("x".into(), FOValue::Int { value: 1 }),
            (
                "x".into(),
                FOValue::Bytes {
                    value: vec![0, 255],
                },
            ),
        ],
    };
    let compact = to_vec(&value).unwrap();
    assert_eq!(compact, serde_json::to_vec(&value).unwrap());
    assert_eq!(
        to_vec_pretty(&value).unwrap(),
        serde_json::to_vec_pretty(&value).unwrap()
    );
    assert_eq!(from_slice::<FOValue>(&compact).unwrap(), value);
    let raw = RawFOValue::from_value(value.clone());
    assert_eq!(serialize_structural(&raw, JsonFormat::Compact), compact);
    assert_eq!(raw.validate().unwrap(), value);
}

#[test]
fn datum_boundary_keeps_logical_paths_and_unit_normalization() {
    #[derive(Debug, Clone, PartialEq)]
    struct Flags {
        enabled: bool,
    }
    crate::record!(Flags { enabled: "enabled" });
    let invalid: FOValue = FOValue::Map {
        entries: vec![("enabled".into(), FOValue::Int { value: 1 })],
    };
    let JsonInputFailure::Decode(error) =
        from_datum_slice::<Flags>(&to_vec(&invalid).unwrap()).unwrap_err()
    else {
        panic!("semantic failure")
    };
    assert_eq!(error.path, [DataStep::Field("enabled".into())]);
    assert!(matches!(*error.cause, DecodeCause::Shape(_)));
    let count = Count::from_u64(u64::MAX);
    assert_eq!(
        from_datum_slice::<Count>(&to_datum_vec(&count)).unwrap(),
        count
    );
    assert_eq!(
        from_datum_slice::<Option<()>>(&to_datum_vec(&Some(()))).unwrap(),
        Some(())
    );
}

#[test]
fn generic_extensions_use_owner_validation_and_encoding_paths() {
    let raw = RawFOValue::List {
        items: vec![RawFOValue::Ext(17_u64)],
    };
    let value = raw.validate_with(|n| Ok(*n)).unwrap();
    let back = RawFOValue::from_value_with(value, Ok).unwrap();
    let refused = RawFOValue::<()>::from_value_with(
        FOValue::List {
            items: vec![FOValue::Ext(())],
        },
        |()| {
            Err(JsonEncodeFailure::new(JsonEncodeCause::NativeComputation(
                JsonEncodeCauseNativeComputation {
                    name: "fixture".into(),
                },
            )))
        },
    )
    .unwrap_err();
    assert_eq!(
        refused.path,
        [
            DataStep::Payload("list".into()),
            DataStep::Field("items".into()),
            DataStep::Index(Count::from_u64(0)),
            DataStep::Payload("ext".into()),
        ]
    );
    assert_eq!(
        serialize_structural(&raw, JsonFormat::Compact),
        serialize_structural(&back, JsonFormat::Compact)
    );
    let error = raw
        .validate_with::<()>(|_| {
            Err(DecodeFailure::new(DecodeCause::MissingField(
                "owner".into(),
            )))
        })
        .unwrap_err();
    assert_eq!(
        error.path,
        [
            DataStep::Payload("list".into()),
            DataStep::Field("items".into()),
            DataStep::Index(Count::from_u64(0)),
            DataStep::Payload("ext".into())
        ]
    );
}

#[test]
fn deep_raw_serialization_validation_and_error_disposal_grow_or_iterate() {
    let mut raw: RawFOValue = RawFOValue::Unit;
    for _ in 0..4_000 {
        raw = RawFOValue::List { items: vec![raw] };
    }
    let bytes = serialize_structural(&raw, JsonFormat::Compact);
    let value = from_slice::<FOValue>(&bytes).unwrap();
    assert_eq!(to_vec(&value).unwrap(), bytes);
    drop_value(value);
    drop(raw);
    let invalid = format!(
        r#"{{"list":{{"items":[{},{{"bool":{{"value":0}}}}]}}}}"#,
        String::from_utf8(bytes).unwrap()
    );
    assert!(from_slice::<FOValue>(invalid.as_bytes()).is_err());
}

#[test]
fn first_order_clone_preserves_shape_order_and_extension_clone_semantics() {
    use std::{cell::RefCell, rc::Rc};

    #[derive(Debug, PartialEq)]
    struct Extension {
        id: u8,
        visits: Rc<RefCell<Vec<u8>>>,
    }
    impl Clone for Extension {
        fn clone(&self) -> Self {
            self.visits.borrow_mut().push(self.id);
            Self {
                id: self.id + 1,
                visits: self.visits.clone(),
            }
        }
    }

    let visits = Rc::new(RefCell::new(Vec::new()));
    let fixture = |offset: u8| {
        let extension = |id: u8| {
            FOValue::Ext(Extension {
                id: id + offset,
                visits: visits.clone(),
            })
        };
        FOValue::List {
            items: vec![
                extension(1),
                FOValue::Map {
                    entries: vec![
                        ("duplicate".into(), extension(2)),
                        (
                            "duplicate".into(),
                            FOValue::Variant {
                                label: "payload".into(),
                                payload: Some(Box::new(extension(3))),
                            },
                        ),
                    ],
                },
                FOValue::Variant {
                    label: "bare".into(),
                    payload: None,
                },
            ],
        }
    };
    let value = fixture(0);
    let cloned = value.clone();
    assert_eq!(*visits.borrow(), [1, 2, 3]);
    assert_eq!(cloned, fixture(1));
    assert_eq!(value, fixture(0));
}

fn deep_first_order_value() -> FOValue {
    let mut value = FOValue::List {
        items: vec![
            FOValue::Unit,
            FOValue::Bool { value: true },
            FOValue::Int { value: i64::MIN },
            FOValue::Float {
                value: Finite::new(-0.0).unwrap(),
            },
            FOValue::String {
                value: "leaf".into(),
            },
            FOValue::Bytes {
                value: vec![0, 128, 255],
            },
            FOValue::Variant {
                label: "bare".into(),
                payload: None,
            },
        ],
    };
    for depth in 0..4_000 {
        value = match depth % 3 {
            0 => FOValue::List { items: vec![value] },
            1 => FOValue::Map {
                entries: vec![
                    ("duplicate".into(), FOValue::Int { value: depth }),
                    ("duplicate".into(), value),
                ],
            },
            _ => FOValue::Variant {
                label: "nested".into(),
                payload: Some(Box::new(value)),
            },
        };
    }
    value
}

#[test]
fn to_datum_vec_clones_deep_first_order_values_without_changing_representation() {
    let value = deep_first_order_value();
    assert_eq!(to_datum_vec(&value), to_vec(&value).unwrap());
    drop_value(value);
}

#[test]
fn from_datum_slice_clones_deep_first_order_values_without_changing_representation() {
    let value = deep_first_order_value();
    let bytes = to_vec(&value).unwrap();
    let decoded = from_datum_slice::<FOValue>(&bytes).unwrap();
    assert_eq!(to_vec(&decoded).unwrap(), bytes);
    drop_value(decoded);
    drop_value(value);
}
