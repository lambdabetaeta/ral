//! Model picker overlay orchestration — the `/model` command handler and its
//! event loop.

use std::fmt::Write;
use std::sync::Arc;
use std::time::Duration;

use ratatui::crossterm::event::{Event as CtEvent, KeyEventKind, poll as ct_poll, read as ct_read};

use crate::bus::Kind;
use crate::credential::CredentialStore;
use crate::models::{LiveSource, ModelCatalog, ModelSource};
use crate::provider::{self, Provider};
use crate::state;

use super::picker::{self, Picker};
use super::render::draw;
use super::tui_loop::{CommandCtx, KeyAction, KeyMode, Tui, key_action};

type FetchRx = std::sync::mpsc::Receiver<(provider::ProviderId, Result<Vec<String>, String>)>;

/// Channel carrying `(model, fetched serving providers or failure)` from the
/// per-model background endpoint-fetch threads back to the picker loop.
type EndpointRx =
    std::sync::mpsc::Receiver<(String, Result<Vec<crate::models::ProviderEndpoint>, String>)>;

pub(super) fn pick_model(tui: &mut Tui, ctx: &mut CommandCtx<'_>) {
    let store = ctx.store;
    let available = store.available();
    // Each plan-backed provider's flavour, for the picker's labels: a ChatGPT
    // login (the OAuth credential) reads as the ChatGPT plan, an otherwise-
    // metered provider whose `ProviderId` declares a flat rate (opencode Go) as
    // the generic subscription. A provider absent from the map is metered.
    let subscription = available
        .iter()
        .filter_map(|id| {
            let kind = if store.is_subscription(id) {
                crate::provider::Subscription::ChatGpt
            } else if id.flat_rate() {
                crate::provider::Subscription::FlatRate
            } else {
                return None;
            };
            Some((id.clone(), kind))
        })
        .collect();
    // Seed the tuning controls from the focused provider's live values, so the
    // overlay opens showing the effort/temperature currently in force (a
    // settled agent with no live handle falls back to the defaults).
    let initial_tuning = ctx
        .agents
        .provider(tui.app.tabs.focused())
        .map(|p| p.current().tuning().clone())
        .unwrap_or_default();
    let mut picker = Picker::new(
        available,
        subscription,
        &initial_tuning,
        crate::pricing::caps_or_default,
    );
    // Seed each provider from the catalog's cache instantly; spawn a background
    // fetch for the rest so the UI shows "loading…" rather than freezing on the
    // network. API-key providers list through genai; ChatGPT subscriptions list
    // through the Codex backend. Both paths share the same cache and result
    // channel, so provider kind does not leak into the picker.
    let mut rx = None;
    let to_fetch: Vec<_> = picker
        .loading_providers()
        .into_iter()
        .filter(|id| {
            match ctx.catalog.cached(id) {
                Some(models) => {
                    picker.set_models(id, picker::ModelsState::Loaded(models));
                    false
                }
                None => true,
            }
        })
        .collect();
    if !to_fetch.is_empty() {
        let (tx, recv) = std::sync::mpsc::channel();
        for id in to_fetch {
            let source = ctx.catalog.source().clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let result = source.list(&id);
                let _ = tx.send((id, result));
            });
        }
        rx = Some(recv);
    }
    tui.app.picker = Some(picker);
    let outcome = drive_picker(tui, store, ctx.catalog, rx.as_ref());
    tui.app.picker = None;
    if let Some((id, model, tuning, route)) = outcome {
        apply_model_switch(tui, ctx, &id, &model, &tuning, route.as_ref());
    }
}

