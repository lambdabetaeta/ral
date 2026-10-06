use super::*;
use crate::source::FileId;
use crate::ty::Ty;
use crate::ty::{closed_record, closed_variant, open_record};
use crate::typecheck::Unifier;

fn site(u: &Unifier, ty: &Ty) -> Site {
    Site::snapshot(u, ty, Fixings::new())
}

fn ground(ty: &Ty) -> Site {
    site(&Unifier::new(), ty)
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
    site.admit(value)
        .expect_err("the value must be refused")
        .to_string()
}

#[test]
fn a_value_of_the_solved_type_is_admitted() {
    let ty = closed_record(&[("a", Ty::Int), ("b", Ty::list(Ty::String))]);
    let value = map(&[
        ("a", Value::Int(1)),
        ("b", Value::list(vec![Value::string("x")])),
    ]);
    assert!(ground(&ty).admit(&value).is_ok());
}

#[test]
fn a_deep_mismatch_names_its_pointer() {
    let ty = Ty::list(closed_record(&[("size", Ty::Int)]));
    let value = Value::list(vec![
        map(&[("size", Value::Int(1))]),
        map(&[("size", Value::string("12kB"))]),
    ]);
    assert_eq!(
        refusal(&ground(&ty), &value),
        "the value at `/1/size` is text (`'12kB'`), but this script uses it as an Int"
    );
}

#[test]
fn a_key_with_a_slash_or_a_tilde_is_escaped_in_its_pointer() {
    let ty = Ty::map(Ty::Int);
    let value = map(&[("a/b~c", Value::string("x"))]);
    assert!(refusal(&ground(&ty), &value).contains("`/a~1b~0c`"));
}

#[test]
fn an_open_row_admits_extra_fields_and_a_closed_one_names_them() {
    let mut u = Unifier::new();
    let tail = u.fresh_row_var();
    let value = map(&[("a", Value::Int(1)), ("extra", Value::Bool(true))]);
    let open = open_record(&[("a", Ty::Int)], tail);
    assert!(site(&u, &open).admit(&value).is_ok());
    let closed = closed_record(&[("a", Ty::Int)]);
    assert!(refusal(&ground(&closed), &value).contains("has a field `extra`"));
}

#[test]
fn a_missing_field_is_named() {
    let ty = closed_record(&[("verdict", Ty::String)]);
    let message = refusal(&ground(&ty), &map(&[]));
    assert_eq!(
        message,
        "the value has no field `verdict`, which this script reads"
    );
}

#[test]
fn a_kinded_variable_admits_only_its_heads() {
    let mut u = Unifier::new();
    let v = u.fresh_kinded(Kind::NUMBER);
    let s = site(&u, &Ty::list(v));
    assert!(
        s.admit(&Value::list(vec![
            Value::Int(1),
            Value::Float(crate::first_order::Finite::new(2.5).expect("finite"))
        ]))
        .is_ok()
    );
    assert_eq!(
        refusal(&s, &Value::list(vec![Value::string("x")])),
        "the value at `/0` is text (`'x'`), but this script uses it as a number"
    );
}

#[test]
fn a_free_variable_admits_anything_a_parametric_use_could_hold() {
    let mut u = Unifier::new();
    let v = u.fresh_ty();
    let s = site(&u, &Ty::list(v));
    assert!(
        s.admit(&Value::list(vec![Value::Int(1), Value::string("a")]))
            .is_ok()
    );
}

#[test]
fn a_comparable_variable_is_fixed_to_numbers_or_to_text() {
    let mut u = Unifier::new();
    let v = u.fresh_kinded(Kind::COMPARABLE);
    let ty = closed_record(&[("a", v.clone()), ("c", v)]);
    let s = site(&u, &ty);
    assert!(
        s.admit(&map(&[
            ("a", Value::Int(1)),
            (
                "c",
                Value::Float(crate::first_order::Finite::new(2.5).expect("finite"))
            )
        ]))
        .is_ok()
    );
    let mixed = map(&[("a", Value::Int(1)), ("c", Value::string("x"))]);
    assert_eq!(
        refusal(&s, &mixed),
        "`/a` is a number and `/c` is text, and this script compares them"
    );
}

#[test]
fn fixings_are_shared_by_every_site_of_a_unit() {
    let mut u = Unifier::new();
    let v = u.fresh_kinded(Kind::COMPARABLE);
    let fixings = Fixings::new();
    let first = Site::snapshot(&u, &v, Arc::clone(&fixings));
    let second = Site::snapshot(&u, &v, fixings);
    assert!(first.admit(&Value::Int(1)).is_ok());
    assert!(refusal(&second, &Value::string("x")).contains("compares them"));
}

