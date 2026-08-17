//! The `/model` tuning overlay — a floating, bezel-framed modal.
//!
//! The rest of the TUI is a flat stack of strips; this one floats over a
//! [`Clear`]ed centre of the frame and is modal in behaviour too — `App::key`
//! returns early while an overlay is open, and `model_picker.rs` drives the
//! keys instead. It edits a model selection together with today's tuning: a
//! fuzzy-filtered `model · provider` list, the [`EFFORT_LADDER`] rung
//! `auto · zero · low · med · high · xhigh · max`, temperature, and top-p.
//!
//! For an `OpenRouter` `vendor/model` a fifth control appears, listing the
//! upstream providers serving *that* model; choosing one pins the request's
//! `provider.order`. It is a routing choice, not a filter, so it never moves
//! the highlighted model, and the row is inert (and skipped by `Tab`) for
//! every other provider — only `OpenRouter` routes.
//!
//! The tuning rows gate on the *highlighted* model's catalog
//! `supported_parameters`: a parameter the catalog positively reports absent
//! grays its row, ignores its arrows, and is masked out of the emitted
//! [`Tuning`] so it is never sent — while the picker keeps the user's setting
//! for the next model that does admit it. An unknown model reads as supported.
//!
//! The picker is pure display+input. `model_picker.rs` owns the credential
//! store, the catalog, and the network seam, and feeds results in through
//! [`Picker::set_models`] and [`Picker::set_endpoints`], so a list reads
//! "loading…" until its background fetch lands. The serving-provider fetch is
//! intent-driven: the driver reads
//! [`Picker::focused_or_model_needing_endpoints`] and fetches a model's
//! providers only once the provider control is focused on it.

use super::line;
use super::palette::{BANNER_GOLD, CYAN, OVERLAY_BG, RED, SLATE};
use crate::provider::identity::{self, Account, AccountId};
use crate::provider::models::ProviderEndpoint;
use crate::provider::{EFFORT_LADDER, Tuning};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use std::cmp::Reverse;
use std::collections::BTreeMap;

pub use crate::provider::listing::{EndpointsState, ModelsState};

/// A serving-provider choice bound to the model it was made for: inert unless
/// that model is still highlighted, so a route can never ride a model whose
/// providers it was not chosen from.
struct Route {
    model: String,
    slug: String,
}

/// What a key press resolved to; `model_picker.rs` acts on the non-`None`
/// outcomes. Cancellation (Esc, Ctrl-C, Ctrl-D) is resolved by the driver
/// before a key reaches [`Picker::key`], so it never appears here.
pub enum PickAction {
    None,
    /// A listed row, with the live tuning and the chosen serving-provider slug
    /// (`None` for auto, and for every provider that does not route).
    Selected(Account, String, Tuning, Option<String>),
    /// The raw query as a model name, for `model_picker.rs` to resolve against
    /// the listings — the escape hatch when a fetch failed or the wanted model
    /// is unlisted. Such a model has no fetched endpoints, so it carries no route.
    Manual(String, Tuning),
}

/// A rendered list row: a listed model, or the synthetic manual-entry row.
enum Row {
    Model(Account, String),
    Manual(String),
}

/// Which control holds the keyboard; it renders bright and the others dim.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Search,
    Provider,
    Effort,
    Temperature,
    TopP,
}

/// The one cycle order, forward; [`Picker::cycle`] filters `Provider` out of it
/// rather than a second order being spelled out without that rung.
const FOCUS_ORDER: &[Focus] = &[
    Focus::Search,
    Focus::Provider,
    Focus::Effort,
    Focus::Temperature,
    Focus::TopP,
];

/// One ramp glyph per [`EFFORT_LADDER`] rung, in the same order. `auto` wears a
/// dot rather than a block so it reads as "no setting" rather than "the
/// smallest setting".
const GLYPHS: &[&str] = &["·", "▁", "▂", "▄", "▆", "▇", "█"];

const _: () = assert!(
    GLYPHS.len() == EFFORT_LADDER.len(),
    "one glyph per effort-ladder rung"
);

/// genai accepts `0.0..=2.0`; the overlay steps by tenths.
const TEMP_MAX: f64 = 2.0;
const TEMP_STEP: f64 = 0.1;
const TEMP_PLACES: usize = 1;
/// genai accepts `0.0..=1.0`; the step is a twentieth so the common `0.9` and
/// `0.95` land exactly.
const TOP_P_MAX: f64 = 1.0;
const TOP_P_STEP: f64 = 0.05;
const TOP_P_PLACES: usize = 2;
/// Cells in a tuning track: the hue gradient and the fill both span this width.
const TRACK_W: usize = 24;

const VISIBLE_ROWS: u16 = 8;
/// The overlay's fixed content width; clamped to the frame when narrower.
const OVERLAY_W: u16 = 74;

/// The airy margin inside the bezel, so the controls never crowd the frame.
/// `pub(super)`: `login.rs` renders its modal through the same chrome.
pub(super) const PAD_X: u16 = 4;
pub(super) const PAD_Y: u16 = 1;
/// Columns of shade cast to the right, against one row below: cells are about
/// 2:1, so two columns read as square as a single row.
const SHADOW_DEPTH: u16 = 2;
/// A shadowed cell keeps its glyph and is repainted this near-black, so what
/// lies beneath shows as a faint silhouette rather than being blanked.
const SHADOW_FG: Color = Color::Rgb(14, 16, 22);

/// Endpoints of the temperature gradient (cold blue → warm red) and of the
/// effort ramp (dim → bright cyan), interpolated by `rail::mix`.
const COLD: Color = Color::Rgb(96, 160, 235);
const WARM: Color = Color::Rgb(228, 116, 96);
const EFFORT_DIM: Color = Color::Rgb(74, 86, 110);
const EFFORT_BRIGHT: Color = Color::Rgb(150, 205, 220);

