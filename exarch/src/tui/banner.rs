//! Session metadata and the visual-vocabulary legend.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::bus::card::{Card, Field, FieldVal, Mark, Role, Span as CardSpan};

use super::block::AgentSlot;
use super::fidelity::Fidelity;
use super::line;
use super::md;
use super::palette::{AGENT_HUES, CODE_BG, READ_W, SLATE};
use super::rail::{self, RailKind};
use super::status::{ctx_ramp, wait_bar};

pub(super) const ART: &str = include_str!("../../data/banner.txt");
pub(super) const EAGLE: &str = include_str!("../../data/eagle.txt");
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Metadata shown in the startup banner.
pub struct SessionInfo<'a> {
    pub system_size: usize,
    pub system_files: &'a [PathBuf],
    pub base: &'a str,
    pub extend_base: Option<&'a Path>,
    pub restrict_files: &'a [PathBuf],
    pub scratch: &'a Path,
    pub cwd: &'a str,
}

/// The startup metadata matrix.  Hue is spent only where it names something:
/// paths carry Path, a `dangerous` base alarms, quantities stay plain ink.
pub(super) fn session_card(s: &SessionInfo<'_>) -> Card {
    let mut rows = vec![
        meta_field("version", vec![CardSpan::new(Role::Strong, VERSION)]),
        meta_field("cwd", vec![CardSpan::new(Role::Path, s.cwd)]),
    ];

    // Provider, model and context window are absent by design: the status bar
    // carries them live and repaints on `/model` (`App::update_live_model`).
    let base_role = if s.base == "dangerous" {
        Role::Bad
    } else {
        Role::Strong
    };
    rows.push(meta_field("base", vec![CardSpan::new(base_role, s.base)]));

    rows.push(meta_field(
        "extend-base",
        match s.extend_base {
            Some(p) => vec![CardSpan::new(Role::Path, p.display().to_string())],
            None => vec![CardSpan::new(Role::Muted, "none")],
        },
    ));

    rows.push(meta_field(
        "restrict",
        if s.restrict_files.is_empty() {
            vec![CardSpan::new(Role::Muted, "none")]
        } else {
            vec![CardSpan::new(Role::Path, join_paths(s.restrict_files))]
        },
    ));

    #[allow(
        clippy::cast_precision_loss,
        reason = "byte count of system prompt; display only"
    )]
    let system_size = s.system_size as f64;
    let sz = format!("{:.1} kB", system_size / 1024.0);
    let mut sys_val = vec![CardSpan::plain(sz), CardSpan::new(Role::Muted, " · ")];
    if s.system_files.is_empty() {
        sys_val.push(CardSpan::new(Role::Muted, "default"));
    } else {
        sys_val.push(CardSpan::new(Role::Path, join_paths(s.system_files)));
    }
    rows.push(meta_field("system prompt", sys_val));

    rows.push(meta_field(
        "scratch",
        vec![CardSpan::new(Role::Path, s.scratch.display().to_string())],
    ));

    Card(vec![Mark::Fields { rows }])
}

pub(super) fn meta_field(label: &str, value: Vec<CardSpan>) -> Field {
    Field {
        label: label.to_string(),
        value: FieldVal::Inline(value),
    }
}

