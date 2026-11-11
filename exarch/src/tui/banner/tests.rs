use super::{SessionInfo, legend_panel, session_card};
use crate::card::{FieldVal, Mark, Role};
use crate::tui::palette::{READ_W, content_w};
use crate::tui::{rail, text};

fn sample(base: &'static str) -> SessionInfo<'static> {
    static CWD: std::sync::LazyLock<ral_core::path::CwdPath> =
        std::sync::LazyLock::new(|| ral_core::host::cwd().expect("test process has a cwd"));
    SessionInfo {
        system_size: 4096,
        system_files: &[],
        base,
        extend: None,
        restrict_files: &[],
        cwd: &CWD,
    }
}

fn rows(s: &SessionInfo<'_>) -> Vec<(String, FieldVal)> {
    let card = session_card(s);
    match card.marks() {
        [Mark::Fields { rows }] => rows
            .iter()
            .map(|f| (f.label.clone(), f.value.clone()))
            .collect(),
        other => panic!("session card must be one fields mark, got {other:?}"),
    }
}

/// The role of a row's leading value span; `None` for plain ink or a measure.
fn lead_role(v: &FieldVal) -> Option<Role> {
    match v {
        FieldVal::Inline(spans) => spans.first().and_then(|sp| sp.role),
        FieldVal::Readout(_) => None,
    }
}

#[test]
fn dangerous_base_is_the_one_field_that_earns_a_hue() {
    let base_role = |b: &'static str| {
        let rs = rows(&sample(b));
        lead_role(&rs.iter().find(|(l, _)| l == "base").unwrap().1)
    };
    assert_eq!(base_role("dangerous"), Some(Role::Bad));
    assert_eq!(base_role("read-only"), Some(Role::Strong));
    assert_eq!(base_role("confined"), Some(Role::Strong));
}

/// Guards the derivation: a shape cannot reach the rail unnamed here.
#[test]
fn legend_names_every_rail_shape() {
    let text: String = legend_panel(content_w(READ_W))
        .iter()
        .map(text::text)
        .collect::<Vec<_>>()
        .join("\n");
    for (_, name) in rail::RAIL_SHAPES {
        assert!(text.contains(name), "legend omits the {name:?} shape row");
    }
}