/// The top-p track's one hue — top-p is nucleus *mass*, not a temperature, so
/// it earns no gradient and the fill alone encodes it. A muted green, kept
/// clear of the effort ramp's cyan and the temperature track's blue→red.
const NUCLEUS: Color = Color::Rgb(140, 196, 150);

/// Snap `t` to `places` decimals, so repeated `±step` additions never drift
/// into float noise like `0.30000000000000004`.
fn snap(t: f64, places: usize) -> f64 {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "places is a small decimal-place count"
    )]
    let scale = 10f64.powi(places as i32);
    (t * scale).round() / scale
}

/// Step a `0.0..=max` knob by `step` in the `up` direction. `auto` (`None`) sits
/// below the floor: the first up-step lands on `0.0`, and stepping below zero
/// returns to `auto`.
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

/// A tag's text: the provider name, then the context window and quantization
/// that tell apart providers of the same model. The wrapping spaces give the
/// active tag its reversed-block look.
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

/// Centre a `w × h` rect within `area`, clamped to fit. Shared with `login.rs`.
pub(super) fn centered(w: u16, h: u16, area: Rect) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// Cast a drop shadow down-and-right of `area`, so the modal reads as lifted
/// off the session rather than punched into it. `cell_mut` bounds-checks, so
/// cells falling off the frame are simply skipped.
pub(super) fn render_shadow(f: &mut Frame, area: Rect) {
    let shadow = Style::default().fg(SHADOW_FG).bg(Color::Black);
    let buf = f.buffer_mut();
    let mut cast = |x: u16, y: u16| {
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_style(shadow);
        }
    };
    // The bottom edge, shifted right by the depth so the corner squares off.
    for x in (area.x + SHADOW_DEPTH)..area.right().saturating_add(SHADOW_DEPTH) {
        cast(x, area.bottom());
    }
    // The right edge, starting one row down so the top-right corner stays
    // unshaded — the light falls from the upper left.
    for y in (area.y + 1)..=area.bottom() {
        for dx in 0..SHADOW_DEPTH {
            cast(area.right() + dx, y);
        }
    }
}

/// Cast the shadow, clear the box, and draw the double-line bezel around
/// `area`, returning the inner content rect. The one shell both `/model` and
/// `/login` render into.
pub(super) fn overlay_frame(f: &mut Frame, area: Rect, title: &str, hint: &str) -> Rect {
    let plane = Style::default().bg(OVERLAY_BG);
    render_shadow(f, area);
    f.render_widget(Clear, area);
    let bezel = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD))
        .style(plane)
        .title(
            Line::from(Span::styled(
                title.to_string(),
                plane.fg(BANNER_GOLD).add_modifier(Modifier::BOLD),
            ))
            .centered(),
        )
        .title_bottom(
            Line::from(Span::styled(
                hint.to_string(),
                plane.fg(SLATE).add_modifier(Modifier::DIM),
            ))
            .centered(),
        )
        .padding(Padding::new(PAD_X, PAD_X, PAD_Y, PAD_Y));
    let inner = bezel.inner(area);
    f.render_widget(bezel, area);
    inner
}

pub struct Picker {
    query: String,
    /// In declaration order; their lists fill in as fetches land. Also the set
    /// [`identity::label`] disambiguates a row's account within.
    providers: Vec<Account>,
    models: BTreeMap<AccountId, ModelsState>,
    /// Keyed by `OpenRouter` model id; absent until the provider control is
    /// first focused on that model.
    endpoints: BTreeMap<String, EndpointsState>,
    /// Index into the current filtered [`Self::rows`].
    selected: usize,
    focus: Focus,
    /// `None` is auto — `OpenRouter` decides.
    route: Option<Route>,
    /// Index into [`EFFORT_LADDER`].
    effort_idx: usize,
    /// `None` is auto (unset).
    temperature: Option<f64>,
    /// `None` is auto (unset).
    top_p: Option<f64>,
    /// Capability lookup for the *highlighted* model, injected so the picker
    /// stays a pure component tests can stub; `model_picker.rs` passes
    /// [`crate::provider::pricing::caps_or_default`].
    caps: fn(&str) -> crate::provider::pricing::ModelCaps,
}

