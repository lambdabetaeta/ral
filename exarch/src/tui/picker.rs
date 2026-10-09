//! The `/model` overlay: a fuzzy-filtered `model · provider` list over every
//! account, with the effort, temperature and top-p to send. A launch with no
//! model to restore opens it alone, before its session.
//!
//! An `OpenRouter` model adds a provider row that pins `provider.order`; it is
//! a routing choice, not a filter, so it never moves the highlight. A tuning
//! row the highlighted model's catalog entry positively lacks is grayed and
//! masked out of the [`Tuning`], but kept for the next model that admits it.
//!
//! Display and input only: `model_picker.rs` fetches, and feeds results in
//! through [`Picker::set_models`] and [`Picker::set_endpoints`].

use super::line;
use super::overlay::{self, Anchor};
use super::palette::{CYAN, Col, OVERLAY_BG, RED, SLATE, TAG_HUES};
use crate::provider::identity::{self, Account, AccountId};
use crate::provider::models::ProviderEndpoint;
use crate::provider::pricing::{ModelCaps, Param};
use crate::provider::{EFFORT_LADDER, Tuning};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use std::collections::BTreeMap;

pub use crate::provider::listing::{EndpointsState, ModelsState};

/// A serving provider, bound to the model it was chosen for and inert off it.
struct Route {
    model: String,
    slug: String,
}

/// A listed model chosen with Enter; `route` is `None` for auto.
pub struct Pick {
    pub account: Account,
    pub model: String,
    pub tuning: Tuning,
    pub route: Option<String>,
}

/// A rendered list row: a listed model and the account that lists it.
type Row = (Account, String);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Search,
    Provider,
    Effort,
    Temperature,
    TopP,
}

/// [`Picker::cycle`] drops `Provider` for a model that does not route.
const FOCUS_ORDER: &[Focus] = &[
    Focus::Search,
    Focus::Provider,
    Focus::Effort,
    Focus::Temperature,
    Focus::TopP,
];

/// One per [`EFFORT_LADDER`] rung; `auto` is a dot, not the smallest block.
const GLYPHS: &[&str] = &["·", "▁", "▂", "▄", "▆", "▇", "█"];

const _: () = assert!(
    GLYPHS.len() == EFFORT_LADDER.len(),
    "one glyph per effort-ladder rung"
);

const TEMP_MAX: f64 = 2.0;
const TEMP_STEP: f64 = 0.1;
const TEMP_PLACES: usize = 1;
/// A twentieth, so the common `0.9` and `0.95` land exactly.
const TOP_P_MAX: f64 = 1.0;
const TOP_P_STEP: f64 = 0.05;
const TOP_P_PLACES: usize = 2;
const TRACK_W: usize = 24;

const VISIBLE_ROWS: u16 = 8;
const BODY_W: u16 = 64;
/// The label gutter every row aligns to: two wider than `provider`.
const FIELD_LABEL: Col = Col::wide(10);
const HINT: &str = " tab field · ↑↓ pick · ←→ adjust · enter apply · esc cancel ";

const COLD: Color = Color::Rgb(96, 160, 235);
const WARM: Color = Color::Rgb(228, 116, 96);
const EFFORT_DIM: Color = Color::Rgb(74, 86, 110);
const EFFORT_BRIGHT: Color = Color::Rgb(150, 205, 220);

/// Top-p is a mass, not a temperature, so its track has one hue.
const NUCLEUS: Color = Color::Rgb(140, 196, 150);

/// Snap `t` to `places` decimals, so repeated steps never drift.
fn snap(t: f64, places: usize) -> f64 {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "places is a small decimal-place count"
    )]
    let scale = 10f64.powi(places as i32);
    (t * scale).round() / scale
}

/// Step a `0.0..=max` knob; `auto` (`None`) sits just below `0.0`.
fn step_knob(value: Option<f64>, up: bool, step: f64, max: f64, places: usize) -> Option<f64> {
    if up {
        Some(snap(value.map_or(0.0, |t| (t + step).min(max)), places))
    } else {
        match value {
            None => None,
            Some(t) if t - step < -f64::EPSILON => None,
            Some(t) => Some(snap((t - step).max(0.0), places)),
        }
    }
}

