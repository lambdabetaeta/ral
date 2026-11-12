use super::*;
use crate::source::FileId;
use crate::ty::{Ty, TyNode, UnitId};
use crate::ty::{closed_record, closed_variant, open_record, open_variant};
use crate::typecheck::Unifier;

fn site(u: &Unifier, ty: &Ty) -> Site {
    Site::snapshot(u, ty, UnitId::fresh())
}

fn ground(ty: &Ty) -> Site {
    site(&Unifier::new(), ty)
}

fn weak_comparable(u: &mut Unifier) -> Ty {
    let v = u.fresh_kinded(Kind::COMPARABLE);
    let TyNode::Var(root) = *v.node() else {
        unreachable!()
    };
    u.mark_residual(crate::ty::Var::Ty(root));
    v
}

fn map(entries: &[(&str, ValClos)]) -> ValClos {
    ValClos::map(
        entries
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect::<Vec<_>>(),
    )
}

fn refusal(site: &Site, value: &ValClos) -> String {
    site.admit(value, &Fixings::default())
        .expect_err("the value must be refused")
        .to_string()
}

#[test]
fn a_withheld_handle_keeps_its_type_but_is_not_data() {
    let handle = ValClos::withheld("child job");
    assert!(
        ground(&Ty::handle(crate::ty::Grade::Value, Ty::int()))
            .admit(&handle, &Fixings::default())
            .is_ok()
    );
    let mut u = Unifier::new();
    let data = u.fresh_kinded(Kind::DATA);
    let value = ValClos::list(vec![ValClos::variant("job", Some(handle))]);
    let mismatch = site(&u, &data)
        .admit(&value, &Fixings::default())
        .expect_err("a withheld handle is still not data");
    assert_eq!(mismatch.failure.pointer, "/0/payload");
    assert_eq!(
        mismatch.failure.cause,
        AdmissionCause::Uses(AdmissionUses {
            found: ValClos::withheld("child job").found(),
            expected: Expected::Kind(Kind::DATA.into()),
        })
    );

    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let mut u = Unifier::new();
            let data = u.fresh_kinded(Kind::DATA);
            let s = site(&u, &data);
            let depth = 10_000;
            let tagged = |terminal| {
                (0..depth).fold(terminal, |payload, _| {
                    ValClos::variant("job", Some(payload))
                })
            };
            let value = ValClos::list(vec![
                tagged(ValClos::UNIT),
                tagged(ValClos::withheld("first job")),
                ValClos::withheld("later job"),
            ]);
            let mismatch = s.admit(&value, &Fixings::default()).unwrap_err();
            assert_eq!(
                mismatch.failure.pointer,
                format!("/1{}", "/payload".repeat(depth))
            );
            assert_eq!(
                mismatch.failure.cause,
                AdmissionCause::Uses(AdmissionUses {
                    found: ValClos::withheld("first job").found(),
                    expected: Expected::Kind(Kind::DATA.into()),
                })
            );
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn a_key_with_a_slash_or_a_tilde_is_escaped_in_its_pointer() {
    let ty = Ty::map(Ty::int());
    let value = map(&[("a/b~c", ValClos::string("x"))]);
    assert!(refusal(&ground(&ty), &value).contains("`/a~1b~0c`"));
}

#[test]
fn an_open_row_admits_extra_fields_and_a_closed_one_names_them() {
    let mut u = Unifier::new();
    let tail = u.fresh_row_var();
    let value = map(&[("a", ValClos::int(1)), ("extra", ValClos::bool(true))]);
    let open = open_record(&[("a", Ty::int())], tail);
    assert!(site(&u, &open).admit(&value, &Fixings::default()).is_ok());
    let closed = closed_record(&[("a", Ty::int())]);
    assert!(refusal(&ground(&closed), &value).contains("has a field \"extra\""));

    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let depth = 10_000;
            let at = Span::new(FileId::DUMMY, 11, 17);
            for deep in [false, true] {
                let mut u = Unifier::new();
                u.at = Some(at);
                let tail = u.fresh_deep_row_var(deep);
                let row = open_record(&[("a", Ty::int())], tail);
                let tags = open_variant(&[("known", Ty::unit())], tail);
                for terminal in [ValClos::UNIT, ValClos::withheld("hidden job")] {
                    let is_unit = matches!(terminal.view(), View::Unit);
                    let payload = (0..depth).fold(terminal, |payload, _| {
                        ValClos::variant("unknown", Some(payload))
                    });
                    for (ty, value, prefix) in [
                        (
                            &row,
                            map(&[("a", ValClos::int(1)), ("extra/~", payload.clone())]),
                            "/extra~1~0",
                        ),
                        (&tags, ValClos::variant("other", Some(payload)), "/payload"),
                    ] {
                        let root = u.fresh_ty();
                        u.unify_ty(&root, ty).unwrap();
                        let result = site(&u, &root).admit(&value, &Fixings::default());
                        if deep && !is_unit {
                            let mismatch = result.unwrap_err();
                            assert_eq!(
                                mismatch.failure.pointer,
                                format!("{prefix}{}", "/payload".repeat(depth))
                            );
                            assert_eq!(mismatch.witness(), Some(at));
                            assert_eq!(
                                mismatch.failure.cause,
                                AdmissionCause::Uses(AdmissionUses {
                                    found: ValClos::withheld("hidden job").found(),
                                    expected: Expected::Kind(Kind::DATA.into()),
                                })
                            );
                        } else {
                            result.unwrap();
                        }
                    }
                }
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn a_kinded_variable_admits_only_its_heads() {
    let mut u = Unifier::new();
    let v = u.fresh_kinded(Kind::NUMBER);
    let s = site(&u, &Ty::list(v));
    assert!(
        s.admit(
            &ValClos::list(vec![
                ValClos::int(1),
                ValClos::float(crate::data::Finite::new(2.5).expect("finite"))
            ]),
            &Fixings::default()
        )
        .is_ok()
    );
    let mismatch = s
        .admit(
            &ValClos::list(vec![ValClos::string("x")]),
            &Fixings::default(),
        )
        .unwrap_err();
    assert_eq!(mismatch.failure.pointer, "/0");
    assert_eq!(
        mismatch.failure.cause,
        AdmissionCause::Uses(AdmissionUses {
            found: ValClos::string("x").found(),
            expected: Expected::Kind(Kind::NUMBER.into())
        })
    );
}

#[test]
fn a_free_variable_admits_anything_a_parametric_use_could_hold() {
    let mut u = Unifier::new();
    let v = u.fresh_ty();
    let s = site(&u, &Ty::list(v));
    assert!(
        s.admit(
            &ValClos::list(vec![ValClos::int(1), ValClos::string("a")]),
            &Fixings::default()
        )
        .is_ok()
    );
}

#[test]
fn a_comparable_variable_is_fixed_to_numbers_or_to_text() {
    let mut u = Unifier::new();
    let v = weak_comparable(&mut u);
    let ty = closed_record(&[("a", v.clone()), ("c", v)]);
    let s = site(&u, &ty);
    assert!(
        s.admit(
            &map(&[
                ("a", ValClos::int(1)),
                (
                    "c",
                    ValClos::float(crate::data::Finite::new(2.5).expect("finite"))
                )
            ]),
            &Fixings::default()
        )
        .is_ok()
    );
    let mixed = map(&[("a", ValClos::int(1)), ("c", ValClos::string("x"))]);
    let mismatch = s.admit(&mixed, &Fixings::default()).unwrap_err();
    assert_eq!(mismatch.failure.pointer, "/c");
    assert_eq!(
        mismatch.failure.cause,
        AdmissionCause::Shares(AdmissionShares {
            other: "/a".into(),
            other_side: crate::fact::NumberOrText::Number,
            side: crate::fact::NumberOrText::Text,
        })
    );
}

#[test]
fn fixings_are_shared_by_every_site_of_a_unit() {
    let mut u = Unifier::new();
    let v = weak_comparable(&mut u);
    let (unit, fixings) = (UnitId::fresh(), Fixings::default());
    let first = Site::snapshot(&u, &v, unit);
    let second = Site::snapshot(&u, &v, unit);
    assert!(first.admit(&ValClos::int(1), &fixings).is_ok());
    let refused = second
        .admit(&ValClos::string("x"), &fixings)
        .expect_err("the unit's variable is fixed to numbers");
    assert!(refused.to_string().contains("compares them"));
}

#[test]
fn a_unit_fixes_independently_in_two_stores() {
    let mut u = Unifier::new();
    let v = weak_comparable(&mut u);
    let s = site(&u, &v);
    let (one, other) = (Fixings::default(), Fixings::default());
    assert!(s.admit(&ValClos::int(1), &one).is_ok());
    assert!(s.admit(&ValClos::string("x"), &other).is_ok());
}

#[test]
fn a_recursive_type_admits_a_nested_document_and_refuses_a_scalar_leaf() {
    let mut u = Unifier::new();
    let v = u.fresh_tyvar();
    u.unify_ty(&Ty::var(v), &Ty::map(Ty::var(v)))
        .expect("a cycle through a map is data");
    let s = site(&u, &Ty::var(v));
    let nested = map(&[("a", map(&[("b", map(&[]))]))]);
    assert!(s.admit(&nested, &Fixings::default()).is_ok());
    let leaf = map(&[("a", map(&[("b", ValClos::int(1))]))]);
    let mismatch = s.admit(&leaf, &Fixings::default()).unwrap_err();
    assert_eq!(mismatch.failure.pointer, "/a/b");
    assert_eq!(
        mismatch.failure.cause,
        AdmissionCause::Uses(AdmissionUses {
            found: Found::Int(1),
            expected: Expected::Shape(ValueShape::Map)
        })
    );
}

#[test]
fn a_json_number_keeps_its_kind_at_a_ground_site() {
    let float = refusal(&ground(&Ty::float()), &ValClos::int(3));
    assert!(float.contains("the integer 3") && float.contains("a float"));
    let int = ground(&Ty::int())
        .admit(
            &ValClos::float(crate::data::Finite::new(2.5).expect("finite")),
            &Fixings::default(),
        )
        .expect_err("a fraction is not an Int");
    assert!(
        int.hint()
            .is_some_and(|hint| hint.contains("`float` or `int`"))
    );
}

#[test]
fn a_variant_site_needs_a_tag_of_its_row() {
    let ty = closed_variant(&[("none", Ty::unit()), ("some", Ty::int())]);
    let s = ground(&ty);
    let some = ValClos::variant("some", Some(ValClos::int(1)));
    assert!(s.admit(&some, &Fixings::default()).is_ok());
    let none = ValClos::variant("none", None);
    assert!(s.admit(&none, &Fixings::default()).is_ok());
    let other = ValClos::variant("other", None);
    assert!(
        matches!(s.admit(&other, &Fixings::default()).unwrap_err().failure.cause, AdmissionCause::Uses(AdmissionUses { expected: Expected::Tags(tags), found: Found::Variant(label) }) if tags == ["none", "some"] && label == "other")
    );
    assert_eq!(
        s.admit(&map(&[]), &Fixings::default())
            .unwrap_err()
            .failure
            .cause,
        AdmissionCause::Uses(AdmissionUses {
            found: Found::Map,
            expected: Expected::Shape(ValueShape::Variant),
        })
    );
    assert!(refusal(&s, &map(&[])).contains("uses it as a tag"));

    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let mut u = Unifier::new();
            let at = Span::new(FileId::DUMMY, 21, 27);
            u.at = Some(at);
            let recursive = u.fresh_ty();
            u.unify_ty(
                &recursive,
                &closed_variant(&[("more", recursive.clone()), ("stop", Ty::unit())]),
            )
            .unwrap();
            let s = site(&u, &recursive);
            let depth = 10_000;
            for (terminal, expected_failure) in [
                (ValClos::variant("stop", None), None),
                (
                    ValClos::variant("other", None),
                    Some((
                        depth,
                        Found::Variant("other".into()),
                        Expected::Tags(vec!["more".into(), "stop".into()]),
                    )),
                ),
                (
                    ValClos::variant("more", None),
                    Some((depth + 1, Found::Unit, Expected::Shape(ValueShape::Variant))),
                ),
                (
                    ValClos::variant("stop", Some(ValClos::variant("other", None))),
                    Some((
                        depth + 1,
                        Found::Variant("other".into()),
                        Expected::Shape(ValueShape::Unit),
                    )),
                ),
            ] {
                let value = (0..depth).fold(terminal, |payload, _| {
                    ValClos::variant("more", Some(payload))
                });
                let result = s.admit(&value, &Fixings::default());
                if let Some((steps, found, expected)) = expected_failure {
                    let mismatch = result.unwrap_err();
                    assert_eq!(mismatch.failure.pointer, "/payload".repeat(steps));
                    assert_eq!(mismatch.witness(), Some(at));
                    assert_eq!(
                        mismatch.failure.cause,
                        AdmissionCause::Uses(AdmissionUses { found, expected })
                    );
                } else {
                    result.unwrap();
                }
            }

            let payload_at = Span::new(FileId::DUMMY, 31, 37);
            for kind in [Kind::ANY, Kind::DATA, Kind::COMPARABLE] {
                u.at = Some(payload_at);
                let payload_ty = u.fresh_kinded(kind);
                u.at = Some(at);
                let root = u.fresh_ty();
                u.unify_ty(&root, &closed_variant(&[("some", payload_ty)]))
                    .unwrap();
                let payload = (0..depth).fold(ValClos::withheld("hidden job"), |payload, _| {
                    ValClos::variant("more", Some(payload))
                });
                let value = ValClos::variant("some", Some(payload));
                let result = site(&u, &root).admit(&value, &Fixings::default());
                if kind == Kind::ANY {
                    result.unwrap();
                    continue;
                }
                let mismatch = result.unwrap_err();
                let (steps, found) = if kind == Kind::DATA {
                    (depth + 1, ValClos::withheld("hidden job").found())
                } else {
                    (1, Found::Variant("more".into()))
                };
                assert_eq!(mismatch.failure.pointer, "/payload".repeat(steps));
                assert_eq!(mismatch.witness(), Some(payload_at));
                assert_eq!(
                    mismatch.failure.cause,
                    AdmissionCause::Uses(AdmissionUses {
                        found,
                        expected: Expected::Kind(kind.into()),
                    })
                );
            }

            let comparable = weak_comparable(&mut u);
            let recursive = u.fresh_ty();
            u.unify_ty(
                &recursive,
                &closed_variant(&[("more", recursive.clone()), ("value", comparable)]),
            )
            .unwrap();
            let tagged = |terminal| {
                (0..depth).fold(ValClos::variant("value", Some(terminal)), |payload, _| {
                    ValClos::variant("more", Some(payload))
                })
            };
            let value = ValClos::list(vec![tagged(ValClos::int(1)), tagged(ValClos::string("x"))]);
            let mismatch = site(&u, &Ty::list(recursive))
                .admit(&value, &Fixings::default())
                .unwrap_err();
            let payload_path = "/payload".repeat(depth + 1);
            assert_eq!(mismatch.failure.pointer, format!("/1{payload_path}"));
            assert_eq!(
                mismatch.failure.cause,
                AdmissionCause::Shares(AdmissionShares {
                    other: format!("/0{payload_path}"),
                    other_side: crate::fact::NumberOrText::Number,
                    side: crate::fact::NumberOrText::Text,
                })
            );
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn the_witness_is_the_use_that_fixed_the_structure() {
    let mut u = Unifier::new();
    let at = Span::new(FileId::DUMMY, 3, 9);
    u.at = Some(at);
    let v = u.fresh_tyvar();
    u.unify_ty(&Ty::var(v), &Ty::int())
        .expect("a free variable meets Int");
    let mismatch = site(&u, &Ty::var(v))
        .admit(&ValClos::string("x"), &Fixings::default())
        .expect_err("text is not an Int");
    assert_eq!(mismatch.witness(), Some(at));
    let Break::Error(error) = mismatch.refusal("from-json") else {
        panic!("admission refusal")
    };
    assert_eq!(error.witness(), Some(at));
    let without_witness = Mismatch {
        witness: None,
        ..mismatch
    };
    let Break::Error(portable) = without_witness.refusal("from-json") else {
        panic!("admission refusal")
    };
    assert_eq!(error.reason, portable.reason);
    assert_eq!(portable.witness(), None);
}

#[test]
fn a_site_round_trips_with_its_unit() {
    let mut u = Unifier::new();
    let v = weak_comparable(&mut u);
    let unit = UnitId::fresh();
    let (a, b) = (Site::snapshot(&u, &v, unit), Site::snapshot(&u, &v, unit));
    let wire = |site: &Site| postcard::to_allocvec(site).expect("a site serialises");
    let (a2, b2): (Site, Site) = (
        postcard::from_bytes(&wire(&a)).expect("and reads back"),
        postcard::from_bytes(&wire(&b)).expect("and reads back"),
    );
    assert_eq!(a2.nodes, a.nodes);
    assert_eq!((a2.unit, b2.unit), (unit, unit));
    let fixings = Fixings::default();
    assert!(a2.admit(&ValClos::int(1), &fixings).is_ok());
    assert!(b2.admit(&ValClos::string("x"), &fixings).is_err());
}

#[test]
fn admission_reasons_keep_structured_causes_and_witnesses_outside_records() {
    use crate::data::datum::Datum as _;
    use crate::fact::Reason;
    use crate::ir::Val;
    use crate::shell::runtime::{Binding, Env, Signature};
    use crate::source::Spanned;

    let mut env = Env::EMPTY;
    env.bind(
        "size".into(),
        Binding {
            value: ValClos::string("wrong"),
            scheme: None,
        },
    );
    let literal = Val::list(vec![Spanned::synthetic(Val::record(vec![(
        "size".into(),
        Spanned::synthetic(Val::Variable("size".into())),
    )]))]);
    let ty = Ty::list(closed_record(&[("size", Ty::int())]));
    for value in [
        ValClos::list(vec![map(&[("size", ValClos::string("wrong"))])]),
        ValClos::close(&literal, &env, &Signature::default()).unwrap(),
    ] {
        let mismatch = ground(&ty).admit(&value, &Fixings::default()).unwrap_err();
        let failure = mismatch.failure().clone();
        assert_eq!(failure.pointer, "/0/size");
        assert_eq!(
            failure.cause,
            AdmissionCause::Uses(AdmissionUses {
                found: ValClos::string("wrong").found(),
                expected: Expected::Shape(ValueShape::Int),
            })
        );
        let Break::Error(error) = mismatch.refusal("from-json") else {
            panic!("refusal")
        };
        assert_eq!(error.witness(), mismatch.witness());
        assert_eq!(error.hints(), []);
        assert!(
            matches!(&error.reason, Reason::Invalid(f) if f.cause == InvalidCause::Admission(failure))
        );
        assert_eq!(
            crate::fact::Reason::decode(&error.reason.clone().encode()).unwrap(),
            error.reason
        );
    }
    let missing = ground(&closed_record(&[("a", Ty::int())]))
        .admit(&map(&[]), &Fixings::default())
        .unwrap_err();
    assert_eq!(missing.failure.cause, AdmissionCause::NoField("a".into()));
    assert_eq!(missing.failure.pointer, "");
    let extra = ground(&closed_record(&[]))
        .admit(&map(&[("a", ValClos::int(1))]), &Fixings::default())
        .unwrap_err();
    assert_eq!(
        extra.failure.cause,
        AdmissionCause::ExtraField(AdmissionExtraField {
            field: "a".into(),
            fields: vec![]
        })
    );
    assert_eq!(extra.failure.pointer, "");
}