impl Picker {
    /// Open over `providers`, every list loading until `model_picker.rs` feeds
    /// in a cached or fetched one. `initial` seeds the tuning controls from the
    /// focused provider's live values, so reopening shows what is in force.
    pub fn new(
        providers: Vec<Account>,
        initial: &Tuning,
        caps: fn(&str) -> crate::provider::pricing::ModelCaps,
    ) -> Self {
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
        }
    }

    /// The row's name, drawn relative to every account open in this picker —
    /// the one place a label is computed, and it is not computed here.
    fn label(&self, account: &Account) -> String {
        identity::label(account, &self.providers)
    }

    /// Record a provider's resolved (or failed) list, clamping the selection in
    /// case the visible list shrank.
    pub fn set_models(&mut self, id: &AccountId, state: ModelsState) {
        self.models.insert(id.clone(), state);
        self.clamp_selection();
    }

    /// Record a model's serving-provider fetch result, or its in-flight state.
    pub fn set_endpoints(&mut self, model: &str, state: EndpointsState) {
        self.endpoints.insert(model.to_string(), state);
    }

    /// `model_picker.rs`'s cue to spawn a serving-provider fetch: the
    /// highlighted `OpenRouter` model, once the provider control is focused on
    /// it and no [`EndpointsState`] is recorded — so seeding `Loading` before
    /// spawning is what dedups the fetch.
    pub fn focused_or_model_needing_endpoints(&self) -> Option<String> {
        if self.focus != Focus::Provider {
            return None;
        }
        let model = self.highlighted_or_model(&self.rows())?;
        (!self.endpoints.contains_key(&model)).then_some(model)
    }

    /// Whether any provider's model list is still in flight.
    pub fn is_loading(&self) -> bool {
        self.providers
            .iter()
            .any(|account| matches!(self.models.get(&account.id), Some(ModelsState::Loading)))
    }

    /// The model on the highlighted row, if it is a listed one — the manual row
    /// and an empty list have none to gate against. Reads the row slice the
    /// caller already computed for this frame or key.
    fn highlighted_model(&self, rows: &[Row]) -> Option<String> {
        match rows.get(self.selected) {
            Some(Row::Model(_, model)) => Some(model.clone()),
            _ => None,
        }
    }

    /// The highlighted model iff its service routes — `OpenRouter` alone, so
    /// the only one the provider control applies to.
    fn highlighted_or_model(&self, rows: &[Row]) -> Option<String> {
        match rows.get(self.selected) {
            Some(Row::Model(account, model)) if account.service.routes => Some(model.clone()),
            _ => None,
        }
    }

    /// What the provider control cycles after `auto`, in listed order. Empty
    /// while loading, failed, or unfetched, and for a row that does not route.
    fn endpoint_slugs(&self, rows: &[Row]) -> Vec<String> {
        let Some(model) = self.highlighted_or_model(rows) else {
            return Vec::new();
        };
        match self.endpoints.get(&model) {
            Some(EndpointsState::Loaded(endpoints)) => {
                endpoints.iter().map(|e| e.slug.clone()).collect()
            }
            _ => Vec::new(),
        }
    }

    /// The route slug in force — `Some` only while the model it was chosen for
    /// is the highlighted one, so a choice can never leak onto another model.
    fn active_route(&self, rows: &[Row]) -> Option<&str> {
        let route = self.route.as_ref()?;
        (self.highlighted_or_model(rows).as_deref() == Some(route.model.as_str()))
            .then_some(route.slug.as_str())
    }

    /// The index convention the provider control's cycling and its tag
    /// rendering must agree on: `0` is auto, `i + 1` is `items[i]`.
    fn active_index<T>(&self, rows: &[Row], items: &[T], slug_of: impl Fn(&T) -> &str) -> usize {
        self.active_route(rows)
            .and_then(|s| items.iter().position(|it| slug_of(it) == s))
            .map_or(0, |i| i + 1)
    }

    /// Whether the highlighted model admits `param`. An unknown model — catalog
    /// miss, manual row, list still loading — reads as supported, so a row grays
    /// only when the catalog *positively* reports the parameter absent.
    fn supports(&self, rows: &[Row], param: &str) -> bool {
        match self.highlighted_model(rows) {
            Some(model) => (self.caps)(&model).supports(param),
            None => true,
        }
    }

    /// The live tuning *for the highlighted model*: a knob it does not admit is
    /// masked to `None` so it is neither sent nor persisted, while the picker
    /// keeps the setting for the next model that does.
    fn tuning(&self, rows: &[Row]) -> Tuning {
        Tuning {
            effort: self
                .supports(rows, "reasoning")
                .then(|| EFFORT_LADDER[self.effort_idx].1.clone())
                .flatten(),
            temperature: self
                .supports(rows, "temperature")
                .then_some(self.temperature)
                .flatten(),
            top_p: self.supports(rows, "top_p").then_some(self.top_p).flatten(),
        }
    }

    /// Handle one key press. The driver intercepts Esc, Ctrl-C and Ctrl-D as
    /// the shared cancel chord before a key reaches here, so this never sees one.
    pub fn key(&mut self, code: ratatui::crossterm::event::KeyCode) -> PickAction {
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
            // Typing always means "filter models", whatever field had focus.
            // The route self-gates on the highlighted model, so a query that
            // moves the highlight off it merely deactivates the route.
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
        PickAction::None
    }

    /// Resolve the highlighted row into a selection carrying the live tuning and
    /// the route in force for that model.
    fn apply(&self, rows: &[Row]) -> PickAction {
        match rows.get(self.selected) {
            Some(Row::Model(account, model)) => {
                let route = self.active_route(rows).map(str::to_string);
                PickAction::Selected(account.clone(), model.clone(), self.tuning(rows), route)
            }
            Some(Row::Manual(query)) => PickAction::Manual(query.clone(), self.tuning(rows)),
            None => PickAction::None,
        }
    }

    /// The next focus. The provider control joins the cycle only when an
    /// `OpenRouter` model is highlighted; otherwise it is skipped.
    fn cycle(&self, rows: &[Row], forward: bool) -> Focus {
        let has_provider = self.highlighted_or_model(rows).is_some();
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

    /// Move the focused control. `up` is the increasing direction: down the
    /// list, up the ladder, warmer.
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
                // `auto → slug₀ → slug₁ → …`, clamping at the ends. A route is
                // a choice *about* the highlighted model, so the selection
                // deliberately stays put.
                let Some(model) = self.highlighted_or_model(rows) else {
                    return;
                };
                let slugs = self.endpoint_slugs(rows);
                if slugs.is_empty() {
                    return; // loading / failed / none — nothing to cycle yet
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
                if !self.supports(rows, "reasoning") {
                    return;
                }
                self.effort_idx = if up {
                    (self.effort_idx + 1).min(EFFORT_LADDER.len() - 1)
                } else {
                    self.effort_idx.saturating_sub(1)
                };
            }
            Focus::Temperature => {
                if !self.supports(rows, "temperature") {
                    return;
                }
                self.temperature =
                    step_knob(self.temperature, up, TEMP_STEP, TEMP_MAX, TEMP_PLACES);
            }
            Focus::TopP => {
                if !self.supports(rows, "top_p") {
                    return;
                }
                self.top_p = step_knob(self.top_p, up, TOP_P_STEP, TOP_P_MAX, TOP_P_PLACES);
            }
        }
    }

    /// The `(provider, model)` pairs matching the fuzzy query, best score
    /// first; the sort is stable, so ties keep listed order, as does an empty
    /// query.
    fn query_matches(&self) -> Vec<(Account, String)> {
        let q = self.query.trim();

        // Candidates and haystacks are positionally paired.
        let mut candidates: Vec<(Account, String)> = Vec::new();
        let mut haystacks: Vec<String> = Vec::new();
        for account in &self.providers {
            if let Some(ModelsState::Loaded(models)) = self.models.get(&account.id) {
                let label = self.label(account);
                for model in models {
                    haystacks.push(format!("{label} / {model}"));
                    candidates.push((account.clone(), model.clone()));
                }
            }
        }

        if q.is_empty() {
            return candidates;
        }

        // Score by index, so a row survives even when two providers list the
        // same model name.
        let pattern = Pattern::parse(q, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        // A matcher per call: `&self` cannot lend a stored one mutably, and one
        // keystroke is one pass over a small list.
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
        scored.sort_by_key(|(_, score)| Reverse(*score));
        scored
            .into_iter()
            .map(|(i, _)| candidates[i].clone())
            .collect()
    }

    /// The query matches, plus a synthetic manual-entry row while the query is
    /// non-empty, so an unlisted model — or one whose provider's fetch failed —
    /// is still reachable. The route is not a filter and does not narrow this.
    fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .query_matches()
            .into_iter()
            .map(|(account, model)| Row::Model(account, model))
            .collect();
        let q = self.query.trim();
        if !q.is_empty() {
            rows.push(Row::Manual(q.to_string()));
        }
        rows
    }

    fn clamp_selection(&mut self) {
        let n = self.rows().len();
        self.selected = if n == 0 { 0 } else { self.selected.min(n - 1) };
    }

    /// Providers whose fetch failed, with their reasons — rendered as notes so
    /// the absent models are explained and the manual fallback is obvious.
    fn failures(&self) -> Vec<(&Account, &str)> {
        self.providers
            .iter()
            .filter_map(|account| match self.models.get(&account.id) {
                Some(ModelsState::Failed(reason)) => Some((account, reason.as_str())),
                _ => None,
            })
            .collect()
    }

    // --- rendering -----------------------------------------------------------

    /// The overlay's outer size: the fixed width, clamped to the frame, over a
    /// height that fits every row [`Self::render`] emits.
    fn desired_size(&self, frame: Rect) -> (u16, u16) {
        let w = OVERLAY_W.min(frame.width);
        // The inner text column: the width less the two bezel cells and the
        // padding on each side. Counting the pre-wrapped note lines reserves
        // exactly the rows the render emits.
        let note_width = w.saturating_sub(2 + 2 * PAD_X);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "wrapped note-row count; bounded by the tiny catalog"
        )]
        let failed = self.failed_lines(note_width).len() as u16;
        // bezel + pad + search + status + list + provider + effort + temp +
        // top-p + notes. The provider row is reserved even for a model that
        // does not route, so the overlay's height never jumps.
        let h = 2 + 2 * PAD_Y + 1 + 1 + (VISIBLE_ROWS + 2) + 1 + 1 + 1 + 1 + failed;
        (w, h.min(frame.height.max(3)))
    }

    /// Draw the overlay over the centre of `frame`. The filtered row slice is
    /// computed once here and threaded through every row-reading helper, so one
    /// frame reads one list.
    pub fn render(&self, f: &mut Frame, frame: Rect) {
        let (w, h) = self.desired_size(frame);
        let area = centered(w, h, frame);
        let plane = Style::default().bg(OVERLAY_BG);
        let rows = self.rows();

        let inner = overlay_frame(
            f,
            area,
            " MODEL ",
            " ⇥ field · ↑↓ pick · ←→ adjust · ⏎ apply · esc cancel ",
        );

        // The eight rows `desired_size` sums the height of; the list carries
        // its own border, hence the two extra cells.
        let [search, status, list, provider, effort, temp, top_p, notes] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(VISIBLE_ROWS + 2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(inner);

        f.render_widget(Paragraph::new(self.search_line()).style(plane), search);
        f.render_widget(Paragraph::new(self.status_line(&rows)).style(plane), status);
        self.render_list(f, &rows, list, plane);
        f.render_widget(
            Paragraph::new(self.provider_line(&rows, provider.width)).style(plane),
            provider,
        );
        f.render_widget(
            Paragraph::new(self.effort_line(self.supports(&rows, "reasoning"))).style(plane),
            effort,
        );
        f.render_widget(
            Paragraph::new(self.temp_line(self.supports(&rows, "temperature"))).style(plane),
            temp,
        );
        f.render_widget(
            Paragraph::new(self.top_p_line(self.supports(&rows, "top_p"))).style(plane),
            top_p,
        );
        let note_lines = self.failed_lines(notes.width);
        if !note_lines.is_empty() {
            f.render_widget(Paragraph::new(note_lines).style(plane), notes);
        }
    }

    /// A field label in the seven-column gutter every row aligns to, bright when
    /// focused, so the eye finds the live control by lightness alone.
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

    fn status_line(&self, rows: &[Row]) -> Line<'static> {
        let n = rows.len();
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

    /// The model list in its own rounded panel: the selected row reversed, the
    /// border brightening when the search field has focus.
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
        // Scroll so the selected row stays visible.
        let start = self.selected.saturating_sub(window.saturating_sub(1));
        let lines: Vec<Line<'static>> = rows
            .iter()
            .enumerate()
            .skip(start)
            .take(window)
            .map(|(i, row)| {
                let reversed = |style: Style| {
                    if i == self.selected {
                        style.add_modifier(Modifier::REVERSED)
                    } else {
                        style
                    }
                };
                match row {
                    Row::Model(account, m) => Line::from(vec![
                        Span::styled(m.clone(), reversed(Style::default().fg(CYAN))),
                        Span::styled(
                            format!(" · {}", self.label(account)),
                            reversed(Style::default().fg(SLATE)),
                        ),
                    ]),
                    Row::Manual(q) => Line::from(Span::styled(
                        format!("use “{q}” as a manual model"),
                        reversed(Style::default().fg(SLATE).add_modifier(Modifier::ITALIC)),
                    )),
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines).style(plane), list_area);
    }

    /// A grayed tuning row: the label plus a note, so a knob the model does not
    /// admit reads as disabled rather than missing.
    fn unsupported_row(label: &str) -> Line<'static> {
        let dim = Style::default().fg(SLATE).add_modifier(Modifier::DIM);
        Line::from(vec![
            Span::styled(format!("{label:<7}"), dim),
            Span::styled(
                "— not supported by this model",
                dim.add_modifier(Modifier::ITALIC),
            ),
        ])
    }

    /// The serving-provider row for the highlighted model: an inert note when
    /// it does not route, `loading…`, `auto` beside the failure reason, or an
    /// `auto` tag and one hue-coded tag per provider with the active one
    /// reversed.
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

        let Some(model) = self.highlighted_or_model(rows) else {
            return Line::from(vec![
                label,
                Span::styled(
                    "— OpenRouter routing only",
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
            // Not yet fetched: `auto` alone, and focusing the control here is
            // what triggers the fetch.
            None => &[],
        };

        // Seven hues far apart on the circle, so neighbouring tags never blur.
        let colors: &[Color] = &[
            Color::Rgb(130, 190, 230), // blue
            Color::Rgb(230, 150, 120), // warm red
            Color::Rgb(130, 210, 150), // green
            Color::Rgb(210, 170, 110), // gold
            Color::Rgb(180, 140, 210), // violet
            Color::Rgb(110, 200, 200), // teal
            Color::Rgb(220, 140, 180), // rose
        ];
        let active = self.active_index(rows, endpoints, |e| e.slug.as_str());

        let mut spans = vec![label, Span::styled(" auto ", auto_style(active == 0))];
        for (i, endpoint) in endpoints.iter().enumerate() {
            let hue = colors[i % colors.len()];
            let style = if (i + 1) == active {
                Style::default()
                    .fg(hue)
                    .add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default().fg(hue).add_modifier(Modifier::DIM)
            };
            spans.push(Span::styled(provider_tag(endpoint), style));
        }
        let total: usize = spans.iter().map(Span::width).sum();
        if total <= width as usize {
            return Line::from(spans);
        }

        // Overflow: keep the label and a window of tags around the active one,
        // so the highlight never falls off the edge.
        let active_span = active + 1; // in spans: 1 = auto, 2 = first tag, …
        let budget = (width as usize).saturating_sub(spans[0].width());

        let mut lo = active_span;
        let mut hi = active_span + 1; // exclusive
        let mut used = spans[active_span].width();

        while lo > 1 && used + spans[lo - 1].width() <= budget {
            lo -= 1;
            used += spans[lo].width();
        }
        while hi < spans.len() && used + spans[hi].width() <= budget {
            used += spans[hi].width();
            hi += 1;
        }

        let mut result = vec![spans[0].clone()];
        if lo > 1 {
            result.push(Span::styled(" … ", dim));
        }
        result.extend(spans[lo..hi].iter().cloned());
        if hi < spans.len() {
            result.push(Span::styled(" …", dim));
        }
        Line::from(result)
    }
    /// The effort ladder: an ascending block ramp that brightens as it grows,
    /// the chosen rung reversed and named. Grayed when the highlighted model
    /// has no reasoning effort to set.
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
        let chosen = EFFORT_LADDER[self.effort_idx].0;
        let label_style = if focused {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(SLATE)
        };
        spans.push(Span::styled(format!("  {chosen}"), label_style));
        Line::from(spans)
    }

    /// The chassis the temperature and top-p tracks share, differing only in
    /// `hue`: [`TRACK_W`] cells filled to `value`'s fraction of `max`, with
    /// `auto` (`None`) drawing the whole track faint.
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

    /// The temperature track: a cold→warm gradient filled to length. Grayed
    /// when the highlighted model does not admit a temperature.
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

    /// The top-p track: one [`NUCLEUS`] hue, the fill alone encoding the value.
    /// Grayed when the highlighted model does not admit a top-p.
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

    /// Each failed-provider note wrapped to `width`, a `⚠ ` opening the first
    /// row and continuations hanging under it. The reason is wrapped rather
    /// than truncated — it is the point of the note. Wrapping through
    /// [`line::push_wrapped`] lets these lines double as the exact height
    /// [`Self::desired_size`] reserves.
    fn failed_lines(&self, width: u16) -> Vec<Line<'static>> {
        const MARKER: &str = "⚠ ";
        const HANG: &str = "  ";
        let style = Style::default().fg(RED).add_modifier(Modifier::BOLD);
        // Leave the marker gutter out of the wrap width, so neither it nor the
        // hanging indent can push a row past `width`.
        let body_w = (width as usize).saturating_sub(MARKER.chars().count());
        let mut out = Vec::new();
        for (account, reason) in self.failures() {
            let text = format!(
                "{} — fetch failed: {reason} (type a model to enter manually)",
                self.label(account)
            );
            line::push_wrapped(&mut out, &text, body_w, |chunk, first| {
                let lead = if first { MARKER } else { HANG };
                Line::from(Span::styled(format!("{lead}{chunk}"), style))
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ReasoningEffort;
    use ratatui::crossterm::event::KeyCode;

    /// A built-in service's sole account, named by the service alone.
    fn account(name: &str) -> Account {
        let service = identity::built_in(&identity::ServiceName::declared(name).unwrap())
            .expect("a known built-in service name");
        Account::of_service(service)
    }

    /// A declared (non-built-in) service's sole account — the shape a
    /// `config.ral` endpoint takes.
    fn declared(name: &str) -> Account {
        Account::of_service(identity::Service {
            name: identity::ServiceName::declared(name).unwrap(),
            endpoint: Some(format!("https://{name}.example/v1/")),
            adapter: genai::adapter::AdapterKind::OpenAI,
            default_model: None,
            auth: identity::Auth::Env(format!("{}_KEY", name.to_uppercase())),
            billing: identity::Billing::Metered,
            routes: false,
        })
    }

    /// A `ChatGPT` login — the one shape whose accounts can collide on handle.
    fn login(handle: &str, issued: &str) -> Account {
        let service = identity::chatgpt_service();
        Account {
            id: AccountId::of_login(&service.name, issued),
            service,
            handle: handle.to_string(),
        }
    }

    /// A stub that knows nothing: an empty `supported_parameters` reads as
    /// "supports everything", so every tuning row stays live.
    fn caps_unknown(_: &str) -> crate::provider::pricing::ModelCaps {
        crate::provider::pricing::ModelCaps::default()
    }

    fn loaded_picker() -> Picker {
        let anthropic = account("anthropic");
        let deepseek = account("deepseek");
        let mut p = Picker::new(
            vec![anthropic.clone(), deepseek.clone()],
            &Tuning::default(),
            caps_unknown,
        );
        p.set_models(
            &anthropic.id,
            ModelsState::Loaded(vec!["claude-opus-4".into(), "claude-haiku-4".into()]),
        );
        p.set_models(
            &deepseek.id,
            ModelsState::Loaded(vec!["deepseek-chat".into()]),
        );
        p
    }

    /// Context window and quantization elided: these tests read only the slug.
    fn endpoint(name: &str, slug: &str) -> ProviderEndpoint {
        ProviderEndpoint {
            provider_name: name.into(),
            slug: slug.into(),
            context_length: None,
            quantization: None,
        }
    }

    /// `vendor/model` ids — the case the serving-provider control exists for.
    fn openrouter_picker() -> Picker {
        let openrouter = account("openrouter");
        let mut p = Picker::new(vec![openrouter.clone()], &Tuning::default(), caps_unknown);
        p.set_models(
            &openrouter.id,
            ModelsState::Loaded(vec![
                "anthropic/claude-3".into(),
                "deepseek/deepseek-chat".into(),
                "deepseek/deepseek-r1".into(),
                "openai/gpt-5".into(),
            ]),
        );
        p
    }

    /// The `model · provider` labels of every listed row, in order.
    fn row_labels(p: &Picker) -> Vec<String> {
        p.rows()
            .into_iter()
            .filter_map(|r| match r {
                Row::Model(account, m) => Some(format!("{m} · {}", p.label(&account))),
                Row::Manual(_) => None,
            })
            .collect()
    }

    #[test]
    fn empty_query_shows_all_loaded_models() {
        let p = loaded_picker();
        assert_eq!(
            row_labels(&p),
            vec![
                "claude-opus-4 · anthropic",
                "claude-haiku-4 · anthropic",
                "deepseek-chat · deepseek",
            ]
        );
    }

    /// A lone `ChatGPT` account has nothing to collide with, so its row keeps
    /// its email rather than falling back to the id.
    #[test]
    fn a_lone_chatgpt_account_row_keeps_its_email() {
        let alex = login("alex@bristol.ac.uk", "acct-1");
        let mut p = Picker::new(vec![alex.clone()], &Tuning::default(), caps_unknown);
        p.set_models(&alex.id, ModelsState::Loaded(vec!["gpt-5.5".into()]));
        assert_eq!(
            row_labels(&p),
            vec!["gpt-5.5 · chatgpt · alex@bristol.ac.uk"]
        );
        // The bare service name still matches search.
        for c in "chatgpt".chars() {
            p.key(KeyCode::Char(c));
        }
        assert_eq!(row_labels(&p).len(), 1);
    }

    /// A key-bearing provider has no login to name, so its row is the model
    /// and the service alone — it never claims a handle it does not have.
    #[test]
    fn flat_rate_provider_rows_are_named_by_their_service_alone() {
        let go = account("opencode-go");
        let mut p = Picker::new(vec![go.clone()], &Tuning::default(), caps_unknown);
        p.set_models(&go.id, ModelsState::Loaded(vec!["glm-5.2".into()]));
        assert_eq!(row_labels(&p), vec!["glm-5.2 · opencode-go"]);
    }

    /// Two `ChatGPT` accounts signed in under the same email are two rows, not
    /// one collapsed into the other — the bug this plan exists to kill.
    #[test]
    fn two_accounts_on_one_email_draw_two_distinguishable_rows() {
        let personal = login("alex@bristol.ac.uk", "acct-1");
        let work = login("alex@bristol.ac.uk (Acme Ltd)", "acct-2");
        let mut p = Picker::new(
            vec![personal.clone(), work.clone()],
            &Tuning::default(),
            caps_unknown,
        );
        p.set_models(&personal.id, ModelsState::Loaded(vec!["gpt-5.5".into()]));
        p.set_models(&work.id, ModelsState::Loaded(vec!["gpt-5.5".into()]));

        let rows = row_labels(&p);
        assert_eq!(rows.len(), 2);
        assert_ne!(
            rows[0], rows[1],
            "two accounts on one email draw distinguishable rows"
        );

        for c in "acme".chars() {
            p.key(KeyCode::Char(c));
        }
        assert_eq!(
            row_labels(&p).len(),
            1,
            "acme narrows to the one account it names"
        );
    }

    #[test]
    fn query_filters_substring_and_appends_manual_row() {
        let mut p = loaded_picker();
        for c in "haiku".chars() {
            p.key(KeyCode::Char(c));
        }
        let rows = p.rows();
        // One model match plus the synthetic manual row.
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0], Row::Model(_, m) if m == "claude-haiku-4"));
        assert!(matches!(&rows[1], Row::Manual(q) if q == "haiku"));
    }

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
            Row::Model(account, _) if account.service.name.as_str() == "deepseek"
        ));
    }

    #[test]
    fn enter_selects_highlighted_model() {
        let mut p = loaded_picker();
        // To the second row, anthropic / claude-haiku-4.
        p.key(KeyCode::Down);
        match p.key(KeyCode::Enter) {
            PickAction::Selected(account, m, _, _)
                if account.service.name.as_str() == "anthropic" =>
            {
                assert_eq!(m, "claude-haiku-4");
            }
            _ => panic!("expected Selected(anthropic, claude-haiku-4)"),
        }
    }

    /// A declared service lists and selects exactly like a built-in one.
    #[test]
    fn declared_provider_lists_and_selects() {
        let llama = declared("local-llama");
        let mut p = Picker::new(vec![llama.clone()], &Tuning::default(), caps_unknown);
        p.set_models(&llama.id, ModelsState::Loaded(vec!["llama-3".into()]));
        let rows = p.rows();
        assert!(matches!(&rows[0], Row::Model(account, m) if account == &llama && m == "llama-3"));
        match p.key(KeyCode::Enter) {
            PickAction::Selected(account, m, _, _) => {
                assert_eq!(account, llama);
                assert_eq!(m, "llama-3");
            }
            _ => panic!("expected Selected(local-llama, llama-3)"),
        }
    }

    #[test]
    fn enter_on_manual_row_yields_query() {
        let mut p = loaded_picker();
        for c in "claude-future-99".chars() {
            p.key(KeyCode::Char(c));
        }
        // Nothing else matches, so the manual row sits selected at index 0.
        match p.key(KeyCode::Enter) {
            PickAction::Manual(q, _) => assert_eq!(q, "claude-future-99"),
            _ => panic!("expected Manual(claude-future-99)"),
        }
    }

    /// These models do not route, so the cycle skips the provider control:
    /// Search → Effort → Temperature → `TopP` → Search.
    #[test]
    fn tab_cycles_focus_and_arrows_drive_the_focused_control() {
        let mut p = loaded_picker();
        assert_eq!(p.focus, Focus::Search);
        p.key(KeyCode::Tab);
        assert_eq!(p.focus, Focus::Effort);
        // Up the ladder twice: auto → zero → low.
        p.key(KeyCode::Right);
        p.key(KeyCode::Right);
        assert_eq!(EFFORT_LADDER[p.effort_idx].0, "low");

        p.key(KeyCode::Tab);
        assert_eq!(p.focus, Focus::Temperature);
        // From auto, one step right reaches 0.0, another 0.1.
        p.key(KeyCode::Right);
        p.key(KeyCode::Right);
        assert_eq!(p.temperature, Some(0.1));

        p.key(KeyCode::Tab);
        assert_eq!(p.focus, Focus::TopP);
        // From auto, one step right reaches 0.0, another 0.05.
        p.key(KeyCode::Right);
        p.key(KeyCode::Right);
        assert_eq!(p.top_p, Some(0.05));

        p.key(KeyCode::Tab);
        assert_eq!(p.focus, Focus::Search);
    }

    /// The model filter stays reachable from any field.
    #[test]
    fn typing_refocuses_search() {
        let mut p = loaded_picker();
        p.key(KeyCode::Tab); // Effort
        p.key(KeyCode::Char('o'));
        assert_eq!(p.focus, Focus::Search);
        assert_eq!(p.query, "o");
    }

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

    #[test]
    fn top_p_steps_clamps_and_floors_to_auto() {
        let mut p = loaded_picker();
        p.key(KeyCode::Tab); // Effort
        p.key(KeyCode::Tab); // Temperature
        p.key(KeyCode::Tab); // TopP
        assert_eq!(p.top_p, None);
        // Three steps up: auto → 0.0 → 0.05 → 0.1.
        p.key(KeyCode::Right);
        p.key(KeyCode::Right);
        p.key(KeyCode::Right);
        assert_eq!(p.top_p, Some(0.1));
        // Down past zero returns to auto.
        p.key(KeyCode::Left);
        p.key(KeyCode::Left);
        assert_eq!(p.top_p, Some(0.0));
        p.key(KeyCode::Left);
        assert_eq!(p.top_p, None);
    }

    #[test]
    fn selection_carries_the_live_tuning() {
        let mut p = loaded_picker();
        p.key(KeyCode::Tab); // Effort
        p.key(KeyCode::Right); // auto → zero
        p.key(KeyCode::Right); // zero → low
        p.key(KeyCode::Right); // low → med
        p.key(KeyCode::Right); // med → high
        p.key(KeyCode::Tab); // Temperature
        p.key(KeyCode::Right); // auto → 0.0
        p.key(KeyCode::Right); // 0.0 → 0.1
        p.key(KeyCode::Tab); // TopP
        p.key(KeyCode::Right); // auto → 0.0
        p.key(KeyCode::Right); // 0.0 → 0.05
        match p.key(KeyCode::Enter) {
            PickAction::Selected(_, _, tuning, _) => {
                assert_eq!(
                    tuning.effort.as_ref().map(ReasoningEffort::variant_name),
                    Some("high")
                );
                assert_eq!(tuning.temperature, Some(0.1));
                assert_eq!(tuning.top_p, Some(0.05));
            }
            _ => panic!("expected Selected with tuning"),
        }
    }

    #[test]
    fn opens_seeded_from_initial_tuning() {
        let p = Picker::new(
            vec![account("anthropic")],
            &Tuning {
                effort: Some(ReasoningEffort::Medium),
                temperature: Some(0.5),
                top_p: Some(0.9),
            },
            caps_unknown,
        );
        assert_eq!(EFFORT_LADDER[p.effort_idx].0, "med");
        assert_eq!(p.temperature, Some(0.5));
        assert_eq!(p.top_p, Some(0.9));
    }

    /// A catalog where `chat-only` admits `temperature` but not `reasoning`.
    fn caps_split(model: &str) -> crate::provider::pricing::ModelCaps {
        let supported_parameters = if model == "chat-only" {
            vec!["temperature".to_string()]
        } else {
            vec!["reasoning".to_string(), "temperature".to_string()]
        };
        crate::provider::pricing::ModelCaps {
            supported_parameters,
            ..Default::default()
        }
    }

    /// Effort is masked out on the model that does not admit it and its arrows
    /// go dead, yet the rung is still there when a reasoning model returns.
    #[test]
    fn unsupported_effort_is_masked_and_remembered() {
        let anthropic = account("anthropic");
        let mut p = Picker::new(vec![anthropic.clone()], &Tuning::default(), caps_split);
        p.set_models(
            &anthropic.id,
            ModelsState::Loaded(vec!["reasoner".into(), "chat-only".into()]),
        );

        // On the reasoning-capable row, set effort=high and temp=0.1.
        p.key(KeyCode::Tab); // Effort
        for _ in 0..4 {
            p.key(KeyCode::Right); // auto → zero → low → med → high
        }
        p.key(KeyCode::Tab); // Temperature
        p.key(KeyCode::Right); // auto → 0.0
        p.key(KeyCode::Right); // 0.0 → 0.1
        let live = p.tuning(&p.rows());
        assert_eq!(
            live.effort.as_ref().map(ReasoningEffort::variant_name),
            Some("high")
        );
        assert_eq!(live.temperature, Some(0.1));

        // Highlight the chat-only model: effort masked out, temperature kept.
        p.key(KeyCode::Tab); // TopP
        p.key(KeyCode::Tab); // Search
        p.key(KeyCode::Down); // → chat-only
        let masked = p.tuning(&p.rows());
        assert!(masked.effort.is_none(), "reasoning masked for chat-only");
        assert_eq!(masked.temperature, Some(0.1));

        // Its effort arrows are inert.
        p.key(KeyCode::Tab); // Effort
        p.key(KeyCode::Left);
        assert_eq!(EFFORT_LADDER[p.effort_idx].0, "high", "rung unchanged");

        // Back on the reasoning model the setting returns.
        p.key(KeyCode::Tab); // Temperature
        p.key(KeyCode::Tab); // TopP
        p.key(KeyCode::Tab); // Search
        p.key(KeyCode::Up); // → reasoner
        assert_eq!(
            p.tuning(&p.rows())
                .effort
                .as_ref()
                .map(ReasoningEffort::variant_name),
            Some("high")
        );
    }

    /// Cycling clamps at both ends rather than wrapping, never moves the
    /// highlighted model, and the chosen slug rides Enter.
    #[test]
    fn provider_cycles_serving_endpoints_without_moving_the_model() {
        let mut p = openrouter_picker();
        p.key(KeyCode::Down); // highlight deepseek/deepseek-chat
        let model = "deepseek/deepseek-chat";
        assert_eq!(p.highlighted_model(&p.rows()).as_deref(), Some(model));
        p.set_endpoints(
            model,
            EndpointsState::Loaded(vec![
                endpoint("DeepInfra", "deepinfra"),
                endpoint("Novita", "novita"),
            ]),
        );

        p.key(KeyCode::Tab); // Search → Provider
        assert_eq!(p.focus, Focus::Provider);

        p.key(KeyCode::Right);
        assert_eq!(p.active_route(&p.rows()), Some("deepinfra"));
        assert_eq!(
            p.highlighted_model(&p.rows()).as_deref(),
            Some(model),
            "the highlighted model never moves when picking a provider"
        );
        p.key(KeyCode::Right);
        assert_eq!(p.active_route(&p.rows()), Some("novita"));
        p.key(KeyCode::Right);
        assert_eq!(p.active_route(&p.rows()), Some("novita"));
        p.key(KeyCode::Left);
        assert_eq!(p.active_route(&p.rows()), Some("deepinfra"));
        p.key(KeyCode::Left);
        assert_eq!(p.active_route(&p.rows()), None);

        p.key(KeyCode::Right); // auto → deepinfra
        match p.key(KeyCode::Enter) {
            PickAction::Selected(_, m, _, route) => {
                assert_eq!(m, model);
                assert_eq!(route.as_deref(), Some("deepinfra"));
            }
            _ => panic!("expected the highlighted model carrying its route"),
        }
    }

    /// Moving the highlight off a route's model deactivates it, and coming back
    /// restores it — the choice never rides a model it was not made for.
    #[test]
    fn route_is_inactive_off_its_model_and_returns_on_it() {
        let mut p = openrouter_picker();
        p.key(KeyCode::Down); // deepseek/deepseek-chat
        let model = "deepseek/deepseek-chat";
        p.set_endpoints(
            model,
            EndpointsState::Loaded(vec![endpoint("DeepInfra", "deepinfra")]),
        );
        p.key(KeyCode::Tab); // Provider
        p.key(KeyCode::Right); // choose deepinfra
        assert_eq!(p.active_route(&p.rows()), Some("deepinfra"));

        for _ in 0..4 {
            p.key(KeyCode::Tab); // Provider → Effort → Temperature → TopP → Search
        }
        assert_eq!(p.focus, Focus::Search);
        p.key(KeyCode::Down); // deepseek/deepseek-r1
        assert_ne!(p.highlighted_model(&p.rows()).as_deref(), Some(model));
        assert_eq!(
            p.active_route(&p.rows()),
            None,
            "the route does not ride another model"
        );

        p.key(KeyCode::Up);
        assert_eq!(p.highlighted_model(&p.rows()).as_deref(), Some(model));
        assert_eq!(p.active_route(&p.rows()), Some("deepinfra"));
    }

    /// A model that does not route skips the provider control, and nothing
    /// requests endpoints for it.
    #[test]
    fn provider_control_skipped_for_non_openrouter_model() {
        let mut p = loaded_picker();
        assert!(p.highlighted_model(&p.rows()).is_some());
        p.key(KeyCode::Tab);
        assert_eq!(p.focus, Focus::Effort);
        assert!(p.focused_or_model_needing_endpoints().is_none());
    }

    /// Focusing the provider control is the cue to fetch, and once the driver
    /// seeds the in-flight state the fetch is not requested again.
    #[test]
    fn focusing_provider_requests_endpoints_once() {
        let mut p = openrouter_picker(); // first row: anthropic/claude-3
        assert!(
            p.focused_or_model_needing_endpoints().is_none(),
            "nothing requested before the control is focused"
        );
        p.key(KeyCode::Tab); // Search → Provider
        assert_eq!(p.focus, Focus::Provider);
        assert_eq!(
            p.focused_or_model_needing_endpoints().as_deref(),
            Some("anthropic/claude-3")
        );
        p.set_endpoints("anthropic/claude-3", EndpointsState::Loading);
        assert!(
            p.focused_or_model_needing_endpoints().is_none(),
            "seeding Loading dedups the fetch"
        );
    }
}