#[test]
fn a_recursive_type_admits_a_nested_document_and_refuses_a_scalar_leaf() {
    let mut u = Unifier::new();
    let v = u.fresh_tyvar();
    u.unify_ty(&Ty::Var(v), &Ty::map(Ty::Var(v)))
        .expect("a cycle through a map is data");
    let s = site(&u, &Ty::Var(v));
    let nested = map(&[("a", map(&[("b", map(&[]))]))]);
    assert!(s.admit(&nested).is_ok());
    let leaf = map(&[("a", map(&[("b", Value::Int(1))]))]);
    assert_eq!(
        refusal(&s, &leaf),
        "the value at `/a/b` is a whole number (`1`), but this script uses it as a map"
    );
}

#[test]
fn a_json_number_keeps_its_kind_at_a_ground_site() {
    let float = refusal(&ground(&Ty::Float), &Value::Int(3));
    assert!(float.contains("a whole number (`3`)") && float.contains("as a Float"));
    let int = ground(&Ty::Int)
        .admit(&Value::Float(
            crate::first_order::Finite::new(2.5).expect("finite"),
        ))
        .expect_err("a fraction is not an Int");
    assert!(
        int.hint()
            .is_some_and(|hint| hint.contains("`float` or `int`"))
    );
}

#[test]
fn a_variant_site_needs_a_tag_of_its_row() {
    let ty = closed_variant(&[("none", Ty::Unit), ("some", Ty::Int)]);
    let s = ground(&ty);
    let some = Value::Variant {
        label: "some".into(),
        payload: Some(Box::new(Value::Int(1))),
    };
    assert!(s.admit(&some).is_ok());
    let none = Value::Variant {
        label: "none".into(),
        payload: None,
    };
    assert!(s.admit(&none).is_ok());
    let other = Value::Variant {
        label: "other".into(),
        payload: None,
    };
    assert!(refusal(&s, &other).contains("one of the tags `none`, `some`"));
    assert!(refusal(&s, &map(&[])).contains("uses it as a variant"));
}

#[test]
fn a_handle_site_admits_the_value_the_worker_settled_with() {
    let s = ground(&Ty::Handle(Box::new(Ty::Int)));
    assert!(s.admit_handled(&Value::Int(1)).is_ok());
    assert!(refusal_of(s.admit_handled(&Value::string("x"))).contains("uses it as an Int"));
}

fn refusal_of(result: Result<(), Mismatch>) -> String {
    result.expect_err("the value must be refused").to_string()
}

#[test]
fn the_witness_is_the_use_that_fixed_the_structure() {
    let mut u = Unifier::new();
    let at = Span::new(FileId::DUMMY, 3, 9);
    u.at = Some(at);
    let v = u.fresh_tyvar();
    u.unify_ty(&Ty::Var(v), &Ty::Int)
        .expect("a free variable meets Int");
    let mismatch = site(&u, &Ty::Var(v))
        .admit(&Value::string("x"))
        .expect_err("text is not an Int");
    assert_eq!(mismatch.witness(), Some(at));
}

#[test]
fn a_site_round_trips_the_wire_and_its_units_sites_meet_one_fixings() {
    let mut u = Unifier::new();
    let v = u.fresh_kinded(Kind::COMPARABLE);
    let fixings = Fixings::new();
    let (a, b) = (
        Site::snapshot(&u, &v, Arc::clone(&fixings)),
        Site::snapshot(&u, &v, fixings),
    );
    let wire = |site: &Site| postcard::to_allocvec(site).expect("a site serialises");
    let (a2, b2): (Site, Site) = (
        postcard::from_bytes(&wire(&a)).expect("and reads back"),
        postcard::from_bytes(&wire(&b)).expect("and reads back"),
    );
    assert_eq!(a2.nodes, a.nodes);
    assert!(Arc::ptr_eq(&a2.fixings, &b2.fixings));
    assert!(a2.admit(&Value::Int(1)).is_ok());
    assert!(b2.admit(&Value::string("x")).is_err());
}

#[test]
fn a_module_closure_is_held_to_its_scheme() {
    use crate::typecheck::builtins::{fun, mk_plain_scheme, pure, thunk};
    let mut u = Unifier::new();
    let block = |param: Ty, result: Ty| thunk(fun(param, pure(result)));
    let ty = closed_record(&[("f", block(Ty::Int, Ty::Int))]);
    let site = site(&u, &ty);
    let record = map(&[("f", crate::types::block_over(&crate::types::Env::new()))]);
    let scheme_of = |param: Ty, result: Ty| {
        vec![(
            "f".to_owned(),
            Arc::new(mk_plain_scheme(&[], &[], block(param, result))),
        )]
    };
    assert!(
        site.admit_module(&record, &scheme_of(Ty::Int, Ty::Int))
            .is_ok()
    );
    let err = site
        .admit_module(&record, &scheme_of(Ty::String, Ty::Int))
        .expect_err("a block over text is no block over Int");
    assert!(
        err.to_string()
            .starts_with("the value at `/f` is a block of type")
    );
    let _ = &mut u;
}
