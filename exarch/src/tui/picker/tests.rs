use super::*;
use crate::provider::ReasoningEffort;
use ratatui::crossterm::event::KeyCode;

/// A stub that knows nothing: an empty `supported_parameters` reads as
/// "supports everything", so every tuning row stays live.
fn caps_unknown(_: &str) -> crate::provider::pricing::ModelCaps {
    crate::provider::pricing::ModelCaps::default()
}

fn loaded_picker() -> Picker {
    let anthropic = Account::built_in("anthropic");
    let deepseek = Account::built_in("deepseek");
    let mut p = Picker::new(
        vec![anthropic.clone(), deepseek.clone()],
        &Tuning::default(),
        caps_unknown,
    );
    p.set_models(
        &anthropic.id,
        ModelsState::Loaded(vec!["model-b".into(), "model-a".into()]),
    );
    p.set_models(&deepseek.id, ModelsState::Loaded(vec!["model-c".into()]));
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
    let openrouter = Account::built_in("openrouter");
    let mut p = Picker::new(vec![openrouter.clone()], &Tuning::default(), caps_unknown);
    p.set_models(
        &openrouter.id,
        ModelsState::Loaded(vec![
            "vendor-a/model-a".into(),
            "vendor-b/model-b".into(),
            "vendor-b/model-c".into(),
            "vendor-c/model-d".into(),
        ]),
    );
    p
}

/// The `model · provider` labels of every listed row, in order.
fn row_labels(p: &Picker) -> Vec<String> {
    p.rows()
        .into_iter()
        .map(|(account, m)| format!("{m} · {}", p.label(&account)))
        .collect()
}

/// Ranking orders every row, empty query included, by the `label / model`
/// line it matched by — so the list reads alphabetically rather than in
/// whatever order the fetches happened to land.
#[test]
fn empty_query_shows_all_loaded_models() {
    let p = loaded_picker();
    assert_eq!(
        row_labels(&p),
        vec![
            "model-a · anthropic",
            "model-b · anthropic",
            "model-c · deepseek",
        ]
    );
}

/// A query's words each narrow the list: `anthropic model` keeps only the
/// rows whose `label / model` line carries both.
#[test]
fn a_two_word_query_narrows_by_both_words() {
    let mut p = loaded_picker();
    for c in "anthropic model".chars() {
        p.key(KeyCode::Char(c));
    }
    assert_eq!(
        row_labels(&p),
        vec!["model-a · anthropic", "model-b · anthropic"]
    );
}