/// A provider's name, context window and quantization, space-padded so the
/// active tag reverses as a block.
fn provider_tag(endpoint: &ProviderEndpoint) -> String {
    let mut text = endpoint.provider_name.clone();
    if let Some(context_length) = endpoint.context_length {
        text.push(' ');
        text.push_str(&crate::provider::humanize_tokens(context_length));
    }
    if let Some(quantization) = &endpoint.quantization {
        text.push(' ');
        text.push_str(quantization);
    }
    format!(" {text} ")
}

pub struct Picker {
    query: String,
    /// In declaration order; also what [`identity::label`] disambiguates in.
    providers: Vec<Account>,
    models: BTreeMap<AccountId, ModelsState>,
    /// Keyed by `OpenRouter` model id.
    endpoints: BTreeMap<String, EndpointsState>,
    /// Into [`Self::rows`].
    selected: usize,
    focus: Focus,
    /// `None` throughout is auto.
    route: Option<Route>,
    effort_idx: usize,
    temperature: Option<f64>,
    top_p: Option<f64>,
    /// Injected so tests can stub the catalog.
    caps: fn(&str) -> ModelCaps,
    /// Why the picker opened, when that is news.
    note: Option<String>,
}

impl Picker {
    /// Open over `providers`, every list loading, on the tuning in force.
    pub fn new(providers: Vec<Account>, initial: &Tuning, caps: fn(&str) -> ModelCaps) -> Self {
        let models = providers
            .iter()
            .map(|account| (account.id.clone(), ModelsState::Loading))
            .collect();
        let effort_idx = EFFORT_LADDER
            .iter()
            .position(|(_, effort)| match (effort, &initial.effort) {
                (None, None) => true,
                (Some(a), Some(b)) => a.variant_name() == b.variant_name(),
                _ => false,
            })
            .unwrap_or(0);
        Self {
            query: String::new(),
            providers,
            models,
            endpoints: BTreeMap::new(),
            selected: 0,
            focus: Focus::Search,
            route: None,
            effort_idx,
            temperature: initial.temperature,
            top_p: initial.top_p,
            caps,
            note: None,
        }
    }

    /// Open with `note` above the failures, saying why the picker is up.
    #[must_use]
    pub fn noting(self, note: Option<String>) -> Self {
        Self { note, ..self }
    }

    fn label(&self, account: &Account) -> String {
        identity::label(account, &self.providers)
    }

    /// Record a provider's list, clamping the selection if it shrank.
    pub fn set_models(&mut self, id: &AccountId, state: ModelsState) {
        self.models.insert(id.clone(), state);
        self.clamp_selection();
    }

    pub fn set_endpoints(&mut self, model: &str, state: EndpointsState) {
        self.endpoints.insert(model.to_string(), state);
    }

    /// The highlighted `OpenRouter` model, until its endpoints are recorded:
    /// the driver's cue to fetch, deduped by seeding `Loading`.
    pub fn model_needing_endpoints(&self) -> Option<String> {
        let model = self.highlighted_routed_model(&self.rows())?;
        (!self.endpoints.contains_key(&model)).then_some(model)
    }

    pub fn is_loading(&self) -> bool {
        self.providers
            .iter()
            .any(|account| matches!(self.models.get(&account.id), Some(ModelsState::Loading)))
    }

    fn highlighted_model(&self, rows: &[Row]) -> Option<String> {
        rows.get(self.selected).map(|(_, model)| model.clone())
    }

    /// The highlighted model iff its service routes, as `OpenRouter` alone does.
    fn highlighted_routed_model(&self, rows: &[Row]) -> Option<String> {
        rows.get(self.selected)
            .filter(|(account, _)| account.service.routes)
            .map(|(_, model)| model.clone())
    }

    /// What the provider control cycles after `auto`.
    fn endpoint_slugs(&self, rows: &[Row]) -> Vec<String> {
        let Some(model) = self.highlighted_routed_model(rows) else {
            return Vec::new();
        };
        match self.endpoints.get(&model) {
            Some(EndpointsState::Loaded(endpoints)) => {
                endpoints.iter().map(|e| e.slug.clone()).collect()
            }
            _ => Vec::new(),
        }
    }

