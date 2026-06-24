//! The `/model` tuning overlay — a floating, bezel-framed modal.
//!
//! The rest of the TUI is a flat stack of strips; this picker is the one
//! deliberate exception — a Norton-Commander-flavoured overlay that floats
//! above the (dimmed) session while open, drawn last over a [`Clear`]ed
//! centre of the frame. It is modal in *behaviour* (an early-return guard in
//! [`super::App::key`] and its own [`drive_picker`](super::drive_picker) loop)
//! and modal in *rendering*.
//!
//! It edits three things at once — the *selection* and *today's tuning*:
//!
//! * **Model** (nominal data) → **position**: a fuzzy-filtered vertical list
//!   of `provider / model` rows, ranked by `nucleo_matcher`, the selected row
//!   reversed.
//! * **Effort** (ordered data) → **value + size**: a rung ladder
//!   `auto · none · low · med · high · xhigh · max`, drawn as an ascending
//!   block ramp whose glyph height *and* brightness grow with the rung — the
//!   canonical Bertin encoding for ordered magnitude.
//! * **Temperature** (quantitative data) → **hue + size**: a track coloured by
//!   a cold-blue→warm-red gradient (literally apt) and filled to length, so
//!   both the position-of-hue and the fill encode the value.
//!
//! Which field has the keyboard is itself shown with **value**: the active
//! field brightens, the others dim. `Tab`/`BackTab` cycle the field; typing
//! always routes to the search box; `Enter` applies the whole selection
//! (model + tuning) from any field; `Esc` dismisses.
//!
//! The picker is a pure display+input component: it holds the query, the
//! per-provider model lists as they arrive, the selection, and the live
//! tuning. Fetching lives in the REPL (which owns the credential-backed
//! catalog and the network seam); the REPL feeds results in via
//! [`Picker::set_models`], so a provider's list shows "loading…" until its
//! background fetch lands, and seeds the tuning from the focused provider's
//! live values via [`Picker::new`].

use super::{BANNER_GOLD, CYAN, OVERLAY_BG, SLATE};
use crate::oauth::Subscription;
use crate::provider::{ProviderId, ReasoningEffort, Tuning};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use std::cmp::Reverse;
use std::collections::BTreeMap;

/// One provider's model-list fetch state.
pub enum ModelsState {
    /// The background fetch is in flight — the row reads "loading…".
    Loading,
    /// A usable list (from cache or a completed fetch).
    Loaded(Vec<String>),
    /// The fetch failed; the provider still accepts a manual model entry.
    Failed(String),
}

/// What a key press resolved to. The REPL acts on the non-`None` outcomes:
/// rebuilding the provider with the chosen model *and* tuning, persisting the
/// selection, and updating the status bar, or closing the picker.
pub enum PickAction {
    /// Keep the picker open; redraw.
    None,
    /// A listed `provider / model` row was chosen, with the live tuning.
    Selected(ProviderId, String, Tuning),
    /// Enter on the synthetic manual row: take the raw query as a model
    /// name and let the REPL resolve its provider (the listing-or-name
    /// fallback) — the escape hatch when a fetch failed or the wanted model
    /// is not listed. Carries the live tuning too.
    Manual(String, Tuning),
    /// Esc — dismiss without switching.
    Cancelled,
}

/// A row in the rendered list: either a listed model or the synthetic
/// manual-entry row.
enum Row {
    Model(ProviderId, String),
    Manual(String),
}

/// Which control holds the keyboard. The active field renders at full value;
/// the others dim. `Tab` cycles forward through this order, `BackTab` back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Search,
    Effort,
    Temperature,
}

/// One rung of the effort ladder: its short label, its ramp glyph (an
/// ascending block, so the ladder *grows* left-to-right), and the genai
/// [`ReasoningEffort`] it sends — `None` for "auto" (the option is not set and
/// the adapter's default stands). `auto` wears a dot rather than a block so it
/// reads as "no setting" rather than "the smallest setting".
struct Rung {
    label: &'static str,
    glyph: &'static str,
    effort: Option<ReasoningEffort>,
}

