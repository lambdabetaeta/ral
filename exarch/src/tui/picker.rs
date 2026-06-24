//! The `/model` searchable picker.
//!
//! The TUI is a flat stack of strips with no overlay layer; the picker
//! honours that — a bordered pane that takes over the prompt region while
//! open, not a floating modal. It lists the available providers' models as
//! `provider / model`, filtered as the user types, with the selected row in
//! `Modifier::REVERSED`; Up/Down move, Enter switches, Esc dismisses. It is
//! modal in *behaviour* (an early-return guard in [`super::App::key`]) but
//! flat in *rendering*.
//!
//! The picker is a pure display+input component: it holds the query, the
//! per-provider model lists as they arrive, and the selection. Fetching
//! lives in the REPL (which owns the credential-backed catalog and the
//! network seam); the REPL feeds results in via [`Picker::set_models`], so
//! a provider's list shows "loading…" until its background fetch lands.

use super::{CYAN, SLATE};
use crate::oauth::Subscription;
use crate::provider::ProviderId;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
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
/// rebuilding the provider, persisting the selection, and updating the status
/// bar, or closing the picker.
pub enum PickAction {
    /// Keep the picker open; redraw.
    None,
    /// A listed `provider / model` row was chosen.
    Selected(ProviderId, String),
    /// Enter on the synthetic manual row: take the raw query as a model
    /// name and let the REPL resolve its provider (the listing-or-name
    /// fallback) — the escape hatch when a fetch failed or the wanted model
    /// is not listed.
    Manual(String),
    /// Esc — dismiss without switching.
    Cancelled,
}

/// A row in the rendered list: either a listed model or the synthetic
/// manual-entry row.
enum Row {
    Model(ProviderId, String),
    Manual(String),
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
}

/// Rows of the list visible at once (the strip is this tall plus borders
/// and the query line). Kept small so the picker is a strip, not a takeover
/// of the whole screen.
const VISIBLE_ROWS: u16 = 10;

impl Picker {
    /// Open over `providers`, all initially loading until the REPL feeds
    /// cached or fetched lists. `subscription` maps each plan-backed provider
    /// to its flavour, whose rows read as the subscription.
    pub fn new(
        providers: Vec<ProviderId>,
        subscription: BTreeMap<ProviderId, Subscription>,
    ) -> Self {
        let models = providers
            .iter()
            .map(|id| (id.clone(), ModelsState::Loading))
            .collect();
        Self {
            query: String::new(),
            providers,
            subscription,
            models,
            selected: 0,
        }
    }

    /// The display label for `kind`'s rows: the plain provider name, or
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