    /// The route slug, while the model it was chosen for is highlighted.
    fn active_route(&self, rows: &[Row]) -> Option<&str> {
        let route = self.route.as_ref()?;
        (self.highlighted_routed_model(rows).as_deref() == Some(route.model.as_str()))
            .then_some(route.slug.as_str())
    }

    /// `0` is auto, `i + 1` is `items[i]`.
    fn active_index<T>(&self, rows: &[Row], items: &[T], slug_of: impl Fn(&T) -> &str) -> usize {
        self.active_route(rows)
            .and_then(|s| items.iter().position(|it| slug_of(it) == s))
            .map_or(0, |i| i + 1)
    }

    /// Whether the highlighted model admits `param`; an unknown one does.
    fn supports(&self, rows: &[Row], param: Param) -> bool {
        match self.highlighted_model(rows) {
            Some(model) => (self.caps)(&model).supports(param),
            None => true,
        }
    }

    /// The tuning for the highlighted model, masked to what it admits.
    fn tuning(&self, rows: &[Row]) -> Tuning {
        Tuning {
            effort: self
                .supports(rows, Param::Reasoning)
                .then(|| EFFORT_LADDER[self.effort_idx].1.clone())
                .flatten(),
            temperature: self
                .supports(rows, Param::Temperature)
                .then_some(self.temperature)
                .flatten(),
            top_p: self
                .supports(rows, Param::TopP)
                .then_some(self.top_p)
                .flatten(),
        }
    }

    /// One key; Enter on a listed row picks it. The driver handles cancel.
    pub fn key(&mut self, code: ratatui::crossterm::event::KeyCode) -> Option<Pick> {
        use ratatui::crossterm::event::KeyCode;
        match code {
            KeyCode::Enter => {
                let rows = self.rows();
                return self.apply(&rows);
            }
            KeyCode::Tab => {
                let rows = self.rows();
                self.focus = self.cycle(&rows, true);
            }
            KeyCode::BackTab => {
                let rows = self.rows();
                self.focus = self.cycle(&rows, false);
            }
            // Typing always filters, whatever has focus.
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
            KeyCode::Up | KeyCode::Left => {
                let rows = self.rows();
                self.move_in_focus(&rows, false);
            }
            KeyCode::Down | KeyCode::Right => {
                let rows = self.rows();
                self.move_in_focus(&rows, true);
            }
            _ => {}
        }
        None
    }

    fn apply(&self, rows: &[Row]) -> Option<Pick> {
        let (account, model) = rows.get(self.selected)?;
        Some(Pick {
            account: account.clone(),
            model: model.clone(),
            tuning: self.tuning(rows),
            route: self.active_route(rows).map(str::to_string),
        })
    }

    fn cycle(&self, rows: &[Row], forward: bool) -> Focus {
        let has_provider = self.highlighted_routed_model(rows).is_some();
        let order: Vec<Focus> = FOCUS_ORDER
            .iter()
            .copied()
            .filter(|f| has_provider || *f != Focus::Provider)
            .collect();
        let pos = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward {
            (pos + 1) % order.len()
        } else {
            (pos + order.len() - 1) % order.len()
        };
        order[next]
    }

