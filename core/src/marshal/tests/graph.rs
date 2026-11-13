use crate::builtin::BuiltinEntry;
use crate::data::json::{JsonDocument, decode_structural};
use crate::ir::{
    Binding, CompKind, Env, VClosRef, Val, ValClos, View, block_over, deep_block_chain,
};
use crate::marshal::wire::{self, Row, Value, Wire};
use crate::marshal::{Marshalled, read};
use crate::shell::grant::SpawnGrant;
use crate::shell::runtime::Mooring;
use crate::source::Spanned;
use crate::{Shell, test_helper};
use std::sync::{Arc, Barrier, Weak};

fn bind(shell: &mut Shell, name: &str, value: ValClos) {
    shell.env.bind(
        name.into(),
        Binding {
            value,
            scheme: None,
        },
    );
}

fn environment(name: &str, value: ValClos) -> Env {
    Env::from_bindings(vec![(
        name.into(),
        Binding {
            value,
            scheme: None,
        },
    )])
    .expect("one distinct binding")
}

fn capture(name: &str, env: &Env) -> ValClos {
    ValClos::thunk(
        Arc::new(Spanned::synthetic(CompKind::Return(Val::Variable(
            name.into(),
        )))),
        env,
    )
}

fn row_of<'a>(env: &'a Env, name: &str) -> &'a Env {
    env.get(name).expect("bound").parts().1
}

fn captured(value: &ValClos) -> &Env {
    let View::Thunk { env, .. } = value.view() else {
        panic!("expected a captured block");
    };
    env
}

fn wire(parent: &Marshalled) -> Wire {
    let document = JsonDocument::parse(&parent.0).expect("marshalled JSON");
    let wire: Wire = decode_structural(document.root()).expect("marshalled structure");
    for (before, Row::Env(entries)) in wire.rows.iter().enumerate() {
        for (_, binding) in entries {
            if let Value::Closure { env: Some(id), .. } | Value::Residual { env: id, .. } =
                &binding.value
            {
                assert!((*id as usize) < before, "a row names only earlier rows");
            }
        }
    }
    wire
}

fn round_trip(value: &ValClos) -> ValClos {
    let mut shell = test_helper::core_shell();
    bind(&mut shell, "value", value.clone());
    let parent = shell.marshal(&SpawnGrant::Inherit).expect("marshal graph");
    let (image, _) = read(&shell.session.runtime, &parent).expect("read graph");
    image.env.get("value").expect("root survives").clone()
}

fn small_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(f)
        .expect("spawn small-stack thread")
        .join()
        .expect("marshal, read and runtime drop fit on a 256 KiB stack");
}

fn release_concurrently(value: ValClos) {
    let aliases = [value.clone(), value.clone(), value.clone(), value];
    let barrier = Arc::new(Barrier::new(aliases.len()));
    let threads: Vec<_> = aliases
        .into_iter()
        .map(|value| {
            let barrier = Arc::clone(&barrier);
            std::thread::Builder::new()
                .stack_size(256 * 1024)
                .spawn(move || {
                    barrier.wait();
                    drop(value);
                })
                .expect("spawn concurrent runtime drop")
        })
        .collect();
    for thread in threads {
        thread.join().expect("concurrent runtime drop completes");
    }
}

fn marker() -> (ValClos, Weak<BuiltinEntry>) {
    let entry = Arc::new(
        test_helper::core_runtime()
            .builtins
            .get("has")
            .expect("has builtin"),
    );
    let weak = Arc::downgrade(&entry);
    (ValClos::builtin(entry), weak)
}

fn builtin_owner<'a>(value: impl Into<VClosRef<'a>>) -> Weak<BuiltinEntry> {
    let View::Builtin(entry) = value.into().view() else {
        panic!("the chain's builtin foot survives");
    };
    Arc::downgrade(entry)
}