/// A lone `ChatGPT` account has nothing to collide with, so its row keeps
/// its email rather than falling back to the id.
#[test]
fn a_lone_chatgpt_account_row_keeps_its_email() {
    let alex = Account::chatgpt("acct-1", "alex@bristol.ac.uk");
    let mut p = Picker::new(vec![alex.clone()], &Tuning::default(), caps_unknown);
    p.set_models(&alex.id, ModelsState::Loaded(vec!["model-a".into()]));
    assert_eq!(
        row_labels(&p),
        vec!["model-a · chatgpt · alex@bristol.ac.uk"]
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
    let go = Account::built_in("opencode-go");
    let mut p = Picker::new(vec![go.clone()], &Tuning::default(), caps_unknown);
    p.set_models(&go.id, ModelsState::Loaded(vec!["model-a".into()]));
    assert_eq!(row_labels(&p), vec!["model-a · opencode-go"]);
}

/// Two `ChatGPT` accounts signed in under the same email are two rows, not
/// one collapsed into the other — the bug this plan exists to kill.
#[test]
fn two_accounts_on_one_email_draw_two_distinguishable_rows() {
    let personal = Account::chatgpt("acct-1", "alex@bristol.ac.uk");
    let work = Account::chatgpt("acct-2", "alex@bristol.ac.uk (Acme Ltd)");
    let mut p = Picker::new(
        vec![personal.clone(), work.clone()],
        &Tuning::default(),
        caps_unknown,
    );
    // A model name the fuzzy `acme` cannot spell its way through.
    p.set_models(&personal.id, ModelsState::Loaded(vec!["x-1".into()]));
    p.set_models(&work.id, ModelsState::Loaded(vec!["x-1".into()]));

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
fn a_query_lists_only_what_matches() {
    let mut p = loaded_picker();
    for c in "deepseek".chars() {
        p.key(KeyCode::Char(c));
    }
    assert_eq!(row_labels(&p), vec!["model-c · deepseek"]);
}

/// A query nothing lists leaves nothing to pick: no free-text model rides
/// past the listing.
#[test]
fn enter_on_an_unlisted_query_picks_nothing() {
    let mut p = loaded_picker();
    for c in "model-z".chars() {
        p.key(KeyCode::Char(c));
    }
    assert_eq!(p.rows(), []);
    assert!(p.key(KeyCode::Enter).is_none());
}

#[test]
fn enter_selects_highlighted_model() {
    let mut p = loaded_picker();
    // To the second row, anthropic / model-b.
    p.key(KeyCode::Down);
    let pick = p.key(KeyCode::Enter).expect("a listed row is picked");
    assert_eq!(pick.account, Account::built_in("anthropic"));
    assert_eq!(pick.model, "model-b");
}

/// A declared service lists and selects exactly like a built-in one.
#[test]
fn declared_provider_lists_and_selects() {
    let llama = Account::declared("local-llama");
    let mut p = Picker::new(vec![llama.clone()], &Tuning::default(), caps_unknown);
    p.set_models(&llama.id, ModelsState::Loaded(vec!["model-a".into()]));
    let pick = p.key(KeyCode::Enter).expect("a listed row is picked");
    assert_eq!(pick.account, llama);
    assert_eq!(pick.model, "model-a");
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
    let tuning = p
        .key(KeyCode::Enter)
        .expect("a listed row is picked")
        .tuning;
    assert_eq!(
        tuning.effort.as_ref().map(ReasoningEffort::variant_name),
        Some("high")
    );
    assert_eq!(tuning.temperature, Some(0.1));
    assert_eq!(tuning.top_p, Some(0.05));
}

#[test]
fn opens_seeded_from_initial_tuning() {
    let p = Picker::new(
        vec![Account::built_in("anthropic")],
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
    let anthropic = Account::built_in("anthropic");
    let mut p = Picker::new(vec![anthropic.clone()], &Tuning::default(), caps_split);
    p.set_models(
        &anthropic.id,
        ModelsState::Loaded(vec!["reasoner".into(), "chat-only".into()]),
    );

    // Rows read alphabetically: chat-only, then reasoner.
    p.key(KeyCode::Down); // → reasoner
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
    p.key(KeyCode::Up); // → chat-only
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
    p.key(KeyCode::Down); // → reasoner
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
    p.key(KeyCode::Down); // highlight vendor-b/model-b
    let model = "vendor-b/model-b";
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
    let pick = p.key(KeyCode::Enter).expect("a listed row is picked");
    assert_eq!(pick.model, model);
    assert_eq!(pick.route.as_deref(), Some("deepinfra"));
}

/// Moving the highlight off a route's model deactivates it, and coming back
/// restores it — the choice never rides a model it was not made for.
#[test]
fn route_is_inactive_off_its_model_and_returns_on_it() {
    let mut p = openrouter_picker();
    p.key(KeyCode::Down); // vendor-b/model-b
    let model = "vendor-b/model-b";
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
    p.key(KeyCode::Down); // vendor-b/model-c
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
    let mut p = openrouter_picker(); // first row: vendor-a/model-a
    assert!(
        p.focused_or_model_needing_endpoints().is_none(),
        "nothing requested before the control is focused"
    );
    p.key(KeyCode::Tab); // Search → Provider
    assert_eq!(p.focus, Focus::Provider);
    assert_eq!(
        p.focused_or_model_needing_endpoints().as_deref(),
        Some("vendor-a/model-a")
    );
    p.set_endpoints("vendor-a/model-a", EndpointsState::Loading);
    assert!(
        p.focused_or_model_needing_endpoints().is_none(),
        "seeding Loading dedups the fetch"
    );
}
