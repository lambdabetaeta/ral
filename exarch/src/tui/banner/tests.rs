use super::{SessionInfo, legend_panel, session_card};
use crate::card::{FieldVal, Mark, Role};
use crate::tui::palette::{READ_W, content_w};
use crate::tui::{line, rail};
use std::path::PathBuf;

fn sample(base: &'static str) -> SessionInfo<'static> {
    SessionInfo {
        system_size: 4096,
        system_files: &[],
        base,
        extend_base: None,
        restrict_files: &[],
        cwd: "/Users/me/projects/ral",
        resumed: None,
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
fn session_card_orders_and_roles_fields() {
    let rs = rows(&sample("read-only"));
    let labels: Vec<&str> = rs.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(
        labels,
        [
            "version",
            "cwd",
            "base",
            "extend-base",
            "restrict",
            "system prompt",
        ]
    );
    let role = |label: &str| lead_role(&rs.iter().find(|(l, _)| l == label).unwrap().1);
    assert_eq!(
        role("version"),
        Some(Role::Strong),
        "version names the binary"
    );
    assert_eq!(role("cwd"), Some(Role::Path), "cwd is a path");
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

#[test]
fn security_paths_are_roled_present_and_muted_when_absent() {
    let rs = rows(&sample("read-only"));
    assert_eq!(
        lead_role(&rs.iter().find(|(l, _)| l == "extend-base").unwrap().1),
        Some(Role::Muted),
        "absent extend-base is muted none"
    );

    let ext = PathBuf::from("/policy/base.ral");
    let restr = vec![PathBuf::from("src/lib.rs")];
    let mut s = sample("read-only");
    s.extend_base = Some(ext.as_path());
    s.restrict_files = &restr;
    let rs = rows(&s);
    assert_eq!(
        lead_role(&rs.iter().find(|(l, _)| l == "extend-base").unwrap().1),
        Some(Role::Path)
    );
    assert_eq!(
        lead_role(&rs.iter().find(|(l, _)| l == "restrict").unwrap().1),
        Some(Role::Path)
    );
}

/// Guards the derivation: a shape cannot reach the rail unnamed here.
#[test]
fn legend_names_every_rail_shape() {
    let text: String = legend_panel(content_w(READ_W))
        .iter()
        .map(line::text)
        .collect::<Vec<_>>()
        .join("\n");
    for (_, name) in rail::RAIL_SHAPES {
        assert!(text.contains(name), "legend omits the {name:?} shape row");
    }
}