#[test]
fn environments_are_interned_by_allocation_not_contents() {
    let mut shell = test_helper::core_shell();
    let shared = environment("x", ValClos::int(7));
    let distinct = environment("x", ValClos::int(7));
    assert!(!shared.ptr_eq(&distinct));
    bind(&mut shell, "a", capture("x", &shared));
    bind(&mut shell, "b", capture("x", &shared));
    bind(&mut shell, "c", capture("x", &distinct));

    let parent = shell.marshal(&SpawnGrant::Inherit).unwrap();
    let encoded = wire(&parent);
    assert_eq!(encoded.rows.len(), 3, "two captures and the session");
    let (image, _) = read(&shell.session.runtime, &parent).unwrap();
    let a = captured(image.env.get("a").unwrap());
    let b = captured(image.env.get("b").unwrap());
    let c = captured(image.env.get("c").unwrap());
    assert!(a.ptr_eq(b), "aliases still share their capture");
    assert!(!a.ptr_eq(c), "equal but distinct captures stay distinct");
}

#[test]
fn built_collections_share_a_row_by_allocation_not_contents() {
    let mut shell = test_helper::core_shell();
    // A handle is never inlined, so each collection holding one has a row.
    let handle = ValClos::withheld("h");
    let list = ValClos::list(vec![handle.clone()]);
    let map = ValClos::map(vec![("x".into(), list.clone())]);
    let other_map = ValClos::map(vec![("x".into(), list.clone())]);
    for (name, value) in [
        ("list", list.clone()),
        ("list_alias", list),
        ("other_list", ValClos::list(vec![handle])),
        ("map", map.clone()),
        ("map_alias", map),
        ("other_map", other_map),
    ] {
        bind(&mut shell, name, value);
    }
    let parent = shell.marshal(&SpawnGrant::Inherit).unwrap();
    let encoded = wire(&parent);
    assert_eq!(encoded.rows.len(), 5, "two lists, two maps and the session");

    let (image, _) = read(&shell.session.runtime, &parent).unwrap();
    let row = |name| row_of(&image.env, name);
    assert!(row("list").ptr_eq(row("list_alias")));
    assert!(!row("list").ptr_eq(row("other_list")));
    assert!(row("map").ptr_eq(row("map_alias")));
    assert!(!row("map").ptr_eq(row("other_map")));
    for name in ["map", "map_alias", "other_map"] {
        let View::Map(entries) = image.env.get(name).unwrap().view() else {
            panic!("{name} stays a map");
        };
        let element = entries.get("x").unwrap().into_owned();
        assert!(element.parts().1.ptr_eq(row("list")));
    }
}

#[test]
fn a_built_list_crosses_as_a_closure_over_its_row() {
    let mut shell = test_helper::core_shell();
    bind(
        &mut shell,
        "list",
        ValClos::list(vec![ValClos::withheld("h"), ValClos::int(7)]),
    );
    let parent = shell.marshal(&SpawnGrant::Inherit).unwrap();
    let encoded = wire(&parent);
    let [Row::Env(items), Row::Env(session)] = encoded.rows.as_slice() else {
        panic!("the list's row and the session's");
    };
    assert!(matches!(
        items.as_slice(),
        [(name, wire::Binding { value: Value::Handle(_), .. })] if name == "%item0"
    ));
    let [(_, binding)] = session.as_slice() else {
        panic!("the session binds the list alone");
    };
    let Value::Closure {
        tags,
        val: Val::List(node),
        env: Some(0),
    } = &binding.value
    else {
        panic!("the list is a closure over row 0");
    };
    assert_eq!(tags, &Vec::<String>::new());
    assert_eq!(node.shape().len(), 2, "the 7 is inline syntax");

    let (image, _) = read(&shell.session.runtime, &parent).unwrap();
    let View::List(items) = image.env.get("list").unwrap().view() else {
        panic!("the list stays a list");
    };
    assert!(matches!(items.get(0).unwrap().view(), View::Handle(_)));
    assert_eq!(items.get(1).unwrap().as_int(), Some(7));
}