    /// `up` increases: down the list, up the ladder, warmer.
    fn move_in_focus(&mut self, rows: &[Row], up: bool) {
        match self.focus {
            Focus::Search => {
                if up {
                    let n = rows.len();
                    if n > 0 {
                        self.selected = (self.selected + 1).min(n - 1);
                    }
                } else {
                    self.selected = self.selected.saturating_sub(1);
                }
            }
            Focus::Provider => {
                let Some(model) = self.highlighted_routed_model(rows) else {
                    return;
                };
                let slugs = self.endpoint_slugs(rows);
                if slugs.is_empty() {
                    return;
                }
                let pos = self.active_index(rows, &slugs, String::as_str);
                let next = if up {
                    (pos + 1).min(slugs.len())
                } else {
                    pos.saturating_sub(1)
                };
                self.route = (next > 0).then(|| Route {
                    model,
                    slug: slugs[next - 1].clone(),
                });
            }
            Focus::Effort => {
                if !self.supports(rows, Param::Reasoning) {
                    return;
                }
                self.effort_idx = if up {
                    (self.effort_idx + 1).min(EFFORT_LADDER.len() - 1)
                } else {
                    self.effort_idx.saturating_sub(1)
                };
            }
            Focus::Temperature => {
                if !self.supports(rows, Param::Temperature) {
                    return;
                }
                self.temperature =
                    step_knob(self.temperature, up, TEMP_STEP, TEMP_MAX, TEMP_PLACES);
            }
            Focus::TopP => {
                if !self.supports(rows, Param::TopP) {
                    return;
                }
                self.top_p = step_knob(self.top_p, up, TOP_P_STEP, TOP_P_MAX, TOP_P_PLACES);
            }
        }
    }