/// Poll keys and background-fetch results until the picker resolves.  Returns
/// the chosen `(provider, model, tuning, route)`, or `None` on cancel. The
/// `route` is the chosen `OpenRouter` serving-provider slug (`None` for auto).
fn drive_picker(
    tui: &mut Tui,
    store: &CredentialStore,
    catalog: &mut ModelCatalog<LiveSource>,
    rx: Option<&FetchRx>,
) -> Option<(
    provider::ProviderId,
    String,
    provider::Tuning,
    Option<String>,
)> {
    // The serving-provider fetch is intent-driven and spawned from inside the
    // loop, so its channel lives for the loop's whole duration (unlike the
    // model-list `rx`, whose fetches are all kicked off before the loop). The
    // sender's payload type follows from the receiver alias.
    let (endpoint_tx, endpoint_rx): (_, EndpointRx) = std::sync::mpsc::channel();
    loop {
        // Fold any landed fetch results into the picker (and the catalog's
        // caches), on this thread, so the disk write stays single-threaded.
        if let Some(rx) = rx {
            while let Ok((id, result)) = rx.try_recv() {
                let state = match result {
                    Ok(models) => {
                        catalog.record(&id, models.clone());
                        picker::ModelsState::Loaded(models)
                    }
                    Err(reason) => picker::ModelsState::Failed(reason),
                };
                if let Some(p) = tui.app.picker_mut() {
                    p.set_models(&id, state);
                }
            }
        }
        // Fold any landed serving-provider results the same way.
        while let Ok((model, result)) = endpoint_rx.try_recv() {
            let state = match result {
                Ok(endpoints) => {
                    catalog.record_endpoints(&model, endpoints.clone());
                    picker::EndpointsState::Loaded(endpoints)
                }
                Err(reason) => picker::EndpointsState::Failed(reason),
            };
            if let Some(p) = tui.app.picker_mut() {
                p.set_endpoints(&model, state);
            }
        }
        // When the provider control is focused on an OpenRouter model whose
        // serving providers we have not fetched, seed it from the catalog memo
        // or spawn a background fetch. Seeding the state first dedups: the next
        // poll no longer reports the model as needing a fetch.
        let needed = tui
            .app
            .picker_mut()
            .and_then(|p| p.focused_or_model_needing_endpoints());
        if let Some(model) = needed {
            if let Some(endpoints) = catalog.cached_endpoints(&model) {
                if let Some(p) = tui.app.picker_mut() {
                    p.set_endpoints(&model, picker::EndpointsState::Loaded(endpoints));
                }
            } else {
                if let Some(p) = tui.app.picker_mut() {
                    p.set_endpoints(&model, picker::EndpointsState::Loading);
                }
                let source = catalog.source().clone();
                let endpoint_tx = endpoint_tx.clone();
                std::thread::spawn(move || {
                    let result = source.endpoints(&model);
                    let _ = endpoint_tx.send((model.clone(), result));
                });
            }
        }
        if draw(&mut tui.app, tui.guard.term()).is_err() {
            return None;
        }
        if !ct_poll(Duration::from_millis(100)).unwrap_or(false) {
            continue;
        }
        let Ok(CtEvent::Key(k)) = ct_read() else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        if key_action(KeyMode::Overlay, &k, false) == KeyAction::Cancel {
            return None;
        }
        let action = tui.app.picker_mut()?.key(k.code);
        match action {
            picker::PickAction::None => {}
            picker::PickAction::Cancelled => return None,
            picker::PickAction::Selected(id, model, tuning, route) => {
                return Some((id, model, tuning, route));
            }
            picker::PickAction::Manual(query, tuning, route) => {
                let available = store.available();
                match crate::models::resolve_model_provider(&query, &available, catalog) {
                    Ok(id) => return Some((id, query, tuning, route)),
                    Err(e) => {
                        let root = tui.app.tabs.root();
                        tui.app.push_error(root, &e);
                    }
                }
            }
        }
    }
}

/// Rebuild the provider for the chosen `kind` + `model` over the same
/// transcript and swap it into the **focused agent's** own provider handle
/// (its next turn reads it), persist the selection to the project state dir,
/// and update the live status bar. A persistence failure is noted but does not
/// undo the in-memory switch.  A focused agent that settled between the picker
/// opening and the selection has no handle to swap, so the switch is dropped.
///
/// A model switch is a *real* operational event, so it goes through `emit` —
/// the UI-thread recording emitter carrying the trunk's transcript — as a
/// [`Kind::SystemNote`].  It records in the trace and draws through the normal
/// bus path, like a worker-raised note; the UI never fabricates an `Event` for
/// it.  Its own failures, by contrast, are view chrome ([`App::push_error`]).
fn apply_model_switch(
    tui: &mut Tui,
    ctx: &CommandCtx<'_>,
    provider_id: &provider::ProviderId,
    model: &str,
    tuning: &provider::Tuning,
    route: Option<&String>,
) {
    let store = ctx.store;
    let info = ctx.info;
    let emit = ctx.emit;
    let id = tui.app.tabs.root();
    let Some(cred) = store.get(provider_id).cloned() else {
        tui.app.push_error(
            id,
            &format!("{} has no resolved credential", provider_id.label()),
        );
        return;
    };
    // Swap the *focused* agent's handle; if it has settled, there is nothing to
    // swap and the selection is dropped (the user can reopen on a live tab).
    let focused = tui.app.tabs.focused();
    let Some(provider) = ctx.agents.provider(focused) else {
        tui.app
            .push_error(id, "the focused agent is no longer live");
        return;
    };
    // Capture the current provider's max_tokens_override before we swap,
    // so the new provider carries the same user override.
    let current_override = provider.current().max_tokens_override();
    let engine = ctx.engine.clone();
    let new_provider = Arc::new(Provider::build(
        engine,
        provider_id,
        model.to_string(),
        &cred,
        current_override,
        tuning.clone(),
        route.cloned(),
    ));
    let label = provider_id.label();
    let status_provider = crate::provider::provider_label(new_provider.subscription(), label);
    provider.swap(new_provider);
    tui.app
        .update_live_model(&provider.current(), &status_provider);
    let state_dir = crate::bootstrap::project_dir(info.cwd);
    if let Err(e) = state::save(
        &state_dir,
        &state::State::new(provider_id, model, tuning, route.map(String::as_str)),
    ) {
        tui.app
            .push_error(id, &format!("could not persist selection: {e}"));
    }
    emit.emit(Kind::SystemNote(format!(
        "[Switched to {label} {model}{}]",
        tuning_suffix(tuning, route.map(String::as_str))
    )));
}

/// A human-readable suffix for the switch note describing any non-default
/// tuning and route, e.g. ` · effort high · temp 0.7 · via deepinfra`. Empty
/// when both knobs are auto and no route is pinned.
fn tuning_suffix(tuning: &provider::Tuning, route: Option<&str>) -> String {
    let mut parts = String::new();
    if let Some(effort) = &tuning.effort {
        let _ = write!(parts, " · effort {}", effort.variant_name());
    }
    if let Some(temperature) = tuning.temperature {
        let _ = write!(parts, " · temp {temperature:.1}");
    }
    if let Some(slug) = route {
        let _ = write!(parts, " · via {slug}");
    }
    parts
}
