use crate::builtin::BuiltinEntry;
use crate::data::json::{JsonDocument, decode_structural};
use crate::ir::{CompKind, Val};
use crate::marshal::wire::{Row, Value, Wire};
use crate::marshal::{Marshalled, read};
use crate::shell::grant::SpawnGrant;
use crate::shell::runtime::{
    Binding, Env, Held, Mooring, ValClos, View, block_over, deep_block_chain,
};
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

fn captured(value: &ValClos) -> &Env {
    let View::Thunk { env, .. } = value.view() else {
        panic!("expected a captured block");
    };
    env
}

fn wire(parent: &Marshalled) -> Wire {
    let document = JsonDocument::parse(&parent.0).expect("marshalled JSON");
    let wire: Wire = decode_structural(document.root()).expect("marshalled structure");
    for (before, row) in wire.rows.iter().enumerate() {
        let check = |value: &Value| {
            let (id, environment) = match value {
                Value::Closure { env: Some(id), .. } | Value::Residual { env: id, .. } => {
                    (*id, true)
                }
                Value::Built(id) => (*id, false),
                _ => return,
            };
            assert!((id as usize) < before, "a row names only earlier rows");
            assert_eq!(matches!(wire.rows[id as usize], Row::Env(_)), environment);
        };
        match row {
            Row::Env(entries) => entries
                .iter()
                .for_each(|(_, binding)| check(&binding.value)),
            Row::List(items) => items.iter().for_each(check),
            Row::Map(entries) => entries.iter().for_each(|(_, value)| check(value)),
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

fn builtin_owner(value: &ValClos) -> Weak<BuiltinEntry> {
    let View::Builtin(entry) = value.view() else {
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
    assert!(encoded.rows.iter().all(|row| matches!(row, Row::Env(_))));
    let (image, _) = read(&shell.session.runtime, &parent).unwrap();
    let a = captured(image.env.get("a").unwrap());
    let b = captured(image.env.get("b").unwrap());
    let c = captured(image.env.get("c").unwrap());
    assert!(a.ptr_eq(b), "aliases still share their capture");
    assert!(!a.ptr_eq(c), "equal but distinct captures stay distinct");
}

#[test]
fn built_collections_are_interned_by_allocation_not_contents() {
    let mut shell = test_helper::core_shell();
    let list = ValClos::list(vec![ValClos::int(7)]);
    let map = ValClos::map(vec![("x".into(), list.clone())]);
    let other_map = ValClos::map(vec![("x".into(), list.clone())]);
    for (name, value) in [
        ("list", list.clone()),
        ("list_alias", list),
        ("other_list", ValClos::list(vec![ValClos::int(7)])),
        ("map", map.clone()),
        ("map_alias", map),
        ("other_map", other_map),
    ] {
        bind(&mut shell, name, value);
    }
    let parent = shell.marshal(&SpawnGrant::Inherit).unwrap();
    let encoded = wire(&parent);
    assert_eq!(encoded.rows.len(), 5, "two lists, two maps and the session");
    assert_eq!(
        encoded
            .rows
            .iter()
            .filter(|row| matches!(row, Row::List(_)))
            .count(),
        2
    );
    assert_eq!(
        encoded
            .rows
            .iter()
            .filter(|row| matches!(row, Row::Map(_)))
            .count(),
        2
    );

    let (image, _) = read(&shell.session.runtime, &parent).unwrap();
    let list = |name| match image.env.get(name).unwrap().held() {
        Held::List(items) => items,
        _ => panic!("built list stays built"),
    };
    let map = |name| match image.env.get(name).unwrap().held() {
        Held::Map(entries) => entries,
        _ => panic!("built map stays built"),
    };
    assert!(std::ptr::eq(list("list"), list("list_alias")));
    assert!(!std::ptr::eq(list("list"), list("other_list")));
    assert!(std::ptr::eq(map("map"), map("map_alias")));
    assert!(!std::ptr::eq(map("map"), map("other_map")));
    for name in ["map", "map_alias", "other_map"] {
        let Held::List(items) = map(name).get("x").unwrap().held() else {
            panic!("map entry stays a built list");
        };
        assert!(std::ptr::eq(items, list("list")));
    }
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
fn one_built_map_captured_by_three_closures_is_written_once() {
    let mut shell = test_helper::core_shell();
    let map = ValClos::map(vec![("x".into(), ValClos::int(7))]);
    let env = environment("map", map.clone());
    bind(&mut shell, "map", map);
    for name in ["a", "b", "c"] {
        bind(&mut shell, name, capture("map", &env));
    }
    let parent = shell.marshal(&SpawnGrant::Inherit).unwrap();
    let encoded = wire(&parent);
    assert_eq!(
        encoded.rows.len(),
        3,
        "one map, one capture and the session"
    );
    assert_eq!(
        encoded
            .rows
            .iter()
            .filter(|row| matches!(row, Row::Map(_)))
            .count(),
        1
    );
    let (image, _) = read(&shell.session.runtime, &parent).unwrap();
    let Held::Map(root) = image.env.get("map").unwrap().held() else {
        panic!("map stays built");
    };
    for name in ["a", "b", "c"] {
        let Held::Map(capture) = captured(image.env.get(name).unwrap())
            .get("map")
            .unwrap()
            .held()
        else {
            panic!("capture stays built");
        };
        assert!(std::ptr::eq(root, capture));
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
        let mut cursor = &back;
        for _ in 0..100_000 {
            let Held::List(items) = cursor.held() else {
                panic!("every list stays built");
            };
            assert_eq!(items.len(), 1);
            cursor = &items[0];
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
                let Row::Env(root) = &encoded.rows[encoded.env.unwrap() as usize] else {
                    panic!("root is an environment");
                };
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
                let (labels, terminal) = back.peel_variants();
                assert_eq!(labels.len(), 10_000);
                assert!(labels.iter().all(|label| label.as_ref() == "deep"));
                assert_eq!(terminal.shape(), shape);
                drop(terminal);
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
        let mut cursor = &back;
        for _ in 0..30_000 {
            let Held::List(items) = cursor.held() else {
                panic!("outer container stays a built list");
            };
            let Held::Map(entries) = items[0].held() else {
                panic!("inner container stays a built map");
            };
            cursor = captured(entries.get("closure").unwrap())
                .get("tail")
                .unwrap();
        }
        let decoded_owner = builtin_owner(cursor);
        release_concurrently(value);
        release_concurrently(back);
        assert!(original_owner.upgrade().is_none());
        assert!(decoded_owner.upgrade().is_none());
    });
}
