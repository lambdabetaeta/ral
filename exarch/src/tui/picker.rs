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
use super::palette::{CYAN, Col, RED, SLATE};
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

impl Focus {
    const fn label(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Provider => "provider",
            Self::Effort => "effort",
            Self::Temperature => "temp",
            Self::TopP => "top-p",
        }
    }
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
/// The label gutter every row aligns to: two wider than the longest label.
const FIELD_LABEL: Col = Col::wide({
    let (mut i, mut w) = (0, 0);
    while i < FOCUS_ORDER.len() {
        let n = FOCUS_ORDER[i].label().len();
        w = if n > w { n } else { w };
        i += 1;
    }
    w + 2
});
const HINT: &str = " tab field · ↑↓ pick · ←→ adjust · enter apply · esc cancel ";

/// Seven hues far apart on the circle, cycled over a row of tags so
/// neighbours never blur.
pub(super) const TAG_HUES: [Color; 7] = [
    Color::Rgb(130, 190, 230),
    Color::Rgb(230, 150, 120),
    Color::Rgb(130, 210, 150),
    Color::Rgb(210, 170, 110),
    Color::Rgb(180, 140, 210),
    Color::Rgb(110, 200, 200),
    Color::Rgb(220, 140, 180),
];
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
    /// The rows matching the query, best first; see [`Self::refilter`].
    rows: Vec<Row>,
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
        let mut picker = Self {
            query: String::new(),
            providers,
            models,
            endpoints: BTreeMap::new(),
            rows: Vec::new(),
            selected: 0,
            focus: Focus::Search,
            route: None,
            effort_idx,
            temperature: initial.temperature,
            top_p: initial.top_p,
            caps,
            note: None,
        };
        picker.refilter();
        picker
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
        self.refilter();
    }

    pub fn set_endpoints(&mut self, model: &str, state: EndpointsState) {
        self.endpoints.insert(model.to_string(), state);
    }

    /// The highlighted `OpenRouter` model, until its endpoints are recorded:
    /// the driver's cue to fetch, deduped by seeding `Loading`.
    pub fn model_needing_endpoints(&self) -> Option<String> {
        let model = self.highlighted_routed_model()?;
        (!self.endpoints.contains_key(&model)).then_some(model)
    }

    pub fn is_loading(&self) -> bool {
        self.providers
            .iter()
            .any(|account| matches!(self.models.get(&account.id), Some(ModelsState::Loading)))
    }

    fn highlighted_model(&self) -> Option<String> {
        self.rows.get(self.selected).map(|(_, model)| model.clone())
    }

    /// The highlighted model iff its service routes, as `OpenRouter` alone does.
    fn highlighted_routed_model(&self) -> Option<String> {
        self.rows
            .get(self.selected)
            .filter(|(account, _)| account.service.routes)
            .map(|(_, model)| model.clone())
    }

    /// The route slug, while the model it was chosen for is highlighted.
    fn active_route(&self) -> Option<&str> {
        let route = self.route.as_ref()?;
        (self.highlighted_routed_model().as_deref() == Some(route.model.as_str()))
            .then_some(route.slug.as_str())
    }

    /// The active route's place among the highlighted model's endpoints;
    /// `None` is auto.
    fn active_index(&self) -> Option<usize> {
        let slug = self.active_route()?;
        let Some(EndpointsState::Loaded(endpoints)) =
            self.endpoints.get(&self.highlighted_routed_model()?)
        else {
            return None;
        };
        endpoints.iter().position(|e| e.slug == slug)
    }

    /// Whether the highlighted model admits `param`; an unknown one does.
    fn supports(&self, param: Param) -> bool {
        match self.highlighted_model() {
            Some(model) => (self.caps)(&model).supports(param),
            None => true,
        }
    }

    /// The tuning for the highlighted model, masked to what it admits.
    fn tuning(&self) -> Tuning {
        Tuning {
            effort: self
                .supports(Param::Reasoning)
                .then(|| EFFORT_LADDER[self.effort_idx].1.clone())
                .flatten(),
            temperature: self
                .supports(Param::Temperature)
                .then_some(self.temperature)
                .flatten(),
            top_p: self.supports(Param::TopP).then_some(self.top_p).flatten(),
        }
    }

    /// One key; Enter on a listed row picks it. The driver handles cancel.
    pub fn key(&mut self, code: ratatui::crossterm::event::KeyCode) -> Option<Pick> {
        use ratatui::crossterm::event::KeyCode;
        match code {
            KeyCode::Enter => return self.apply(),
            KeyCode::Tab => self.focus = self.cycle(true),
            KeyCode::BackTab => self.focus = self.cycle(false),
            // Typing always filters, whatever has focus.
            KeyCode::Char(c) => {
                self.focus = Focus::Search;
                self.query.push(c);
                self.selected = 0;
                self.refilter();
            }
            KeyCode::Backspace => {
                self.focus = Focus::Search;
                self.query.pop();
                self.selected = 0;
                self.refilter();
            }
            KeyCode::Up | KeyCode::Left => self.move_in_focus(false),
            KeyCode::Down | KeyCode::Right => self.move_in_focus(true),
            _ => {}
        }
        None
    }

    fn apply(&self) -> Option<Pick> {
        let (account, model) = self.rows.get(self.selected)?;
        Some(Pick {
            account: account.clone(),
            model: model.clone(),
            tuning: self.tuning(),
            route: self.active_route().map(str::to_string),
        })
    }

    fn cycle(&self, forward: bool) -> Focus {
        let routed = self.highlighted_routed_model().is_some();
        let order = FOCUS_ORDER
            .iter()
            .copied()
            .filter(|f| routed || *f != Focus::Provider);
        let before = |f: &Focus| *f != self.focus;
        let both = order.clone().chain(order);
        if forward {
            both.skip_while(before).nth(1)
        } else {
            both.rev().skip_while(before).nth(1)
        }
        .unwrap_or(Focus::Search)
    }

    /// `up` increases: down the list, up the ladder, warmer.
    fn move_in_focus(&mut self, up: bool) {
        match self.focus {
            Focus::Search => {
                if up {
                    self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1));
                } else {
                    self.selected = self.selected.saturating_sub(1);
                }
            }
            Focus::Provider => {
                let Some(model) = self.highlighted_routed_model() else {
                    return;
                };
                let Some(EndpointsState::Loaded(endpoints)) = self.endpoints.get(&model) else {
                    return;
                };
                let Some(last) = endpoints.len().checked_sub(1) else {
                    return;
                };
                let at = self.active_index();
                let next = if up {
                    Some(at.map_or(0, |i| (i + 1).min(last)))
                } else {
                    at.and_then(|i| i.checked_sub(1))
                };
                self.route = next.map(|i| Route {
                    model,
                    slug: endpoints[i].slug.clone(),
                });
            }
            Focus::Effort => {
                if !self.supports(Param::Reasoning) {
                    return;
                }
                self.effort_idx = if up {
                    (self.effort_idx + 1).min(EFFORT_LADDER.len() - 1)
                } else {
                    self.effort_idx.saturating_sub(1)
                };
            }
            Focus::Temperature => {
                if !self.supports(Param::Temperature) {
                    return;
                }
                self.temperature =
                    step_knob(self.temperature, up, TEMP_STEP, TEMP_MAX, TEMP_PLACES);
            }
            Focus::TopP => {
                if !self.supports(Param::TopP) {
                    return;
                }
                self.top_p = step_knob(self.top_p, up, TOP_P_STEP, TOP_P_MAX, TOP_P_PLACES);
            }
        }
    }

    /// Recompute [`Self::rows`], best first, each ranked on its own
    /// `label / model` (two accounts may list the same model), and clamp the
    /// selection into them.
    fn refilter(&mut self) {
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
        self.rows = ral_core::text::rank_by(
            self.query.trim(),
            rows,
            |r: &(String, _)| r.0.as_str(),
            false,
        )
        .into_iter()
        .map(|(_, row)| row)
        .collect();
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
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

    /// Draw the overlay over the centre of `frame`.
    pub fn render(&self, f: &mut Frame, frame: Rect) {
        let width = overlay::body_width(BODY_W, frame);
        let notes = self.note_lines(width);
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
            (width, usize::from(heights.iter().sum::<u16>())),
            " MODEL ",
            HINT,
        );
        let [search, status, list, provider, effort, temp, top_p, note] =
            Layout::vertical(heights.map(Constraint::Length)).areas(body);
        self.render_list(f, list);
        for (line, at) in [
            (self.search_line(), search),
            (self.status_line(), status),
            (self.provider_line(provider.width), provider),
            (self.effort_line(self.supports(Param::Reasoning)), effort),
            (self.temp_line(self.supports(Param::Temperature)), temp),
            (self.top_p_line(self.supports(Param::TopP)), top_p),
        ] {
            f.render_widget(Paragraph::new(line), at);
        }
        f.render_widget(Paragraph::new(notes), note);
    }

    fn field_label(&self, field: Focus) -> Span<'static> {
        let style = if self.focus == field {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(SLATE).add_modifier(Modifier::DIM)
        };
        Span::styled(FIELD_LABEL.left(field.label()), style)
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
            self.field_label(Focus::Search),
            Span::styled(self.query.clone(), query_style),
            Span::styled(caret, Style::default().fg(CYAN)),
        ])
    }

    fn status_line(&self) -> Line<'static> {
        let n = self.rows.len();
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

    fn render_list(&self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Search;
        let border = if focused {
            Style::default().fg(CYAN)
        } else {
            Style::default().fg(SLATE).add_modifier(Modifier::DIM)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border);
        let list_area = block.inner(area);
        f.render_widget(block, area);

        let window = list_area.height as usize;
        let start = self.selected.saturating_sub(window.saturating_sub(1));
        let lines: Vec<Line<'static>> = self
            .rows
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
        f.render_widget(Paragraph::new(lines), list_area);
    }

    fn unsupported_row(field: Focus) -> Line<'static> {
        let dim = Style::default().fg(SLATE).add_modifier(Modifier::DIM);
        Line::from(vec![
            Span::styled(FIELD_LABEL.left(field.label()), dim),
            Span::styled(
                "not supported by this model",
                dim.add_modifier(Modifier::ITALIC),
            ),
        ])
    }

    fn provider_line(&self, width: u16) -> Line<'static> {
        let focused = self.focus == Focus::Provider;
        let label = self.field_label(Focus::Provider);
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

        let Some(model) = self.highlighted_routed_model() else {
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

        let active = self.active_index();
        let tags: Vec<Span<'static>> =
            std::iter::once(Span::styled(" auto ", auto_style(active.is_none())))
                .chain(
                    endpoints
                        .iter()
                        .zip(TAG_HUES.iter().cycle())
                        .enumerate()
                        .map(|(i, (endpoint, &hue))| {
                            let style = if active == Some(i) {
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
        // Auto leads the tags.
        let at = active.map_or(0, |i| i + 1);
        spans.extend(line::window_around(&tags, at, room, dim));
        Line::from(spans)
    }

    fn effort_line(&self, supported: bool) -> Line<'static> {
        if !supported {
            return Self::unsupported_row(Focus::Effort);
        }
        let focused = self.focus == Focus::Effort;
        #[allow(clippy::cast_precision_loss, reason = "const slice length")]
        let last = GLYPHS.len().saturating_sub(1).max(1) as f32;
        let mut spans = vec![self.field_label(Focus::Effort)];
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
        field: Focus,
        value: Option<f64>,
        max: f64,
        places: usize,
        hue: impl Fn(usize) -> Color,
    ) -> Line<'static> {
        let focused = self.focus == field;
        let mut spans = vec![self.field_label(field)];

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
            return Self::unsupported_row(Focus::Temperature);
        }
        #[allow(clippy::cast_precision_loss, reason = "const track width")]
        let last = (TRACK_W - 1).max(1) as f32;
        #[allow(
            clippy::cast_precision_loss,
            reason = "enumerate index over a small const-length range"
        )]
        let hue = |i: usize| super::rail::mix(COLD, WARM, i as f32 / last);
        self.track_line(
            Focus::Temperature,
            self.temperature,
            TEMP_MAX,
            TEMP_PLACES,
            hue,
        )
    }

    fn top_p_line(&self, supported: bool) -> Line<'static> {
        if !supported {
            return Self::unsupported_row(Focus::TopP);
        }
        self.track_line(Focus::TopP, self.top_p, TOP_P_MAX, TOP_P_PLACES, |_| {
            NUCLEUS
        })
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