    /// Handle one key press. Typing filters; Up/Down move; Enter selects
    /// the highlighted row; Esc cancels.
    pub fn key(&mut self, code: ratatui::crossterm::event::KeyCode) -> PickAction {
        use ratatui::crossterm::event::KeyCode;
        match code {
            KeyCode::Esc => PickAction::Cancelled,
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                PickAction::None
            }
            KeyCode::Down => {
                let n = self.rows().len();
                if n > 0 {
                    self.selected = (self.selected + 1).min(n - 1);
                }
                PickAction::None
            }
            KeyCode::Enter => match self.rows().into_iter().nth(self.selected) {
                Some(Row::Model(kind, model)) => PickAction::Selected(kind, model),
                Some(Row::Manual(query)) => PickAction::Manual(query),
                None => PickAction::None,
            },
            KeyCode::Backspace => {
                self.query.pop();
                self.selected = 0;
                PickAction::None
            }
            KeyCode::Char(c) => {
                self.query.push(c);
                self.selected = 0;
                PickAction::None
            }
            _ => PickAction::None,
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

    /// Total height the picker wants in the prompt region: a query line, a
    /// status line, the visible rows, one note per failed provider, and the
    /// rounded border — clamped to the available height.  The failed-provider
    /// notes are budgeted explicitly so the explanation (and the manual-entry
    /// hint it carries) is never clipped off the bottom by [`Self::render`].
    pub fn height(&self, max_h: u16) -> u16 {
        let failed = self
            .models
            .values()
            .filter(|s| matches!(s, ModelsState::Failed(_)))
            .count() as u16;
        // query + status + rows + failed-provider notes + top/bottom border
        let want = 2 + VISIBLE_ROWS + failed + 2;
        want.min(max_h.max(3))
    }

    /// Render the strip: a bordered pane with a query line, a loading/empty
    /// status line, and the scrolled list with the selected row reversed.
    pub fn render(&self, f: &mut Frame, area: Rect) {
        let rows = self.rows();
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(vec![
            Span::styled("search ", Style::default().fg(SLATE)),
            Span::styled(self.query.clone(), Style::default().fg(CYAN)),
            Span::styled("▏", Style::default().fg(CYAN)),
        ]));
        let status = if self.is_loading() {
            "loading…  ↑↓ move · Enter switch · Esc cancel".to_string()
        } else {
            format!(
                "{} match{}  ↑↓ move · Enter switch · Esc cancel",
                rows.len(),
                if rows.len() == 1 { "" } else { "es" }
            )
        };
        lines.push(Line::from(Span::styled(
            status,
            Style::default().fg(SLATE).add_modifier(Modifier::DIM),
        )));

        // Scroll the window so the selected row stays visible.
        let window = VISIBLE_ROWS as usize;
        let start = self.selected.saturating_sub(window.saturating_sub(1));
        for (i, row) in rows.iter().enumerate().skip(start).take(window) {
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
            lines.push(Line::from(Span::styled(text, style)));
        }

        // Note any provider whose fetch failed, with its reason, so the
        // absent models are explained and the manual-entry fallback is
        // obvious. Informational, not selectable.
        for id in &self.providers {
            if let Some(ModelsState::Failed(reason)) = self.models.get(id) {
                lines.push(Line::from(Span::styled(
                    format!(
                        "{} — fetch failed: {reason} (type a model to enter manually)",
                        id.label()
                    ),
                    Style::default()
                        .fg(SLATE)
                        .add_modifier(Modifier::DIM | Modifier::ITALIC),
                )));
            }
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(CYAN))
            .padding(Padding::horizontal(1));
        f.render_widget(Paragraph::new(lines).block(block), area);
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

    /// Enter on a listed row yields `Selected(provider, model)`.
    #[test]
    fn enter_selects_highlighted_model() {
        let mut p = loaded_picker();
        // Move to the second row (anthropic / claude-haiku-4).
        p.key(KeyCode::Down);
        match p.key(KeyCode::Enter) {
            PickAction::Selected(id, m) if id.famous() == Some(ProviderKind::Anthropic) => {
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
        let mut p = Picker::new(vec![llama.clone()], BTreeMap::new());
        p.set_models(&llama, ModelsState::Loaded(vec!["llama-3".into()]));
        let rows = p.rows();
        assert!(matches!(&rows[0], Row::Model(id, m) if id == &llama && m == "llama-3"));
        match p.key(KeyCode::Enter) {
            PickAction::Selected(id, m) => {
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
            PickAction::Manual(q) => assert_eq!(q, "claude-future-99"),
            _ => panic!("expected Manual(claude-future-99)"),
        }
    }

    /// Esc cancels.
    #[test]
    fn esc_cancels() {
        let mut p = loaded_picker();
        assert!(matches!(p.key(KeyCode::Esc), PickAction::Cancelled));
    }

    /// Each failed provider adds a note row to the picker's wanted height, so
    /// `render`'s failure explanations are not clipped off the bottom.
    #[test]
    fn height_reserves_a_row_per_failed_provider() {
        let mut p = Picker::new(
            vec![fam(ProviderKind::Anthropic), fam(ProviderKind::Deepseek)],
            BTreeMap::new(),
        );
        let base = p.height(u16::MAX);
        p.set_models(
            &fam(ProviderKind::Anthropic),
            ModelsState::Failed("x".into()),
        );
        assert_eq!(p.height(u16::MAX), base + 1);
        p.set_models(
            &fam(ProviderKind::Deepseek),
            ModelsState::Failed("y".into()),
        );
        assert_eq!(p.height(u16::MAX), base + 2);
    }

    /// A still-loading provider reports `is_loading` until its list lands.
    #[test]
    fn loading_until_all_lists_land() {
        let mut p = Picker::new(
            vec![fam(ProviderKind::Anthropic), fam(ProviderKind::Deepseek)],
            BTreeMap::new(),
        );
        assert!(p.is_loading());
        assert_eq!(p.loading_providers().len(), 2);
        p.set_models(
            &fam(ProviderKind::Anthropic),
            ModelsState::Loaded(vec!["m".into()]),
        );
        assert!(p.is_loading());
        p.set_models(
            &fam(ProviderKind::Deepseek),
            ModelsState::Failed("x".into()),
        );
        assert!(!p.is_loading());
    }
}