/// The effort ladder, ascending. `auto` first (the default, no option sent);
/// then genai's keyword rungs from `none` up to `max`. `XHigh`'s keyword is
/// `xhigh`; `Budget`/`Minimal` are intentionally omitted (the former carries a
/// raw token count with no place on an ordered ladder, the latter is a legacy
/// pre-gpt-5 alias for `low`).
const LADDER: &[Rung] = &[
    Rung { label: "auto", glyph: "·", effort: None },
    Rung { label: "none", glyph: "▁", effort: Some(ReasoningEffort::None) },
    Rung { label: "low", glyph: "▂", effort: Some(ReasoningEffort::Low) },
    Rung { label: "med", glyph: "▄", effort: Some(ReasoningEffort::Medium) },
    Rung { label: "high", glyph: "▆", effort: Some(ReasoningEffort::High) },
    Rung { label: "xhigh", glyph: "▇", effort: Some(ReasoningEffort::XHigh) },
    Rung { label: "max", glyph: "█", effort: Some(ReasoningEffort::Max) },
];

/// Temperature bounds and step. genai accepts `0.0..=2.0`; the overlay steps
/// by tenths and treats "below zero" as a return to auto (unset).
const TEMP_MAX: f64 = 2.0;
const TEMP_STEP: f64 = 0.1;
/// Cells in the temperature track — the hue gradient and the fill both span
/// this width.
const TRACK_W: usize = 24;

/// Rows of the model list visible at once.
const VISIBLE_ROWS: u16 = 8;
/// The overlay's fixed content width; clamped to the frame when narrower.
const OVERLAY_W: u16 = 64;

/// The temperature gradient endpoints (cold blue → warm red) and the effort
/// value-ramp endpoints (dim → bright cyan), as raw RGB for interpolation.
const COLD: (u8, u8, u8) = (96, 160, 235);
const WARM: (u8, u8, u8) = (228, 116, 96);
const EFFORT_DIM: (u8, u8, u8) = (74, 86, 110);
const EFFORT_BRIGHT: (u8, u8, u8) = (150, 205, 220);