#[test]
fn forty_definitions_write_forty_rows_not_exponentially_many() {
    let mut shell = test_helper::core_shell();
    for i in 0..40 {
        let block = block_over(&shell.env);
        bind(&mut shell, &format!("d{i}"), block);
    }
    let parent = shell.marshal(&SpawnGrant::Inherit).unwrap();
    assert_eq!(wire(&parent).rows.len(), 40);
    let (image, _) = read(&shell.session.runtime, &parent).unwrap();
    assert_eq!(image.env.len(), 40);
    for i in 0..40 {
        assert_eq!(captured(image.env.get(&format!("d{i}")).unwrap()).len(), i);
    }
}

#[test]
fn one_built_list_captured_by_three_closures_is_written_once() {
    let mut shell = test_helper::core_shell();
    let list = ValClos::list(vec![ValClos::withheld("h")]);
    let env = environment("list", list.clone());
    bind(&mut shell, "list", list);
    for name in ["a", "b", "c"] {
        bind(&mut shell, name, capture("list", &env));
    }
    let parent = shell.marshal(&SpawnGrant::Inherit).unwrap();
    assert_eq!(
        wire(&parent).rows.len(),
        3,
        "one list, one capture and the session"
    );
    let (image, _) = read(&shell.session.runtime, &parent).unwrap();
    let root = row_of(&image.env, "list");
    for name in ["a", "b", "c"] {
        assert!(row_of(captured(image.env.get(name).unwrap()), "list").ptr_eq(root));
    }
}

#[test]
fn an_empty_capture_names_no_row() {
    let mut shell = test_helper::core_shell();
    bind(&mut shell, "unused", ValClos::int(7));
    let block = ValClos::thunk(
        Arc::new(Spanned::synthetic(CompKind::Return(Val::Unit))),
        &shell.env,
    );
    assert!(captured(&block).is_empty());
    bind(&mut shell, "block", block);
    let parent = shell.marshal(&SpawnGrant::Inherit).unwrap();
    let encoded = wire(&parent);
    let [Row::Env(entries)] = encoded.rows.as_slice() else {
        panic!("only the session has a row");
    };
    let (_, binding) = entries.iter().find(|(name, _)| name == "block").unwrap();
    assert!(matches!(binding.value, Value::Closure { env: None, .. }));
}

#[test]
fn the_session_row_carries_no_prelude_binding() {
    let mut shell = crate::shell::boot::boot_shell(
        crate::io::terminal::TerminalState::default(),
        crate::shell::boot::BakedPrelude::runtime(),
        &crate::shell::boot::HostSurface::default(),
        test_helper::process_cwd(),
    )
    .expect("boot with the prelude");
    bind(&mut shell, "only_mine", ValClos::int(1));
    let parent = shell.marshal(&SpawnGrant::Inherit).unwrap();
    let encoded = wire(&parent);
    let [Row::Env(entries)] = encoded.rows.as_slice() else {
        panic!("only the session has a row");
    };
    assert_eq!(
        entries
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["only_mine"]
    );
}

#[test]
fn a_hundred_thousand_blocks_round_trip_and_drop_on_small_stacks() {
    small_stack(|| {
        let (foot, original_owner) = marker();
        let chain = deep_block_chain(100_000, foot);
        let back = round_trip(&chain);
        let mut cursor = &back;
        for _ in 0..100_000 {
            cursor = captured(cursor)
                .get("tail")
                .expect("each block keeps its tail");
        }
        let decoded_owner = builtin_owner(cursor);
        release_concurrently(chain);
        release_concurrently(back);
        assert!(original_owner.upgrade().is_none());
        assert!(decoded_owner.upgrade().is_none());
    });
}

#[test]
fn thirty_thousand_residuals_round_trip_and_drop_on_small_stacks() {
    small_stack(|| {
        let mut shell = test_helper::core_shell();
        let entry = Arc::new(shell.lookup_builtin("has").expect("has builtin"));
        let (mut value, original_owner) = marker();
        for _ in 0..30_000 {
            value = crate::shell::evaluator::apply(
                &ValClos::builtin(Arc::clone(&entry)),
                vec![value],
                &Mooring::adrift(),
                &mut shell,
            )
            .expect("partial has");
        }
        let back = round_trip(&value);
        let mut cursor = &back;
        for _ in 0..30_000 {
            let env = captured(cursor);
            cursor = env
                .get(crate::ir::synthetic("arg", 0).as_ref())
                .expect("residual keeps its argument");
        }
        let decoded_owner = builtin_owner(cursor);
        release_concurrently(value);
        release_concurrently(back);
        assert!(original_owner.upgrade().is_none());
        assert!(decoded_owner.upgrade().is_none());
    });
}

