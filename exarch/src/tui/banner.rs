//! Session metadata and the visual-vocabulary legend.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::card::{Card, Field, FieldVal, Mark, Role, Span as CardSpan};
use crate::provider::{self, Provider};

use super::block::AgentSlot;
use super::fidelity::Fidelity;
use super::line;
use super::palette::{AGENT_HUES, CODE_BG, READ_W, SLATE};
use super::md;
use super::rail::{self, RailKind};
use super::status::{ctx_ramp, wait_bar};

pub(super) const ART: &str = include_str!("../../data/banner.txt");
pub(super) const EAGLE: &str = include_str!("../../data/eagle.txt");

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

/// A rail-less Plain like the splash above it.  Hue is spent only where it
/// names something: a path carries the Path identity, a `dangerous` base
/// alarms; names and quantities stay plain ink.
pub(super) fn session_card(s: &SessionInfo<'_>, p: &Provider) -> Card {
    let caps = crate::pricing::caps_or_default(p.model());
    let mut rows: Vec<Field> = vec![
        meta_field("cwd", vec![meta_span(Role::Path, s.cwd)]),
    ];

    // Neither the provider nor the model is named here: the live status bar
    // carries both (and updates them on a `/model` switch), so the one-shot
    // banner card need not — and a model-less launch has nothing to print.
    if let Some(ctx) = caps.context_window {
        rows.push(meta_field(
            "context",
            vec![meta_plain(provider::humanize_tokens(ctx))],
        ));
    }

    let base_role = if s.base == "dangerous" {
        Role::Bad
    } else {
        Role::Strong
    };
    rows.push(meta_field("base", vec![meta_span(base_role, s.base)]));

    rows.push(meta_field(
        "extend-base",
        match s.extend_base {
            Some(p) => vec![meta_span(Role::Path, p.display().to_string())],
            None => vec![meta_span(Role::Muted, "none")],
        },
    ));

    rows.push(meta_field(
        "restrict",
        if s.restrict_files.is_empty() {
            vec![meta_span(Role::Muted, "none")]
        } else {
            vec![meta_span(Role::Path, join_paths(s.restrict_files))]
        },
    ));

    #[allow(
        clippy::cast_precision_loss,
        reason = "byte count of system prompt; display only"
    )]
    let system_size = s.system_size as f64;
    let sz = format!("{:.1} kB", system_size / 1024.0);
    let mut sys_val = vec![meta_plain(sz), meta_span(Role::Muted, " · ")];
    if s.system_files.is_empty() {
        sys_val.push(meta_span(Role::Muted, "default"));
    } else {
        sys_val.push(meta_span(Role::Path, join_paths(s.system_files)));
    }
    rows.push(meta_field("system prompt", sys_val));

    rows.push(meta_field(
        "scratch",
        vec![meta_span(Role::Path, s.scratch.display().to_string())],
    ));

    Card(vec![Mark::Fields { rows }])
}

/// [`Role`] (the renderer binds it to a hue), never a colour.
pub(super) fn meta_span(role: Role, text: impl Into<String>) -> CardSpan {
    CardSpan {
        role: Some(role),
        text: text.into(),
    }
}

/// A roleless value span — a quantity readout the matrix renders as plain
/// ink, carrying no nominal identity.
pub(super) fn meta_plain(text: impl Into<String>) -> CardSpan {
    CardSpan {
        role: None,
        text: text.into(),
    }
}

/// One `(label, value)` row of the startup metadata matrix.
pub(super) fn meta_field(label: &str, value: Vec<CardSpan>) -> Field {
    Field {
        label: label.to_string(),
        value: FieldVal::Inline(value),
    }
}