/// Linear RGB interpolation between two colours at `t ∈ [0, 1]`.
fn lerp(a: (u8, u8, u8), b: (u8, u8, u8), t: f64) -> Color {
    let mix = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
    Color::Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// Snap a temperature to one decimal place — keeps repeated `±0.1` steps from
/// drifting into float noise like `0.30000000000000004`.
fn snap(t: f64) -> f64 {
    (t * 10.0).round() / 10.0
}

/// Centre a `w × h` rect within `area`, clamped to fit.
fn centered(w: u16, h: u16, area: Rect) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

pub struct Picker {
    query: String,
    /// Available providers in declaration order; their lists fill in as
    /// fetches land.
    providers: Vec<ProviderId>,
    /// Each subscription-backed provider's plan flavour; their rows render
    /// the subscription-decorated label. A provider absent from the map is
    /// metered and renders its bare name.
    subscription: BTreeMap<ProviderId, Subscription>,
    models: BTreeMap<ProviderId, ModelsState>,
    /// Index into the current filtered [`Self::rows`].
    selected: usize,
    /// Which control has the keyboard.
    focus: Focus,
    /// Index into [`LADDER`] — the chosen effort rung.
    effort_idx: usize,
    /// The chosen temperature, or `None` for auto (unset).
    temperature: Option<f64>,
}

impl Picker {
    /// Open over `providers`, all initially loading until the REPL feeds
    /// cached or fetched lists. `subscription` maps each plan-backed provider
    /// to its flavour, whose rows read as the subscription. `initial` seeds
    /// the effort/temperature controls from the focused provider's live
    /// tuning, so reopening shows the values currently in force.
    pub fn new(
        providers: Vec<ProviderId>,
        subscription: BTreeMap<ProviderId, Subscription>,
        initial: Tuning,
    ) -> Self {
        let models = providers
            .iter()
            .map(|id| (id.clone(), ModelsState::Loading))
            .collect();
        let effort_idx = LADDER
            .iter()
            .position(|r| match (&r.effort, &initial.effort) {
                (None, None) => true,
                (Some(a), Some(b)) => a.variant_name() == b.variant_name(),
                _ => false,
            })
            .unwrap_or(0);
        Self {
            query: String::new(),
            providers,
            subscription,
            models,
            selected: 0,
            focus: Focus::Search,
            effort_idx,
            temperature: initial.temperature,
        }
    }

    /// The display label for `id`'s rows: the plain provider name, or
    /// the subscription-decorated form when it is on a plan.
    fn label(&self, id: &ProviderId) -> String {
        let subscription = self
            .subscription
            .get(id)
            .copied()
            .unwrap_or(Subscription::Metered);
        crate::oauth::provider_label(subscription, id.label())
    }

    /// The providers whose lists are not yet known — the REPL spawns a
    /// background fetch for each on open.
    pub fn loading_providers(&self) -> Vec<ProviderId> {
        self.providers
            .iter()
            .filter(|id| matches!(self.models.get(id), Some(ModelsState::Loading)))
            .cloned()
            .collect()
    }

    /// Record a provider's resolved (or failed) list. Clamps the selection
    /// in case the visible list shrank.
    pub fn set_models(&mut self, id: &ProviderId, state: ModelsState) {
        self.models.insert(id.clone(), state);
        self.clamp_selection();
    }

    /// Whether any provider's fetch is still in flight.
    pub fn is_loading(&self) -> bool {
        self.providers
            .iter()
            .any(|id| matches!(self.models.get(id), Some(ModelsState::Loading)))
    }

    /// The live tuning the controls currently express.
    fn tuning(&self) -> Tuning {
        Tuning {
            effort: LADDER[self.effort_idx].effort.clone(),
            temperature: self.temperature,
        }
    }

    /// Handle one key press. Typing filters (and focuses the search box);
    /// `Tab`/`BackTab` cycle the field; the arrows move the focused control;
    /// `Enter` applies the model + tuning; `Esc` cancels.
    pub fn key(&mut self, code: ratatui::crossterm::event::KeyCode) -> PickAction {
        use ratatui::crossterm::event::KeyCode;
        match code {
            KeyCode::Esc => return PickAction::Cancelled,
            KeyCode::Enter => return self.apply(),
            KeyCode::Tab => self.focus = self.cycle(true),
            KeyCode::BackTab => self.focus = self.cycle(false),
            // Typing always means "filter models", whatever field had focus.
            KeyCode::Char(c) => {
                self.focus = Focus::Search;
                self.query.push(c);
                self.selected = 0;
            }
            KeyCode::Backspace => {
                self.focus = Focus::Search;
                self.query.pop();
                self.selected = 0;
            }
            KeyCode::Up => self.move_in_focus(false),
            KeyCode::Down => self.move_in_focus(true),
            KeyCode::Left => self.move_in_focus(false),
            KeyCode::Right => self.move_in_focus(true),
            _ => {}
        }
        PickAction::None
    }

    /// Resolve the highlighted row into a selection carrying the live tuning.
    fn apply(&self) -> PickAction {
        match self.rows().into_iter().nth(self.selected) {
            Some(Row::Model(id, model)) => PickAction::Selected(id, model, self.tuning()),
            Some(Row::Manual(query)) => PickAction::Manual(query, self.tuning()),
            None => PickAction::None,
        }
    }

    /// The next focus in cycle order (`Search → Effort → Temperature → …`).
    fn cycle(&self, forward: bool) -> Focus {
        match (self.focus, forward) {
            (Focus::Search, true) => Focus::Effort,
            (Focus::Effort, true) => Focus::Temperature,
            (Focus::Temperature, true) => Focus::Search,
            (Focus::Search, false) => Focus::Temperature,
            (Focus::Effort, false) => Focus::Search,
            (Focus::Temperature, false) => Focus::Effort,
        }
    }

    /// Move the focused control: in Search the model selection, in Effort the
    /// rung, in Temperature the value. `up` is the increasing direction (down
    /// the list, up the ladder, warmer).
    fn move_in_focus(&mut self, up: bool) {
        match self.focus {
            Focus::Search => {
                if up {
                    let n = self.rows().len();
                    if n > 0 {
                        self.selected = (self.selected + 1).min(n - 1);
                    }
                } else {
                    self.selected = self.selected.saturating_sub(1);
                }
            }
            Focus::Effort => {
                self.effort_idx = if up {
                    (self.effort_idx + 1).min(LADDER.len() - 1)
                } else {
                    self.effort_idx.saturating_sub(1)
                };
            }
            Focus::Temperature => {
                self.temperature = if up {
                    Some(snap(self.temperature.map_or(0.0, |t| (t + TEMP_STEP).min(TEMP_MAX))))
                } else {
                    // Stepping below zero returns to auto (unset).
                    match self.temperature {
                        None => None,
                        Some(t) if t - TEMP_STEP < -f64::EPSILON => None,
                        Some(t) => Some(snap((t - TEMP_STEP).max(0.0))),
                    }
                };
            }
        }
    }

    /// The filtered rows: every loaded model whose `provider / model` label
    /// fuzzy-matches the query (ranked by `nucleo_matcher` score, ties keeping
    /// listed order), plus a synthetic manual-entry row when the query is
    /// non-empty (so a model that's not listed, or a provider whose fetch
    /// failed, is still reachable). An empty query shows every model unfiltered.
    fn rows(&self) -> Vec<Row> {
        let q = self.query.trim();

        // (provider, model) pairs paired positionally with their haystacks.
        let mut candidates: Vec<(ProviderId, String)> = Vec::new();
        let mut haystacks: Vec<String> = Vec::new();
        for id in &self.providers {
            if let Some(ModelsState::Loaded(models)) = self.models.get(id) {
                let label = self.label(id);
                for model in models {
                    haystacks.push(format!("{label} / {model}"));
                    candidates.push((id.clone(), model.clone()));
                }
            }
        }

        // Empty query: show every loaded model, in listed order.
        if q.is_empty() {
            return candidates
                .into_iter()
                .map(|(id, model)| Row::Model(id, model))
                .collect();
        }

        // Fuzzy-match each haystack, keeping its index so the row survives
        // even when two providers list the same model name.
        let pattern = Pattern::parse(q, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        // A fresh per-call matcher is cheap here (one keystroke produces one
        // `rows()` call over a small list); we cannot borrow a stored one
        // mutably through `&self`.
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut scored: Vec<(usize, u32)> = haystacks
            .iter()
            .enumerate()
            .filter_map(|(i, hay)| {
                pattern
                    .score(Utf32Str::new(hay, &mut buf), &mut matcher)
                    .map(|score| (i, score))
            })
            .collect();
        // Descending score; stable sort keeps listed order on ties.
        scored.sort_by_key(|(_, score)| Reverse(*score));

        let mut rows: Vec<Row> = scored
            .into_iter()
            .map(|(i, _)| Row::Model(candidates[i].0.clone(), candidates[i].1.clone()))
            .collect();
        rows.push(Row::Manual(q.to_string()));
        rows
    }

    fn clamp_selection(&mut self) {
        let n = self.rows().len();
        self.selected = if n == 0 { 0 } else { self.selected.min(n - 1) };
    }

    /// Providers whose fetch failed, with their reasons — surfaced as dim
    /// notes so the absent models are explained and the manual-entry fallback
    /// is obvious.
    fn failures(&self) -> Vec<(&ProviderId, &str)> {
        self.providers
            .iter()
            .filter_map(|id| match self.models.get(id) {
                Some(ModelsState::Failed(reason)) => Some((id, reason.as_str())),
                _ => None,
            })
            .collect()
    }

    // --- rendering -----------------------------------------------------------

    /// The overlay's outer size: the fixed width (clamped to the frame) and a
    /// height that fits the search line, status line, bordered model list, the
    /// two tuning rows, one note per failed provider, and the bezel.
    fn desired_size(&self, frame: Rect) -> (u16, u16) {
        let failed = self.failures().len() as u16;
        // bezel(2) + search(1) + status(1) + list(VISIBLE+border) + effort(1)
        //          + temp(1) + failed notes
        let h = 2 + 1 + 1 + (VISIBLE_ROWS + 2) + 1 + 1 + failed;
        (OVERLAY_W.min(frame.width), h.min(frame.height.max(3)))
    }

    /// Draw the floating overlay over the centre of `frame`: a double-line
    /// bezel (the "above the session" affordance) holding the search box, the
    /// fuzzy model list, the effort ramp, the temperature track, and the
    /// function-key footer on the bottom border.
    pub fn render(&self, f: &mut Frame, frame: Rect) {
        let (w, h) = self.desired_size(frame);
        let area = centered(w, h, frame);
        let plane = Style::default().bg(OVERLAY_BG);

        // Blank the cells beneath, then paint the bezel.
        f.render_widget(Clear, area);
        let bezel = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .border_style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD))
            .style(plane)
            .title(
                Line::from(Span::styled(
                    " MODEL · EFFORT · TEMP ",
                    plane.fg(BANNER_GOLD).add_modifier(Modifier::BOLD),
                ))
                .centered(),
            )
            .title_bottom(
                Line::from(Span::styled(
                    " ⇥ field · ↑↓ pick · ←→ adjust · ⏎ apply · esc cancel ",
                    plane.fg(SLATE).add_modifier(Modifier::DIM),
                ))
                .centered(),
            )
            .padding(Padding::horizontal(1));
        let inner = bezel.inner(area);
        f.render_widget(bezel, area);

        let chunks = Layout::vertical([
            Constraint::Length(1),              // search
            Constraint::Length(1),              // status
            Constraint::Length(VISIBLE_ROWS + 2), // bordered model list
            Constraint::Length(1),              // effort
            Constraint::Length(1),              // temperature
            Constraint::Min(0),                 // failed-provider notes
        ])
        .split(inner);

        f.render_widget(Paragraph::new(self.search_line()).style(plane), chunks[0]);
        f.render_widget(Paragraph::new(self.status_line()).style(plane), chunks[1]);
        self.render_list(f, chunks[2], plane);
        f.render_widget(Paragraph::new(self.effort_line()).style(plane), chunks[3]);
        f.render_widget(Paragraph::new(self.temp_line()).style(plane), chunks[4]);
        let notes = self.failed_lines();
        if !notes.is_empty() {
            f.render_widget(Paragraph::new(notes).style(plane), chunks[5]);
        }
    }

    /// A field label, bright when focused and dim otherwise — focus rendered
    /// as value, so the eye finds the live control by lightness.
    fn field_label(&self, text: &str, field: Focus) -> Span<'static> {
        let style = if self.focus == field {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(SLATE).add_modifier(Modifier::DIM)
        };
        Span::styled(format!("{text:<7}"), style)
    }

    fn search_line(&self) -> Line<'static> {
        let focused = self.focus == Focus::Search;
        let query_style = if focused {
            Style::default().fg(CYAN)
        } else {
            Style::default().fg(SLATE)
        };
        let caret = if focused { "▏" } else { " " };
        Line::from(vec![
            self.field_label("search", Focus::Search),
            Span::styled(self.query.clone(), query_style),
            Span::styled(caret, Style::default().fg(CYAN)),
        ])
    }

    fn status_line(&self) -> Line<'static> {
        let n = self.rows().len();
        let text = if self.is_loading() {
            "loading…".to_string()
        } else {
            format!("{n} match{}", if n == 1 { "" } else { "es" })
        };
        Line::from(Span::styled(
            format!("{:<7}{text}", ""),
            Style::default().fg(SLATE).add_modifier(Modifier::DIM),
        ))
    }

    /// The model list inside its own rounded panel (a panel-within-the-bezel,
    /// Norton-Commander style). The selected row is reversed; the panel border
    /// brightens when the search field has focus.
    fn render_list(&self, f: &mut Frame, area: Rect, plane: Style) {
        let focused = self.focus == Focus::Search;
        let border = if focused {
            Style::default().fg(CYAN)
        } else {
            Style::default().fg(SLATE).add_modifier(Modifier::DIM)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border)
            .style(plane);
        let list_area = block.inner(area);
        f.render_widget(block, area);

        let rows = self.rows();
        let window = list_area.height as usize;
        // Scroll so the selected row stays visible.
        let start = self.selected.saturating_sub(window.saturating_sub(1));
        let lines: Vec<Line<'static>> = rows
            .iter()
            .enumerate()
            .skip(start)
            .take(window)
            .map(|(i, row)| {
                let text = match row {
                    Row::Model(id, m) => format!("{} / {m}", self.label(id)),
                    Row::Manual(q) => format!("use “{q}” as a manual model"),
                };
                let mut style = match row {
                    Row::Manual(_) => Style::default().fg(SLATE).add_modifier(Modifier::ITALIC),
                    Row::Model(..) => Style::default().fg(CYAN),
                };
                if i == self.selected {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                Line::from(Span::styled(text, style))
            })
            .collect();
        f.render_widget(Paragraph::new(lines).style(plane), list_area);
    }

    /// The effort ladder: an ascending block ramp, each rung brightening with
    /// its ordinal (value) as the glyph grows (size); the chosen rung is
    /// reversed and its label printed.
    fn effort_line(&self) -> Line<'static> {
        let focused = self.focus == Focus::Effort;
        let last = LADDER.len().saturating_sub(1).max(1) as f64;
        let mut spans = vec![self.field_label("effort", Focus::Effort)];
        for (i, rung) in LADDER.iter().enumerate() {
            let value = lerp(EFFORT_DIM, EFFORT_BRIGHT, i as f64 / last);
            let mut style = Style::default().fg(value);
            if !focused {
                style = style.add_modifier(Modifier::DIM);
            }
            if i == self.effort_idx {
                style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
            }
            spans.push(Span::styled(rung.glyph, style));
        }
        let chosen = LADDER[self.effort_idx].label;
        let label_style = if focused {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(SLATE)
        };
        spans.push(Span::styled(format!("  {chosen}"), label_style));
        Line::from(spans)
    }

    /// The temperature track: `TRACK_W` cells coloured along a cold→warm
    /// gradient, filled to the value's fraction of [`TEMP_MAX`] (size), unset
    /// cells dimmed. Auto draws the whole track faint.
    fn temp_line(&self) -> Line<'static> {
        let focused = self.focus == Focus::Temperature;
        let mut spans = vec![self.field_label("temp", Focus::Temperature)];

        let filled = self
            .temperature
            .map_or(0, |t| ((t / TEMP_MAX) * TRACK_W as f64).round() as usize);
        let last = (TRACK_W - 1).max(1) as f64;
        for i in 0..TRACK_W {
            let hue = lerp(COLD, WARM, i as f64 / last);
            let on = self.temperature.is_some() && i < filled;
            let glyph = if on { "█" } else { "░" };
            let mut style = Style::default().fg(hue);
            if !on || !focused {
                style = style.add_modifier(Modifier::DIM);
            }
            spans.push(Span::styled(glyph, style));
        }

        let readout = match self.temperature {
            Some(t) => format!("  {t:.1}"),
            None => "  auto".to_string(),
        };
        let readout_style = if focused {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(SLATE)
        };
        spans.push(Span::styled(readout, readout_style));
        Line::from(spans)
    }

    fn failed_lines(&self) -> Vec<Line<'static>> {
        self.failures()
            .into_iter()
            .map(|(id, reason)| {
                Line::from(Span::styled(
                    format!(
                        "{} — fetch failed: {reason} (type a model to enter manually)",
                        id.label()
                    ),
                    Style::default()
                        .fg(SLATE)
                        .add_modifier(Modifier::DIM | Modifier::ITALIC),
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderKind;
    use ratatui::crossterm::event::KeyCode;

    /// A famous provider's id — the common case in these tests.
    fn fam(kind: ProviderKind) -> ProviderId {
        ProviderId::Famous(kind)
    }

    /// A custom provider's id with `label`.
    fn custom(label: &str) -> ProviderId {
        ProviderId::Custom(std::sync::Arc::new(crate::provider::CustomProvider {
            label: label.into(),
            key_env: format!("{}_KEY", label.to_uppercase()),
            endpoint: format!("https://{label}.example/v1/"),
            adapter: genai::adapter::AdapterKind::OpenAI,
        }))
    }

    fn loaded_picker() -> Picker {
        let mut p = Picker::new(
            vec![fam(ProviderKind::Anthropic), fam(ProviderKind::Deepseek)],
            BTreeMap::new(),
            Tuning::default(),
        );
        p.set_models(
            &fam(ProviderKind::Anthropic),
            ModelsState::Loaded(vec!["claude-opus-4".into(), "claude-haiku-4".into()]),
        );
        p.set_models(
            &fam(ProviderKind::Deepseek),
            ModelsState::Loaded(vec!["deepseek-chat".into()]),
        );
        p
    }

    /// With no query every loaded model across providers is shown.
    #[test]
    fn empty_query_shows_all_loaded_models() {
        let p = loaded_picker();
        let labels: Vec<String> = p
            .rows()
            .into_iter()
            .filter_map(|r| match r {
                Row::Model(id, m) => Some(format!("{} / {m}", id.label())),
                Row::Manual(_) => None,
            })
            .collect();
        assert_eq!(
            labels,
            vec![
                "anthropic / claude-opus-4",
                "anthropic / claude-haiku-4",
                "deepseek / deepseek-chat",
            ]
        );
    }

    /// A subscription-backed provider's rows render the decorated label
    /// (`openai (ChatGPT subscription) / model`) while still matching a
    /// plain `openai` search, so the picker reads as the subscription and
    /// the search haystack keeps the bare provider name.
    #[test]
    fn subscription_provider_rows_carry_decorated_label() {
        let mut p = Picker::new(
            vec![fam(ProviderKind::Openai)],
            BTreeMap::from([(fam(ProviderKind::Openai), Subscription::ChatGpt)]),
            Tuning::default(),
        );
        p.set_models(
            &fam(ProviderKind::Openai),
            ModelsState::Loaded(vec!["gpt-5.5".into()]),
        );
        let labels: Vec<String> = p
            .rows()
            .into_iter()
            .filter_map(|r| match r {
                Row::Model(id, m) => Some(format!("{} / {m}", p.label(&id))),
                Row::Manual(_) => None,
            })
            .collect();
        assert_eq!(labels, vec!["openai (ChatGPT subscription) / gpt-5.5"]);
        // The bare provider name still matches search.
        for c in "openai".chars() {
            p.key(KeyCode::Char(c));
        }
        let model_rows: Vec<_> = p
            .rows()
            .into_iter()
            .filter(|r| matches!(r, Row::Model(..)))
            .collect();
        assert_eq!(model_rows.len(), 1);
    }

    /// A flat-rate provider (opencode Go) renders the generic
    /// `(subscription)` suffix — distinct from the ChatGPT plan's decoration
    /// — so the picker reads its plan correctly without claiming it is a
    /// ChatGPT login.
    #[test]
    fn flat_rate_provider_rows_carry_generic_subscription_label() {
        let mut p = Picker::new(
            vec![fam(ProviderKind::OpencodeGo)],
            BTreeMap::from([(fam(ProviderKind::OpencodeGo), Subscription::FlatRate)]),
            Tuning::default(),
        );
        p.set_models(
            &fam(ProviderKind::OpencodeGo),
            ModelsState::Loaded(vec!["glm-5.2".into()]),
        );
        let labels: Vec<String> = p
            .rows()
            .into_iter()
            .filter_map(|r| match r {
                Row::Model(id, m) => Some(format!("{} / {m}", p.label(&id))),
                Row::Manual(_) => None,
            })
            .collect();
        assert_eq!(labels, vec!["opencode-go (subscription) / glm-5.2"]);
    }

    /// Typing filters by substring over the `provider / model` label, and a
    /// manual-entry row is appended once the query is non-empty.
    #[test]
    fn query_filters_substring_and_appends_manual_row() {
        let mut p = loaded_picker();
        for c in "haiku".chars() {
            p.key(KeyCode::Char(c));
        }
        let rows = p.rows();
        // One model match (haiku) plus the synthetic manual row.
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0], Row::Model(_, m) if m == "claude-haiku-4"));
        assert!(matches!(&rows[1], Row::Manual(q) if q == "haiku"));
    }

    /// A provider filter narrows to that provider's models.
    #[test]
    fn provider_substring_narrows() {
        let mut p = loaded_picker();
        for c in "deepseek".chars() {
            p.key(KeyCode::Char(c));
        }
        let model_rows: Vec<_> = p
            .rows()
            .into_iter()
            .filter(|r| matches!(r, Row::Model(..)))
            .collect();
        assert_eq!(model_rows.len(), 1);
        assert!(matches!(
            &model_rows[0],
            Row::Model(id, _) if id.famous() == Some(ProviderKind::Deepseek)
        ));
    }

    /// Enter on a listed row yields `Selected(provider, model, tuning)`.
    #[test]
    fn enter_selects_highlighted_model() {
        let mut p = loaded_picker();
        // Move to the second row (anthropic / claude-haiku-4).
        p.key(KeyCode::Down);
        match p.key(KeyCode::Enter) {
            PickAction::Selected(id, m, _) if id.famous() == Some(ProviderKind::Anthropic) => {
                assert_eq!(m, "claude-haiku-4")
            }
            _ => panic!("expected Selected(anthropic, claude-haiku-4)"),
        }
    }

    /// A custom provider's models list and select through the picker exactly
    /// like a famous one: its declared label decorates the row and Enter
    /// yields the custom `ProviderId`.
    #[test]
    fn custom_provider_lists_and_selects() {
        let llama = custom("local-llama");
        let mut p = Picker::new(vec![llama.clone()], BTreeMap::new(), Tuning::default());
        p.set_models(&llama, ModelsState::Loaded(vec!["llama-3".into()]));
        let rows = p.rows();
        assert!(matches!(&rows[0], Row::Model(id, m) if id == &llama && m == "llama-3"));
        match p.key(KeyCode::Enter) {
            PickAction::Selected(id, m, _) => {
                assert_eq!(id, llama);
                assert_eq!(m, "llama-3");
            }
            _ => panic!("expected Selected(local-llama, llama-3)"),
        }
    }

    /// Enter on the manual row (a query matching nothing) yields the raw
    /// query for the REPL to resolve.
    #[test]
    fn enter_on_manual_row_yields_query() {
        let mut p = loaded_picker();
        for c in "claude-future-99".chars() {
            p.key(KeyCode::Char(c));
        }
        // Only the manual row matches; it is selected at index 0.
        match p.key(KeyCode::Enter) {
            PickAction::Manual(q, _) => assert_eq!(q, "claude-future-99"),
            _ => panic!("expected Manual(claude-future-99)"),
        }
    }

    /// Esc cancels.
    #[test]
    fn esc_cancels() {
        let mut p = loaded_picker();
        assert!(matches!(p.key(KeyCode::Esc), PickAction::Cancelled));
    }

    /// `Tab` cycles the focus Search → Effort → Temperature → Search, and the
    /// arrows then drive the focused control: in Effort they climb the ladder,
    /// in Temperature they warm the value.
    #[test]
    fn tab_cycles_focus_and_arrows_drive_the_focused_control() {
        let mut p = loaded_picker();
        assert_eq!(p.focus, Focus::Search);
        p.key(KeyCode::Tab);
        assert_eq!(p.focus, Focus::Effort);
        // Up the ladder twice: auto → none → low.
        p.key(KeyCode::Right);
        p.key(KeyCode::Right);
        assert_eq!(LADDER[p.effort_idx].label, "low");

        p.key(KeyCode::Tab);
        assert_eq!(p.focus, Focus::Temperature);
        // From auto, one step right reaches 0.0, another 0.1.
        p.key(KeyCode::Right);
        p.key(KeyCode::Right);
        assert_eq!(p.temperature, Some(0.1));

        p.key(KeyCode::Tab);
        assert_eq!(p.focus, Focus::Search);
    }

    /// Typing routes to the search box even when a tuning field had focus, so
    /// the model filter is always reachable.
    #[test]
    fn typing_refocuses_search() {
        let mut p = loaded_picker();
        p.key(KeyCode::Tab); // Effort
        p.key(KeyCode::Char('o'));
        assert_eq!(p.focus, Focus::Search);
        assert_eq!(p.query, "o");
    }

    /// Temperature steps in tenths, clamps at the maximum, and stepping below
    /// zero returns to auto (unset).
    #[test]
    fn temperature_steps_clamps_and_floors_to_auto() {
        let mut p = loaded_picker();
        p.key(KeyCode::Tab); // Effort
        p.key(KeyCode::Tab); // Temperature
        assert_eq!(p.temperature, None);
        // Three steps up: auto → 0.0 → 0.1 → 0.2.
        p.key(KeyCode::Right);
        p.key(KeyCode::Right);
        p.key(KeyCode::Right);
        assert_eq!(p.temperature, Some(0.2));
        // Down past zero returns to auto.
        p.key(KeyCode::Left);
        p.key(KeyCode::Left);
        assert_eq!(p.temperature, Some(0.0));
        p.key(KeyCode::Left);
        assert_eq!(p.temperature, None);
    }

    /// The chosen effort + temperature ride along with the selection.
    #[test]
    fn selection_carries_the_live_tuning() {
        let mut p = loaded_picker();
        p.key(KeyCode::Tab); // Effort
        p.key(KeyCode::Right); // auto → none
        p.key(KeyCode::Right); // none → low
        p.key(KeyCode::Right); // low → med
        p.key(KeyCode::Right); // med → high
        p.key(KeyCode::Tab); // Temperature
        p.key(KeyCode::Right); // auto → 0.0
        p.key(KeyCode::Right); // 0.0 → 0.1
        match p.key(KeyCode::Enter) {
            PickAction::Selected(_, _, tuning) => {
                assert_eq!(
                    tuning.effort.as_ref().map(|e| e.variant_name()),
                    Some("high")
                );
                assert_eq!(tuning.temperature, Some(0.1));
            }
            _ => panic!("expected Selected with tuning"),
        }
    }

    /// The controls open seeded from the focused provider's live tuning.
    #[test]
    fn opens_seeded_from_initial_tuning() {
        let p = Picker::new(
            vec![fam(ProviderKind::Anthropic)],
            BTreeMap::new(),
            Tuning {
                effort: Some(ReasoningEffort::Medium),
                temperature: Some(0.5),
            },
        );
        assert_eq!(LADDER[p.effort_idx].label, "med");
        assert_eq!(p.temperature, Some(0.5));
    }
}