#[test]
fn a_hundred_thousand_built_lists_round_trip_and_drop_on_small_stacks() {
    small_stack(|| {
        let (mut value, original_owner) = marker();
        for _ in 0..100_000 {
            value = ValClos::list(vec![value]);
        }
        let back = round_trip(&value);
        let mut cursor = VClosRef::from(&back);
        for _ in 0..100_000 {
            let View::List(items) = cursor.view() else {
                panic!("every list stays a list");
            };
            assert_eq!(items.len(), 1);
            cursor = items.get(0).unwrap();
        }
        let decoded_owner = builtin_owner(cursor);
        release_concurrently(value);
        release_concurrently(back);
        assert!(original_owner.upgrade().is_none());
        assert!(decoded_owner.upgrade().is_none());
    });
}

#[test]
fn ten_thousand_tags_round_trip_flat_and_env_broken_on_small_stacks() {
    small_stack(|| {
        for broken in [false, true] {
            for foot in [
                ValClos::UNIT,
                ValClos::int(7),
                ValClos::list(vec![ValClos::int(7)]),
                ValClos::withheld("deep tags"),
                capture("x", &environment("x", ValClos::int(7))),
            ] {
                let shape = foot.shape();
                let mut value = foot;
                for depth in 0..10_000 {
                    value = if broken && depth == 5_000 {
                        ValClos::closure(
                            vec!["deep".into()],
                            Val::Variable("tail".into()),
                            environment("tail", value),
                        )
                        .expect("tag's foot is bound")
                    } else {
                        ValClos::variant("deep", Some(value))
                    };
                }
                let mut shell = test_helper::core_shell();
                bind(&mut shell, "value", value.clone());
                let parent = shell.marshal(&SpawnGrant::Inherit).unwrap();
                let encoded = wire(&parent);
                let Row::Env(root) = &encoded.rows[encoded.env.unwrap() as usize];
                let Value::Closure { tags, val, .. } = &root[0].1.value else {
                    panic!("tag spine stays a closure");
                };
                assert_eq!(tags.len(), if broken { 5_000 } else { 10_000 });
                assert!(!matches!(val, Val::Variant { .. }), "wire spine is flat");
                drop(encoded);
                let (image, _) = read(&shell.session.runtime, &parent).unwrap();
                let back = image.env.get("value").unwrap().clone();
                drop(image);
                drop(shell);
                let (labels, terminal) = VClosRef::from(&back).peel_variants();
                assert_eq!(labels.len(), 10_000);
                assert!(labels.iter().all(|label| label.as_ref() == "deep"));
                assert_eq!(terminal.shape(), shape);
                drop(labels);
                release_concurrently(value);
                release_concurrently(back);
            }
        }
    });
}

#[test]
fn shared_captures_inside_built_containers_drop_on_small_stacks() {
    small_stack(|| {
        let (mut value, original_owner) = marker();
        for _ in 0..30_000 {
            value = ValClos::list(vec![ValClos::map(vec![(
                "closure".into(),
                capture("tail", &environment("tail", value)),
            )])]);
        }
        let back = round_trip(&value);
        let mut cursor = VClosRef::from(&back);
        for _ in 0..30_000 {
            let View::List(items) = cursor.view() else {
                panic!("outer container stays a list");
            };
            let View::Map(entries) = items.get(0).unwrap().view() else {
                panic!("inner container stays a map");
            };
            let View::Thunk { env, .. } = entries.get("closure").unwrap().view() else {
                panic!("the entry stays a capture");
            };
            cursor = env.get("tail").unwrap().into();
        }
        let decoded_owner = builtin_owner(cursor);
        release_concurrently(value);
        release_concurrently(back);
        assert!(original_owner.upgrade().is_none());
        assert!(decoded_owner.upgrade().is_none());
    });
}
