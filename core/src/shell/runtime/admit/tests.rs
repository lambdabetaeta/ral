use super::*;
use crate::source::FileId;
use crate::ty::{Ty, TyNode, UnitId};
use crate::ty::{closed_record, closed_variant, open_record};
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

fn map(entries: &[(&str, Value)]) -> Value {
    Value::map(
        entries
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect::<Vec<_>>(),
    )
}

fn refusal(site: &Site, value: &Value) -> String {
    site.admit(value, &Fixings::default())
        .expect_err("the value must be refused")
        .to_string()
}

#[test]
fn a_key_with_a_slash_or_a_tilde_is_escaped_in_its_pointer() {
    let ty = Ty::map(Ty::int());
    let value = map(&[("a/b~c", Value::string("x"))]);
    assert!(refusal(&ground(&ty), &value).contains("`/a~1b~0c`"));
}

#[test]
fn an_open_row_admits_extra_fields_and_a_closed_one_names_them() {
    let mut u = Unifier::new();
    let tail = u.fresh_row_var();
    let value = map(&[("a", Value::Int(1)), ("extra", Value::Bool(true))]);
    let open = open_record(&[("a", Ty::int())], tail);
    assert!(site(&u, &open).admit(&value, &Fixings::default()).is_ok());
    let closed = closed_record(&[("a", Ty::int())]);
    assert!(refusal(&ground(&closed), &value).contains("has a field \"extra\""));
}

#[test]
fn a_kinded_variable_admits_only_its_heads() {
    let mut u = Unifier::new();
    let v = u.fresh_kinded(Kind::NUMBER);
    let s = site(&u, &Ty::list(v));
    assert!(
        s.admit(
            &Value::list(vec![
                Value::Int(1),
                Value::Float(crate::data::Finite::new(2.5).expect("finite"))
            ]),
            &Fixings::default()
        )
        .is_ok()
    );
    let mismatch = s
        .admit(&Value::list(vec![Value::string("x")]), &Fixings::default())
        .unwrap_err();
    assert_eq!(mismatch.failure.pointer, "/0");
    assert_eq!(
        mismatch.failure.cause,
        AdmissionCause::Uses(AdmissionUses {
            found: Value::string("x").found(),
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
            &Value::list(vec![Value::Int(1), Value::string("a")]),
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
                ("a", Value::Int(1)),
                (
                    "c",
                    Value::Float(crate::data::Finite::new(2.5).expect("finite"))
                )
            ]),
            &Fixings::default()
        )
        .is_ok()
    );
    let mixed = map(&[("a", Value::Int(1)), ("c", Value::string("x"))]);
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
    assert!(first.admit(&Value::Int(1), &fixings).is_ok());
    let refused = second
        .admit(&Value::string("x"), &fixings)
        .expect_err("the unit's variable is fixed to numbers");
    assert!(refused.to_string().contains("compares them"));
}

#[test]
fn a_unit_fixes_independently_in_two_stores() {
    let mut u = Unifier::new();
    let v = weak_comparable(&mut u);
    let s = site(&u, &v);
    let (one, other) = (Fixings::default(), Fixings::default());
    assert!(s.admit(&Value::Int(1), &one).is_ok());
    assert!(s.admit(&Value::string("x"), &other).is_ok());
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
    let leaf = map(&[("a", map(&[("b", Value::Int(1))]))]);
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
    let float = refusal(&ground(&Ty::float()), &Value::Int(3));
    assert!(float.contains("the integer 3") && float.contains("a float"));
    let int = ground(&Ty::int())
        .admit(
            &Value::Float(crate::data::Finite::new(2.5).expect("finite")),
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
    let some = Value::Variant {
        label: "some".into(),
        payload: Some(Box::new(Value::Int(1))),
    };
    assert!(s.admit(&some, &Fixings::default()).is_ok());
    let none = Value::Variant {
        label: "none".into(),
        payload: None,
    };
    assert!(s.admit(&none, &Fixings::default()).is_ok());
    let other = Value::Variant {
        label: "other".into(),
        payload: None,
    };
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
        .admit(&Value::string("x"), &Fixings::default())
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
    assert!(a2.admit(&Value::Int(1), &fixings).is_ok());
    assert!(b2.admit(&Value::string("x"), &fixings).is_err());
}

#[test]
fn a_module_closure_is_held_to_its_scheme() {
    use crate::ty::Binders;
    use crate::typecheck::builtins::{fun, pure, thunk};
    let mut u = Unifier::new();
    let block = |param: Ty, result: Ty| thunk(fun(param, pure(result)));
    let ty = closed_record(&[("f", block(Ty::int(), Ty::int()))]);
    let site = site(&u, &ty);
    let record = map(&[(
        "f",
        crate::shell::runtime::block_over(&crate::shell::runtime::Env::new()),
    )]);
    let scheme_of =
        |param: Ty, result: Ty| vec![("f".to_owned(), Binders::close(|_| block(param, result)))];
    assert!(
        site.admit_module(
            &record,
            &scheme_of(Ty::int(), Ty::int()),
            &Fixings::default()
        )
        .is_ok()
    );
    let err = site
        .admit_module(
            &record,
            &scheme_of(Ty::string(), Ty::int()),
            &Fixings::default(),
        )
        .expect_err("a block over text is no block over Int");
    assert!(
        err.to_string()
            .starts_with("the value at `/f` is a block of type")
    );
    assert!(
        matches!(&err.failure.cause, AdmissionCause::Export(AdmissionExport { found, expected }) if found != expected)
    );
    let _ = &mut u;
}

#[test]
fn admission_reasons_keep_structured_causes_and_witnesses_outside_records() {
    use crate::data::datum::Datum as _;
    use crate::fact::Reason;
    let ty = Ty::list(closed_record(&[("size", Ty::int())]));
    let value = Value::list(vec![map(&[("size", Value::string("wrong"))])]);
    let mismatch = ground(&ty).admit(&value, &Fixings::default()).unwrap_err();
    let failure = mismatch.failure().clone();
    assert_eq!(failure.pointer, "/0/size");
    assert!(matches!(
        &failure.cause,
        AdmissionCause::Uses(AdmissionUses {
            expected: Expected::Shape(ValueShape::Int),
            found: Found::Text(_)
        })
    ));
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
    let missing = ground(&closed_record(&[("a", Ty::int())]))
        .admit(&map(&[]), &Fixings::default())
        .unwrap_err();
    assert_eq!(missing.failure.cause, AdmissionCause::NoField("a".into()));
    assert_eq!(missing.failure.pointer, "");
    let extra = ground(&closed_record(&[]))
        .admit(&map(&[("a", Value::Int(1))]), &Fixings::default())
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
