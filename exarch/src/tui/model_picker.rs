//! The model picker's orchestration, over a live session (`/model`) or alone
//! on the screen before one exists (a launch with no model to restore).
//!
//! [`Picker`] is display and input only; this module, its one caller, reaches
//! the [`Bureau`] holding the credentials, the model catalog, and the network
//! seam, and turns a [`Pick`] into a live provider swap plus a saved
//! [`state::State`]. [`super::login`] mirrors the split.

use std::io;
use std::time::Duration;

use crossterm::event::{Event as CtEvent, KeyEventKind, poll as ct_poll, read as ct_read};

use crate::provider::identity::{self, Account};
use crate::provider::listing::{Fetches, Listing};
use crate::provider::models::{ModelSource, ProviderEndpoint};
use crate::provider::{Bureau, Tuning, pricing, state};
use crate::record::AgentId;

use super::app::{App, CommandCtx, Input, Overlay, input_of};
use super::picker::{self, Pick, Picker};
use super::terminal::TerminalGuard;

/// An open picker with the listing that fills it and the endpoint fetches in
/// flight.
pub(super) struct PickerSession {
    pub(super) picker: Picker,
    listing: Listing,
    // Spawned from inside `poll`, unlike `listing`, whose fetches are all away
    // before it.
    endpoints: Fetches<String, Vec<ProviderEndpoint>>,
}

impl PickerSession {
    /// Fold landed fetches into the picker and start the one it needs next.
    ///
    /// The catalog is taken per fold rather than held across polls: the
    /// listing's own fetches run on their own threads, so no frame holds a
    /// bureau lock while one of them is out.
    pub(super) fn poll(&mut self, bureau: &Bureau) {
        // The picker's copy is for render; `listing` stays authoritative.
        let woken = bureau
            .with_catalog(|catalog| self.listing.pump(catalog))
            .unwrap_or_default();
        for id in woken {
            if let Some(state) = self.listing.state(&id) {
                self.picker.set_models(&id, state.clone());
            }
        }
        for (model, result) in self.endpoints.landed() {
            let state = match result {
                Ok(list) => {
                    let _ = bureau
                        .with_catalog(|catalog| catalog.record_endpoints(&model, list.clone()));
                    picker::EndpointsState::Loaded(list)
                }
                Err(reason) => picker::EndpointsState::Failed(reason),
            };
            self.picker.set_endpoints(&model, state);
        }
        // Seeding the state is also the dedup: the next poll no longer reports
        // this model as needing a fetch.
        if let Some(model) = self.picker.model_needing_endpoints() {
            let cached = bureau
                .with_catalog(|catalog| catalog.cached_endpoints(&model))
                .flatten();
            if let Some(list) = cached {
                self.picker
                    .set_endpoints(&model, picker::EndpointsState::Loaded(list));
            } else {
                self.picker
                    .set_endpoints(&model, picker::EndpointsState::Loading);
                // The seam is cloned out and the fetch runs on its own thread,
                // so the catalog is free again before the network is touched.
                if let Some(source) = bureau.with_catalog(|catalog| catalog.source().clone()) {
                    self.endpoints
                        .spawn(model.clone(), move || source.endpoints(&model));
                }
            }
        }
    }
}

/// A picker over `accounts` opened on `tuning`, seeded from whatever the
/// catalog already holds, with the listing that fetches the rest. `None` when
/// the bureau is scripted and has no catalog to list from.
fn open(bureau: &Bureau, accounts: Vec<Account>, tuning: &Tuning) -> Option<PickerSession> {
    let ids = accounts.iter().map(|account| account.id.clone()).collect();
    let listing = bureau.with_catalog(|catalog| Listing::open(ids, catalog))?;
    let mut picker = Picker::new(accounts, tuning, pricing::caps_or_default);
    for (id, state) in listing.states() {
        picker.set_models(id, state.clone());
    }
    Some(PickerSession {
        picker,
        listing,
        endpoints: Fetches::new(),
    })
}

