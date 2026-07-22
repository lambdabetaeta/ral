//! Model picker overlay orchestration — the `/model` command handler and its
//! event loop.

use std::fmt::Write;
use std::sync::Arc;

use crate::bus::Kind;
use crate::provider::credential::CredentialStore;
use crate::provider::models::{LiveSource, ModelCatalog, ModelSource, ProviderEndpoint};
use crate::provider::state;
use crate::provider::{self, Provider};

use super::app::Overlay;
use super::picker::{self, Picker};
use super::tui_loop::{CommandCtx, OverlayTick, Tui, overlay_tick};

type FetchRx = std::sync::mpsc::Receiver<(provider::ProviderId, Result<Vec<String>, String>)>;

/// Channel carrying `(model, fetched serving providers or failure)` from the
/// per-model background endpoint-fetch threads back to the picker loop.
type EndpointRx = std::sync::mpsc::Receiver<(String, Result<Vec<ProviderEndpoint>, String>)>;

pub(super) fn pick_model(tui: &mut Tui, ctx: &mut CommandCtx<'_>) {
    // A shared reborrow: `pick_model` only reads the store (`/login`'s
    // `add_oauth` is the one site that needs it mutably), so `ctx` stays
    // whole for `apply_model_switch`'s later reborrow.
    let store = &*ctx.store;
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
        crate::provider::pricing::caps_or_default,
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
        .filter(|id| match ctx.catalog.cached(id) {
            Some(models) => {
                picker.set_models(id, picker::ModelsState::Loaded(models));
                false
            }
            None => true,
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
    tui.app.overlay = Some(Overlay::Picker(picker));
    let outcome = drive_picker(tui, store, ctx.catalog, rx.as_ref());
    tui.app.overlay = None;
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
            fold_fetch(
                rx,
                |id, models| catalog.record(id, models),
                |id, state| {
                    if let Some(p) = tui.app.picker_mut() {
                        p.set_models(id, state);
                    }
                },
            );
        }
        // Fold any landed serving-provider results the same way.
        fold_fetch(
            &endpoint_rx,
            |model, endpoints| catalog.record_endpoints(model, endpoints),
            |model, state| {
                if let Some(p) = tui.app.picker_mut() {
                    p.set_endpoints(model, state);
                }
            },
        );
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
        match overlay_tick(tui) {
            OverlayTick::TerminalLost | OverlayTick::Cancel => return None,
            OverlayTick::Idle => {}
            OverlayTick::Key(code) => {
                let action = tui.app.picker_mut()?.key(code);
                match action {
                    picker::PickAction::None => {}
                    picker::PickAction::Selected(id, model, tuning, route) => {
                        return Some((id, model, tuning, route));
                    }
                    picker::PickAction::Manual(query, tuning) => {
                        let available = store.available();
                        match crate::provider::models::resolve_model_provider(
                            &query, &available, catalog,
                        ) {
                            Ok(id) => return Some((id, query, tuning, None)),
                            Err(e) => {
                                let root = tui.app.tabs.root();
                                tui.app.push_error(root, &e);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Fold every fetch result currently queued on `rx` into the picker via
/// `set`, recording each success into the catalog via `record` first (so the
/// on-disk cache stays authoritative for the next open). Shared by the
/// model-list and serving-provider pumps in [`drive_picker`] — they differ
/// only in their key, payload, and where each callback writes.
fn fold_fetch<K, T: Clone>(
    rx: &std::sync::mpsc::Receiver<(K, Result<T, String>)>,
    mut record: impl FnMut(&K, T),
    mut set: impl FnMut(&K, picker::FetchState<T>),
) {
    while let Ok((key, result)) = rx.try_recv() {
        let state = match result {
            Ok(value) => {
                record(&key, value.clone());
                picker::FetchState::Loaded(value)
            }
            Err(reason) => picker::FetchState::Failed(reason),
        };
        set(&key, state);
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
    let store = &*ctx.store;
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
    provider.swap(new_provider);
    tui.app.update_live_model(&provider.current());
    let state_dir = crate::bootstrap::EXARCH.project_dir(info.cwd);
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
/// tuning and route, e.g. ` · effort high · temp 0.7 · top_p 0.9 · via
/// deepinfra`. Empty when every knob is auto and no route is pinned.
fn tuning_suffix(tuning: &provider::Tuning, route: Option<&str>) -> String {
    let mut parts = String::new();
    if let Some(effort) = &tuning.effort {
        let _ = write!(parts, " · effort {}", effort.variant_name());
    }
    if let Some(temperature) = tuning.temperature {
        let _ = write!(parts, " · temp {temperature:.1}");
    }
    if let Some(top_p) = tuning.top_p {
        let _ = write!(parts, " · top_p {top_p:.2}");
    }
    if let Some(slug) = route {
        let _ = write!(parts, " · via {slug}");
    }
    parts
}