/// A comma-joined display of `paths` for a single matrix value cell.
pub(super) fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `/legend` panel: the transcript's visual vocabulary exhibited as a
/// graphic — each rail shape, agent hue, value step, stratum, bar, and
/// fidelity grade shown as the literal styled output it wears in the flow,
/// under plain slate-bold heads.  The samples derive from [`rail::RAIL_SHAPES`],
/// [`AGENT_HUES`], and the bar / grain / spark / fidelity builders, so a
/// palette or shape change updates the legend with no edit here.
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

    // ── rail: one cell, three variables ───────────────────────────────
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
    // One shape, the same hue, stepped up the value ramp by feeding one
    // magnitude per `rail::value_step` bucket (0..=3) — so the row *is* the
    // ramp the rail lightens by, not a restatement of it.
    ls.extend(line::legend_rows(vec![(
        "small → large",
        [Some(4), Some(20), Some(80), Some(200)]
            .into_iter()
            .map(|mag| rail::span(RailKind::Patch, AgentSlot(0), mag))
            .collect(),
    )]));

    // ── strata: who is speaking, read off the background ───────────────
    ls.push(Line::default());
    ls.push(head("strata · background = machine region"));
    // Each swatch is the literal `line::wash` output, so the legend wears the
    // exact tones the transcript paints. Background is reserved for one thing —
    // machine text, a recessed panel; prose sits at the base, and your prompt
    // is fenced by a rule (the rail's `❖`), not a fill.
    let swatch = |text: &str, bg: Option<Color>| match bg {
        Some(bg) => line::wash(Line::from(Span::raw(text.to_string())), bg, None).spans,
        None => vec![note(text)],
    };
    ls.extend(line::legend_rows(vec![
        (
            "code",
            swatch(
                "scripts and shell output — a recessed panel",
                Some(CODE_BG),
            ),
        ),
        (
            "prose",
            swatch("model narration and replies — the base", None),
        ),
    ]));

    // ── the ordered bars ───────────────────────────────────────────────
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

    // ── the status line's two bottom bars ──────────────────────────────
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
            v.push(note("grows with the current phase's wall-time"));
            v
        }),
    ]));

    // ── coherent degradation: how much to trust a passage ──────────────
    ls.push(Line::default());
    ls.push(head(
        "fidelity · a shaky answer renders drained, not authoritative",
    ));
    // Real prose through the real `render_md`, so the drain and wash are
    // exactly what a degraded block wears — never a re-derived colour.
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

    // ── disclosure: detail is something you dial ───────────────────────
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
    use crate::card::{FieldVal, Mark, Role};
    use crate::provider::scripted::Script;
    use crate::provider::{Provider, ProviderKind};
    use crate::tui::{line, rail};
    use std::path::{Path, PathBuf};

    /// A representative session: a fetched-catalog model (distinct slug,
    /// known context window), default system prompt, no extend/restrict.
    #[allow(clippy::disallowed_methods)] // test scaffolding: a fixed literal scratch path, no path semantics to get wrong
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

    fn sample_provider() -> Provider {
        Provider::scripted("claude-opus-4-8", ProviderKind::Anthropic, Script::new())
    }

    fn rows(s: &SessionInfo<'_>, p: &Provider) -> Vec<(String, FieldVal)> {
        let card = session_card(s, p);
        match card.marks() {
            [Mark::Fields { rows }] => rows
                .iter()
                .map(|f| (f.label.clone(), f.value.clone()))
                .collect(),
            other => panic!("session card must be one fields mark, got {other:?}"),
        }
    }

    /// The nominal role of a row's leading value span — `None` for a plain
    /// (roleless) quantity readout or a measure.
    fn lead_role(v: &FieldVal) -> Option<Role> {
        match v {
            FieldVal::Inline(spans) => spans.first().and_then(|sp| sp.role),
            FieldVal::Measure(_) => None,
        }
    }

    /// The matrix orders location → identity → capacity → security → prompt,
    /// roles paths as Path, and leaves quantities as plain ink (no hue).
    #[test]
    fn session_card_orders_and_roles_fields() {
        let rs = rows(&sample("read-only"), &sample_provider());
        let labels: Vec<&str> = rs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            [
                "cwd",
                "base",
                "extend-base",
                "restrict",
                "system prompt",
                "scratch",
            ]
        );
        let role = |label: &str| lead_role(&rs.iter().find(|(l, _)| l == label).unwrap().1);
        assert_eq!(role("cwd"), Some(Role::Path), "cwd is a path");
        assert_eq!(role("scratch"), Some(Role::Path), "scratch is a path");
    }

    /// Hue is spent on `base` only when it alarms: `dangerous` → Bad (red),
    /// every safe base → Strong (plain bold).
    #[test]
    fn dangerous_base_is_the_one_field_that_earns_a_hue() {
        let base_role = |b: &'static str| {
            let rs = rows(&sample(b), &sample_provider());
            lead_role(&rs.iter().find(|(l, _)| l == "base").unwrap().1)
        };
        assert_eq!(base_role("dangerous"), Some(Role::Bad));
        assert_eq!(base_role("read-only"), Some(Role::Strong));
        assert_eq!(base_role("confined"), Some(Role::Strong));
    }

    /// Present extend-base / restrict paths carry the Path identity; absent
    /// ones read as a muted "none" rather than borrowing a hue.
    #[test]
    fn security_paths_are_roled_present_and_muted_when_absent() {
        let rs = rows(&sample("read-only"), &sample_provider());
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
        let rs = rows(&s, &sample_provider());
        assert_eq!(
            lead_role(&rs.iter().find(|(l, _)| l == "extend-base").unwrap().1),
            Some(Role::Path)
        );
        assert_eq!(
            lead_role(&rs.iter().find(|(l, _)| l == "restrict").unwrap().1),
            Some(Role::Path)
        );
    }

    /// The Bertin claim: rendered, every value lands in one shared column —
    /// each field line opens with a label cell of identical width.
    #[test]
    fn rendered_matrix_aligns_every_value_in_one_column() {
        let card = session_card(&sample("dangerous"), &sample_provider());
        let lines = line::render_card(&card, 3);
        let label_w = rows(&sample("dangerous"), &sample_provider())
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap()
            + 2;
        for l in &lines {
            // Dump so the column is eyeballable under `--nocapture`.
            eprintln!(
                "[{:>2}] {}",
                l.spans.first().map_or(0, |s| s.content.chars().count()),
                line::plain(l)
            );
        }
        for l in &lines {
            let Some(first) = l.spans.first() else {
                continue;
            };
            assert_eq!(
                first.content.chars().count(),
                label_w,
                "every field line opens with a label cell of width {label_w}"
            );
        }
    }

    /// The legend enumerates the rail's *own* shape vocabulary: every
    /// `RAIL_SHAPES` entry's name appears as a row label, so a new shape
    /// cannot land on the rail without showing up in the legend.
    #[test]
    fn legend_names_every_rail_shape() {
        let text: String = legend_panel()
            .iter()
            .map(line::plain)
            .collect::<Vec<_>>()
            .join("\n");
        for (_, name) in rail::RAIL_SHAPES {
            assert!(text.contains(name), "legend omits the {name:?} shape row");
        }
    }

    /// The legend is ambient, rail-less chrome: no row borrows a marginal
    /// rail glyph as its leading span.  The shape samples *contain* the
    /// glyphs, but always in a value cell behind a label — never as the
    /// row-leading rail the copy contract ([`line::plain`]) would strip.
    #[test]
    fn legend_wears_no_marginal_rail() {
        for l in legend_panel() {
            assert_eq!(
                line::rail_skip(&l),
                0,
                "a legend row leads with a rail glyph: {:?}",
                line::plain(&l)
            );
        }
    }
}