pub(super) fn pick_model(app: &mut App, ctx: &CommandCtx<'_>) {
    // Open on the focused agent's live tuning; a settled one falls back to the
    // defaults.
    let tab = app.tabs.focused();
    let tuning = app
        .tabs
        .agent(tab)
        .map(|agent| agent.current_provider().tuning().clone())
        .unwrap_or_default();
    let Some(session) = open(ctx.bureau, ctx.bureau.available(), &tuning) else {
        return;
    };
    app.open(Overlay::Picker(session, tab));
}

/// Cancel closes the picker; a pick switches the focused agent's model and closes it.
pub(super) fn input(
    s: &mut PickerSession,
    tab: AgentId,
    input: &Input,
    app: &mut App,
    ctx: &CommandCtx<'_>,
) -> bool {
    match input {
        Input::Cancel => false,
        Input::Key(code) => match s.picker.key(*code) {
            Some(pick) => {
                apply_model_switch(app, ctx, tab, &pick);
                false
            }
            None => true,
        },
        Input::Save | Input::Paste(_) => true,
    }
}

/// Choose a model before any session exists: the picker alone on `screen`,
/// over `accounts`, opened on `tuning`, and saying `why` when that is news.
///
/// # Errors
/// If the bureau is scripted and so lists nothing, if the user backs out, or if
/// the terminal fails.
pub fn choose(
    screen: &mut TerminalGuard,
    bureau: &Bureau,
    accounts: Vec<Account>,
    tuning: &Tuning,
    why: Option<String>,
) -> Result<Pick, String> {
    let mut session =
        open(bureau, accounts, tuning).ok_or("a scripted session has no models to choose from")?;
    session.picker = session.picker.noting(why);
    let tty = |e: io::Error| e.to_string();
    loop {
        session.poll(bureau);
        screen
            .term()
            .draw(|f| session.picker.render(f, f.area()))
            .map_err(tty)?;
        if !ct_poll(Duration::from_millis(100)).map_err(tty)? {
            continue;
        }
        let CtEvent::Key(k) = ct_read().map_err(tty)? else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match input_of(&k) {
            Input::Cancel => return Err("no model chosen".into()),
            Input::Key(code) => {
                if let Some(pick) = session.picker.key(code) {
                    return Ok(pick);
                }
            }
            Input::Save | Input::Paste(_) => {}
        }
    }
}

/// Rebuild the provider for `pick` and swap it into `tab`'s agent's
/// handle, which its next turn reads; a failed persist leaves that switch
/// standing. The switch reaches the trace as a [`Forensic::ModelChanged`] and
/// the screen as the status bar's live label — never as transcript chatter;
/// its own failures are view chrome.
///
/// [`Forensic::ModelChanged`]: crate::record::Forensic::ModelChanged
fn apply_model_switch(app: &mut App, ctx: &CommandCtx<'_>, tab: AgentId, pick: &Pick) {
    // Every failure below answers one gesture where the user is, so it lands on
    // the focused tab — the persist too, whose file is project-wide but whose
    // message is not.
    let focused = app.tabs.focused();
    let available = ctx.bureau.available();
    // A tab that settled while the picker was open has no handle to swap.
    let Some(agent) = app.tabs.agent(tab) else {
        app.push_error(focused, "the agent you picked a model for has ended");
        return;
    };
    let provider = agent.0.provider.clone();
    // The token override is no part of the selection, so it rides across by hand.
    let built = match ctx.bureau.build(
        pick.choice.clone(),
        &pick.tuning,
        provider.current().max_tokens_override(),
    ) {
        Ok(built) => built,
        Err(e) => {
            app.push_error(focused, &e);
            return;
        }
    };
    let saved = state::State::of(&built, &available);
    let context_window = built.context_window();
    provider.swap(built);
    let persisted = crate::app::EXARCH
        .project(ctx.info.cwd.as_path())
        .map_err(String::from)
        .and_then(|project| state::save(&project, &saved));
    if let Err(e) = persisted {
        app.push_error(focused, &format!("could not persist selection: {e}"));
    }
    if let Some(sb) = app.tabs.scrollback(tab) {
        sb.recorder()
            .emit_or_report(crate::record::Forensic::ModelChanged {
                model: pick.choice.model.clone(),
                context_window,
                label: identity::label(&pick.choice.account, &available),
                service: pick.choice.account.service.name.as_str().to_string(),
                account: pick.choice.account.id.as_str().to_string(),
            });
    }
}