    /// The rows matching the query, best first, each ranked on its own
    /// `label / model`: two accounts may list the same model.
    fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<(String, Row)> = Vec::new();
        for account in &self.providers {
            if let Some(ModelsState::Loaded(models)) = self.models.get(&account.id) {
                let label = self.label(account);
                for model in models {
                    rows.push((
                        format!("{label} / {model}"),
                        (account.clone(), model.clone()),
                    ));
                }
            }
        }
        ral_core::text::rank_by(
            self.query.trim(),
            rows,
            |r: &(String, _)| r.0.as_str(),
            false,
        )
        .into_iter()
        .map(|(_, row)| row)
        .collect()
    }

    fn clamp_selection(&mut self) {
        let n = self.rows().len();
        self.selected = if n == 0 { 0 } else { self.selected.min(n - 1) };
    }

    /// Headline and reason: why the picker opened, then each failed listing.
    fn notes(&self) -> impl Iterator<Item = (String, Option<String>)> {
        let failures =
            self.providers
                .iter()
                .filter_map(|account| match self.models.get(&account.id) {
                    Some(ModelsState::Failed(reason)) => Some((
                        format!("{} could not list its models", self.label(account)),
                        Some(reason.clone()),
                    )),
                    _ => None,
                });
        self.note
            .iter()
            .map(|note| (note.clone(), None))
            .chain(failures)
    }

    // --- rendering -----------------------------------------------------------

    /// Draw the overlay over the centre of `frame`, from one filtered row slice.
    pub fn render(&self, f: &mut Frame, frame: Rect) {
        let rows = self.rows();
        let width = overlay::body_width(BODY_W, frame);
        let notes = self.note_lines(width);
        let plane = Style::default().bg(OVERLAY_BG);
        // The provider row stands even for a model that does not route, so the
        // height never jumps; the list's border takes two more.
        let heights = [
            1,
            1,
            VISIBLE_ROWS + 2,
            1,
            1,
            1,
            1,
            u16::try_from(notes.len()).unwrap_or(u16::MAX),
        ];
        let body = overlay::draw(
            f,
            frame,
            Anchor::Centre,
            (width, heights.iter().sum()),
            " MODEL ",
            HINT,
        );
        let [search, status, list, provider, effort, temp, top_p, note] =
            Layout::vertical(heights.map(Constraint::Length)).areas(body);
        self.render_list(f, &rows, list, plane);
        for (line, at) in [
            (self.search_line(), search),
            (self.status_line(&rows), status),
            (self.provider_line(&rows, provider.width), provider),
            (
                self.effort_line(self.supports(&rows, Param::Reasoning)),
                effort,
            ),
            (
                self.temp_line(self.supports(&rows, Param::Temperature)),
                temp,
            ),
            (self.top_p_line(self.supports(&rows, Param::TopP)), top_p),
        ] {
            f.render_widget(Paragraph::new(line).style(plane), at);
        }
        f.render_widget(Paragraph::new(notes).style(plane), note);
    }

    fn field_label(&self, text: &str, field: Focus) -> Span<'static> {
        let style = if self.focus == field {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(SLATE).add_modifier(Modifier::DIM)
        };
        Span::styled(FIELD_LABEL.left(text), style)
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

    fn status_line(&self, rows: &[Row]) -> Line<'static> {
        let n = rows.len();
        let text = if self.is_loading() {
            "loading…".to_string()
        } else {
            format!("{n} match{}", if n == 1 { "" } else { "es" })
        };
        Line::from(Span::styled(
            format!("{}{text}", FIELD_LABEL.left("")),
            Style::default().fg(SLATE).add_modifier(Modifier::DIM),
        ))
    }

    fn render_list(&self, f: &mut Frame, rows: &[Row], area: Rect, plane: Style) {
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

        let window = list_area.height as usize;
        let start = self.selected.saturating_sub(window.saturating_sub(1));
        let lines: Vec<Line<'static>> = rows
            .iter()
            .enumerate()
            .skip(start)
            .take(window)
            .map(|(i, (account, model))| {
                let reversed = |style: Style| {
                    if i == self.selected {
                        style.add_modifier(Modifier::REVERSED)
                    } else {
                        style
                    }
                };
                Line::from(vec![
                    Span::styled(model.clone(), reversed(Style::default().fg(CYAN))),
                    Span::styled(
                        format!(" · {}", self.label(account)),
                        reversed(Style::default().fg(SLATE)),
                    ),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(lines).style(plane), list_area);
    }

    fn unsupported_row(label: &str) -> Line<'static> {
        let dim = Style::default().fg(SLATE).add_modifier(Modifier::DIM);
        Line::from(vec![
            Span::styled(FIELD_LABEL.left(label), dim),
            Span::styled(
                "not supported by this model",
                dim.add_modifier(Modifier::ITALIC),
            ),
        ])
    }

    fn provider_line(&self, rows: &[Row], width: u16) -> Line<'static> {
        let focused = self.focus == Focus::Provider;
        let label = self.field_label("provider", Focus::Provider);
        let dim = Style::default().fg(SLATE).add_modifier(Modifier::DIM);
        let auto_style = |active: bool| {
            if active && focused {
                Style::default()
                    .fg(CYAN)
                    .add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else if active {
                Style::default()
                    .fg(SLATE)
                    .add_modifier(Modifier::DIM | Modifier::REVERSED)
            } else {
                dim
            }
        };

        let Some(model) = self.highlighted_routed_model(rows) else {
            return Line::from(vec![
                label,
                Span::styled(
                    "OpenRouter routing only",
                    dim.add_modifier(Modifier::ITALIC),
                ),
            ]);
        };

        let endpoints: &[ProviderEndpoint] = match self.endpoints.get(&model) {
            Some(EndpointsState::Loaded(endpoints)) => endpoints,
            Some(EndpointsState::Loading) => {
                return Line::from(vec![label, Span::styled("loading…", dim)]);
            }
            Some(EndpointsState::Failed(reason)) => {
                return Line::from(vec![
                    label,
                    Span::styled(" auto ", auto_style(true)),
                    Span::styled(format!("  {reason}"), dim.add_modifier(Modifier::ITALIC)),
                ]);
            }
            None => &[],
        };

        let active = self.active_index(rows, endpoints, |e| e.slug.as_str());
        let tags: Vec<Span<'static>> =
            std::iter::once(Span::styled(" auto ", auto_style(active == 0)))
                .chain(
                    endpoints
                        .iter()
                        .zip(TAG_HUES.iter().cycle())
                        .enumerate()
                        .map(|(i, (endpoint, &hue))| {
                            let style = if i + 1 == active {
                                Style::default()
                                    .fg(hue)
                                    .add_modifier(Modifier::REVERSED | Modifier::BOLD)
                            } else {
                                Style::default().fg(hue).add_modifier(Modifier::DIM)
                            };
                            Span::styled(provider_tag(endpoint), style)
                        }),
                )
                .collect();
        let room = usize::from(width).saturating_sub(label.width());
        let mut spans = vec![label];
        spans.extend(line::window_around(&tags, active, room, dim));
        Line::from(spans)
    }

    fn effort_line(&self, supported: bool) -> Line<'static> {
        if !supported {
            return Self::unsupported_row("effort");
        }
        let focused = self.focus == Focus::Effort;
        #[allow(clippy::cast_precision_loss, reason = "const slice length")]
        let last = GLYPHS.len().saturating_sub(1).max(1) as f32;
        let mut spans = vec![self.field_label("effort", Focus::Effort)];
        for (i, glyph) in GLYPHS.iter().enumerate() {
            #[allow(
                clippy::cast_precision_loss,
                reason = "enumerate index over a small const-length slice"
            )]
            let value = super::rail::mix(EFFORT_DIM, EFFORT_BRIGHT, i as f32 / last);
            let mut style = Style::default().fg(value);
            if !focused {
                style = style.add_modifier(Modifier::DIM);
            }
            if i == self.effort_idx {
                style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
            }
            spans.push(Span::styled(*glyph, style));
        }
        // Every readout starts level with the tracks'.
        spans.push(Span::raw(" ".repeat(TRACK_W - GLYPHS.len())));
        let chosen = EFFORT_LADDER[self.effort_idx].0;
        let label_style = if focused {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(SLATE)
        };
        spans.push(Span::styled(format!("  {chosen}"), label_style));
        Line::from(spans)
    }

    /// [`TRACK_W`] cells filled to `value / max`; `auto` draws it all faint.
    fn track_line(
        &self,
        label: &str,
        field: Focus,
        value: Option<f64>,
        max: f64,
        places: usize,
        hue: impl Fn(usize) -> Color,
    ) -> Line<'static> {
        let focused = self.focus == field;
        let mut spans = vec![self.field_label(label, field)];

        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "t/max in [0,1], TRACK_W is a small const"
        )]
        let filled = value.map_or(0, |t| ((t / max) * TRACK_W as f64).round() as usize);
        for i in 0..TRACK_W {
            let on = value.is_some() && i < filled;
            let glyph = if on { "█" } else { "░" };
            let mut style = Style::default().fg(hue(i));
            if !on || !focused {
                style = style.add_modifier(Modifier::DIM);
            }
            spans.push(Span::styled(glyph, style));
        }

        let readout = match value {
            Some(t) => format!("  {t:.places$}"),
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

    fn temp_line(&self, supported: bool) -> Line<'static> {
        if !supported {
            return Self::unsupported_row("temp");
        }
        #[allow(clippy::cast_precision_loss, reason = "const track width")]
        let last = (TRACK_W - 1).max(1) as f32;
        #[allow(
            clippy::cast_precision_loss,
            reason = "enumerate index over a small const-length range"
        )]
        let hue = |i: usize| super::rail::mix(COLD, WARM, i as f32 / last);
        self.track_line(
            "temp",
            Focus::Temperature,
            self.temperature,
            TEMP_MAX,
            TEMP_PLACES,
            hue,
        )
    }

    fn top_p_line(&self, supported: bool) -> Line<'static> {
        if !supported {
            return Self::unsupported_row("top-p");
        }
        self.track_line(
            "top-p",
            Focus::TopP,
            self.top_p,
            TOP_P_MAX,
            TOP_P_PLACES,
            |_| NUCLEUS,
        )
    }

    /// Each note after a blank row: a bold `⚠` headline, then its reason
    /// wrapped, never truncated, beneath.
    fn note_lines(&self, width: u16) -> Vec<Line<'static>> {
        let width = width as usize;
        let bold = Style::default().fg(RED).add_modifier(Modifier::BOLD);
        let plain = Style::default().fg(RED);
        self.notes()
            .flat_map(|(headline, reason)| {
                let head = line::hang(
                    &[Span::styled("⚠ ", bold)],
                    vec![Span::styled(headline, bold)],
                    width,
                );
                let body = reason.into_iter().flat_map(|reason| {
                    line::hang(&[Span::raw("  ")], vec![Span::styled(reason, plain)], width)
                });
                std::iter::once(Line::default())
                    .chain(head)
                    .chain(body)
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