pub(super) fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `/legend` panel: every rail shape, agent hue, value step, stratum, bar
/// and fidelity grade, drawn by the real builders rather than redescribed, so a
/// palette or shape change shows up here with no edit.
pub(super) fn legend_panel() -> Vec<Line<'static>> {
    let head = |s: &str| {
        Line::from(Span::styled(
            s.to_string(),
            Style::default().fg(SLATE).add_modifier(Modifier::BOLD),
        ))
    };
    let note = |s: &str| Span::styled(s.to_string(), Style::default().fg(SLATE));

    let mut ls: Vec<Line<'static>> = vec![
        Line::default(),
        head("legend — the transcript as a graphic"),
    ];

    ls.push(Line::default());
    ls.push(head("rail · shape = block kind"));
    ls.extend(line::legend_rows(
        rail::RAIL_SHAPES
            .iter()
            .map(|(kind, name)| (*name, vec![rail::span(*kind, AgentSlot(0), None)]))
            .collect(),
    ));
    ls.push(Line::default());
    ls.push(head("rail · hue = which agent (constant down a tab)"));
    ls.extend(line::legend_rows(
        (0..AGENT_HUES.len())
            .map(|slot| {
                let label = if slot == 0 { "root" } else { "subagent" };
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "slot indexes AGENT_HUES (len 6), fits u8"
                )]
                let agent_slot = slot as u8;
                (
                    label,
                    vec![rail::span(
                        RailKind::ToolCall(false),
                        AgentSlot(agent_slot),
                        None,
                    )],
                )
            })
            .collect(),
    ));
    ls.push(Line::default());
    ls.push(head("rail · value = magnitude (brighter is bigger)"));
    // One magnitude per `rail::value_step` bucket, so the row is the ramp
    // itself; these numbers must keep tracking that function's thresholds.
    ls.extend(line::legend_rows(vec![(
        "small → large",
        [Some(4), Some(20), Some(80), Some(200)]
            .into_iter()
            .map(|mag| rail::span(RailKind::Patch, AgentSlot(0), mag))
            .collect(),
    )]));

    ls.push(Line::default());
    ls.push(head("strata · background = machine region"));
    // Two rows because there are two strata: background belongs to machine
    // text, prose sits at the base, and the human's turn is fenced by the
    // rail's `❖` rather than a fill.
    let swatch = |text: &str, bg: Option<Color>| match bg {
        Some(bg) => line::wash(Line::from(Span::raw(text.to_string())), bg, None).spans,
        None => vec![note(text)],
    };
    ls.extend(line::legend_rows(vec![
        (
            "code",
            swatch("scripts and shell output — a recessed panel", Some(CODE_BG)),
        ),
        (
            "prose",
            swatch("model narration and replies — the base", None),
        ),
    ]));

    ls.push(Line::default());
    ls.push(head("bars · length and texture, beside a collapsed header"));
    ls.extend(line::legend_rows(vec![
        (
            "size",
            vec![line::size_bar(120), note("  log-scaled magnitude")],
        ),
        (
            "grain",
            vec![
                line::grain_run(9, 1),
                note("  diff density: ⣿ all adds → ⣀ all deletes"),
            ],
        ),
        (
            "sparkline",
            vec![
                Span::styled(
                    [None, Some(2), Some(40), Some(8), Some(300)]
                        .into_iter()
                        .map(line::spark_glyph)
                        .collect::<String>(),
                    Style::default().fg(SLATE),
                ),
                note("  one bar per call in a coalesced ral block"),
            ],
        ),
    ]));

    ls.push(Line::default());
    ls.push(head("status line · the two bars under the transcript"));
    ls.extend(line::legend_rows(vec![
        ("window", {
            let mut v = ctx_ramp(72);
            v.push(note("fills and brightens toward a full context window"));
            v
        }),
        ("elapsed", {
            let mut v = wait_bar(Duration::from_secs(18));
            v.push(note("grows with the time spent in the current state"));
            v
        }),
    ]));

    ls.push(Line::default());
    ls.push(head(
        "fidelity · a shaky answer renders drained, not authoritative",
    ));
    let prose = "An answer the model committed to the transcript.";
    let sample = |f: Fidelity| {
        md::render_md(prose, READ_W, 0, f)
            .into_iter()
            .next()
            .map(|line| line.spans)
            .unwrap_or_default()
    };
    ls.extend(line::legend_rows(vec![
        ("sound", sample(Fidelity::default())),
        (
            "drained",
            sample(Fidelity {
                context: 2,
                echo: 0,
            }),
        ),
        (
            "echoed",
            sample(Fidelity {
                context: 0,
                echo: 2,
            }),
        ),
    ]));
    ls.push(Line::from(note(
        "  context pressure drains the ink; echoing its own script washes the field behind it",
    )));

    ls.push(Line::default());
    ls.push(head("disclosure · dial detail on the rail (wheel / click)"));
    ls.push(Line::from(note(
        "  levels L1–L3; tool calls, diffs, and subagents floor at L1 — model prose always renders full",
    )));

    ls
}

#[cfg(test)]
mod tests {
    use super::{SessionInfo, legend_panel, session_card};
    use crate::bus::card::{FieldVal, Mark, Role};
    use crate::tui::{line, rail};
    use std::path::{Path, PathBuf};

    #[allow(clippy::disallowed_methods)] // a literal scratch path; no path semantics at stake
    fn sample(base: &'static str) -> SessionInfo<'static> {
        SessionInfo {
            system_size: 4096,
            system_files: &[],
            base,
            extend_base: None,
            restrict_files: &[],
            scratch: Path::new("/tmp/scratch"),
            cwd: "/Users/me/projects/ral",
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
            FieldVal::Measure(_) => None,
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
                "scratch",
            ]
        );
        let role = |label: &str| lead_role(&rs.iter().find(|(l, _)| l == label).unwrap().1);
        assert_eq!(
            role("version"),
            Some(Role::Strong),
            "version names the binary"
        );
        assert_eq!(role("cwd"), Some(Role::Path), "cwd is a path");
        assert_eq!(role("scratch"), Some(Role::Path), "scratch is a path");
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
        let text: String = legend_panel()
            .iter()
            .map(line::text)
            .collect::<Vec<_>>()
            .join("\n");
        for (_, name) in rail::RAIL_SHAPES {
            assert!(text.contains(name), "legend omits the {name:?} shape row");
        }
    }
}
