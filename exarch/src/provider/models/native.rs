//! The listings Anthropic, Gemini and `DeepSeek` serve themselves, which —
//! unlike genai's — report each model's context window.

use super::Listed;
use crate::provider::error::body_detail;
use crate::provider::identity::Account;
use futures_util::{TryStreamExt, stream};
use genai::adapter::AdapterKind;
use reqwest::{Client, RequestBuilder};
use serde_json::Value;
use std::collections::HashSet;

const ANTHROPIC_VERSION: &str = "2023-06-01";
const PAGE_SIZE: &str = "1000";

/// One page of a listing, and the cursor of the next, if any.
type Page = (Vec<Listed>, Option<String>);

/// A provider's own listing: where it lives, how a key is presented, how it
/// pages, and how a page reads.
pub(super) struct Native {
    base: &'static str,
    authorise: fn(RequestBuilder, &str) -> RequestBuilder,
    /// The page-size and cursor parameters; `None` for a listing that arrives whole.
    paging: Option<(&'static str, &'static str)>,
    page: fn(&Value) -> Result<Page, String>,
}

static ANTHROPIC: Native = Native {
    base: "https://api.anthropic.com/v1/",
    authorise: |request, key| {
        request
            .header("x-api-key", key)
            .header("anthropic-version", ANTHROPIC_VERSION)
    },
    paging: Some(("limit", "after_id")),
    page: anthropic_page,
};

static GEMINI: Native = Native {
    base: "https://generativelanguage.googleapis.com/v1beta/",
    authorise: |request, key| request.header("x-goog-api-key", key),
    paging: Some(("pageSize", "pageToken")),
    page: gemini_page,
};

static DEEPSEEK: Native = Native {
    base: "https://api.deepseek.com/v1/",
    authorise: |request, key| request.bearer_auth(key),
    paging: None,
    page: deepseek_page,
};

impl Native {
    /// `None` leaves the listing to genai, which reports no window.
    pub(super) fn of(adapter: AdapterKind) -> Option<&'static Self> {
        match adapter {
            AdapterKind::Anthropic => Some(&ANTHROPIC),
            AdapterKind::Gemini => Some(&GEMINI),
            AdapterKind::DeepSeek => Some(&DEEPSEEK),
            _ => None,
        }
    }

    /// Every page of `account`'s listing, followed to the last.
    pub(super) async fn list(&self, account: &Account, key: &str) -> Result<Vec<Listed>, String> {
        let client = &crate::provider::tls::client();
        // The cursor still to fetch (`None` before the first page); `None` once the last is in.
        stream::try_unfold(
            (Some(None), HashSet::new()),
            move |(pending, mut seen): (Option<Option<String>>, HashSet<String>)| async move {
                let Some(cursor) = pending else {
                    return Ok(None);
                };
                let (models, next) = self.fetch(client, account, key, cursor.as_deref()).await?;
                record_cursor(next.as_deref(), &mut seen)?;
                Ok(Some((models, (next.map(Some), seen))))
            },
        )
        .try_concat()
        .await
    }

    /// One page, from `cursor` on.
    async fn fetch(
        &self,
        client: &Client,
        account: &Account,
        key: &str,
        cursor: Option<&str>,
    ) -> Result<Page, String> {
        let params: Vec<_> = self
            .paging
            .into_iter()
            .flat_map(|(size, after)| {
                std::iter::once((size, PAGE_SIZE)).chain(cursor.map(|at| (after, at)))
            })
            .collect();
        let request = (self.authorise)(client.get(self.url(account, &params)?), key);
        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!("HTTP {status}: {}", body_detail(&body)));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|e| format!("unreadable listing: {e}"))?;
        (self.page)(&body)
    }

    /// The account's `models` URL — its declared endpoint, else this listing's
    /// base — with `params` percent-encoded onto it.
    fn url(&self, account: &Account, params: &[(&str, &str)]) -> Result<reqwest::Url, String> {
        let base = account.service.endpoint.as_deref().unwrap_or(self.base);
        let sep = if base.ends_with('/') { "" } else { "/" };
        reqwest::Url::parse_with_params(&format!("{base}{sep}models"), params)
            .map_err(|e| format!("bad endpoint {base}: {e}"))
    }
}

/// A window of `0` or `null` on the wire is no window.
fn window(wire: &Value) -> Option<u64> {
    wire.as_u64().filter(|&n| n > 0)
}

fn catalog<'a>(body: &'a Value, key: &str) -> Result<&'a [Value], String> {
    body.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| format!("invalid model listing: `{key}` must be an array"))
}

fn id_of(entry: &Value, key: &str) -> Result<String, String> {
    entry
        .get(key)
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("invalid model listing: `{key}` must be a non-empty string"))
}

fn record_cursor(cursor: Option<&str>, seen: &mut HashSet<String>) -> Result<(), String> {
    if let Some(cursor) = cursor
        && !seen.insert(cursor.to_owned())
    {
        return Err(
            "invalid model listing: pagination cursor repeated; refusing to fetch the same page again"
                .into(),
        );
    }
    Ok(())
}

/// One page of Anthropic's `GET /models`, and the `after_id` of the next.
fn anthropic_page(body: &Value) -> Result<Page, String> {
    let listed = catalog(body, "data")?
        .iter()
        .map(|m| {
            Ok(Listed {
                id: id_of(m, "id")?,
                context_window: window(&m["max_input_tokens"]),
            })
        })
        .collect::<Result<_, String>>()?;
    let has_more = body["has_more"]
        .as_bool()
        .ok_or("invalid model listing: `has_more` must be a boolean")?;
    let next = has_more.then(|| id_of(body, "last_id")).transpose()?;
    Ok((listed, next))
}

/// `DeepSeek`'s `GET /models`, which has no paging.
fn deepseek_page(body: &Value) -> Result<Page, String> {
    let listed = catalog(body, "data")?
        .iter()
        .map(|m| {
            Ok(Listed {
                id: id_of(m, "id")?,
                context_window: window(&m["context_window"]),
            })
        })
        .collect::<Result<_, String>>()?;
    Ok((listed, None))
}

/// One page of Gemini's `GET /models`, and the `pageToken` of the next.
fn gemini_page(body: &Value) -> Result<Page, String> {
    let body = body
        .as_object()
        .ok_or("invalid model listing: Gemini response must be an object")?;
    // ProtoJSON omits empty repeated fields and accepts null as unset.
    let models = match body.get("models") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(models)) => models.as_slice(),
        Some(_) => return Err("invalid model listing: `models` must be an array".into()),
    };
    let listed = models
        .iter()
        .map(|m| {
            let name = id_of(m, "name")?;
            let id = name.strip_prefix("models/").unwrap_or(&name);
            if id.trim().is_empty() {
                return Err("invalid model listing: `name` must contain a model ID".into());
            }
            Ok(Listed {
                id: id.to_owned(),
                context_window: window(&m["inputTokenLimit"]),
            })
        })
        .collect::<Result<_, String>>()?;
    let next = match body.get("nextPageToken") {
        None | Some(Value::Null) => None,
        Some(Value::String(token)) if token.is_empty() => None,
        Some(Value::String(token)) => Some(token.clone()),
        Some(_) => return Err("invalid model listing: `nextPageToken` must be a string".into()),
    };
    Ok((listed, next))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_page_reads_windows_and_the_cursor() {
        let body = serde_json::json!({
            "data": [
                {"id": "claude-opus-4-1", "max_input_tokens": 200_000},
                {"id": "claude-old", "max_input_tokens": null},
            ],
            "has_more": true,
            "last_id": "claude-old",
        });
        let (listed, next) = anthropic_page(&body).unwrap();
        assert_eq!(
            listed,
            vec![
                Listed {
                    id: "claude-opus-4-1".into(),
                    context_window: Some(200_000)
                },
                Listed::bare("claude-old"),
            ]
        );
        assert_eq!(next.as_deref(), Some("claude-old"));
        let last = serde_json::json!({"data": [], "has_more": false, "last_id": "x"});
        assert_eq!(anthropic_page(&last).unwrap().1, None);
    }

    #[test]
    fn deepseek_page_reads_windows() {
        let body = serde_json::json!({"data": [
            {"id": "deepseek-flash", "context_window": 1_000_000},
            {"id": "deepseek-zero", "context_window": 0},
        ]});
        let (listed, next) = deepseek_page(&body).unwrap();
        assert_eq!(listed[0].context_window, Some(1_000_000));
        assert_eq!(listed[1], Listed::bare("deepseek-zero"));
        assert_eq!(next, None);
    }

    #[test]
    fn gemini_page_strips_the_prefix_and_reads_the_token() {
        let body = serde_json::json!({
            "models": [{"name": "models/gemini-3-pro", "inputTokenLimit": 1_048_576}],
            "nextPageToken": "tok",
        });
        let (listed, next) = gemini_page(&body).unwrap();
        assert_eq!(
            listed,
            vec![Listed {
                id: "gemini-3-pro".into(),
                context_window: Some(1_048_576)
            }]
        );
        assert_eq!(next.as_deref(), Some("tok"));
    }

    #[test]
    fn empty_catalogs_are_valid() {
        for (native, body) in [
            (
                &ANTHROPIC,
                serde_json::json!({"data": [], "has_more": false}),
            ),
            (&DEEPSEEK, serde_json::json!({"data": []})),
            (&GEMINI, serde_json::json!({"models": []})),
            (
                &GEMINI,
                serde_json::json!({"models": [], "nextPageToken": ""}),
            ),
        ] {
            assert_eq!((native.page)(&body).unwrap(), (vec![], None));
        }
    }

    #[test]
    fn rest_catalog_arrays_are_required() {
        for (native, key) in [(&ANTHROPIC, "data"), (&DEEPSEEK, "data")] {
            assert!((native.page)(&serde_json::json!({"has_more": false})).is_err());
            for invalid in [
                Value::Null,
                serde_json::json!(false),
                serde_json::json!(0),
                serde_json::json!("not an array"),
                serde_json::json!({}),
            ] {
                let mut body = serde_json::json!({"has_more": false});
                body[key] = invalid;
                assert!((native.page)(&body).unwrap_err().contains(key));
            }
        }
    }

    #[test]
    fn malformed_ids_reject_the_whole_page() {
        for (native, array, id) in [
            (&ANTHROPIC, "data", "id"),
            (&DEEPSEEK, "data", "id"),
            (&GEMINI, "models", "name"),
        ] {
            for invalid in [
                Value::Null,
                serde_json::json!({}),
                serde_json::json!({(id): null}),
                serde_json::json!({(id): 42}),
                serde_json::json!({(id): false}),
                serde_json::json!({(id): []}),
                serde_json::json!({(id): {}}),
                serde_json::json!({(id): ""}),
                serde_json::json!({(id): "   "}),
            ] {
                let mut body = serde_json::json!({"has_more": false});
                body[array] = serde_json::json!([{(id): "valid-model"}, invalid]);
                assert!((native.page)(&body).unwrap_err().contains(id));
            }
        }
        for name in ["models/", "models/   "] {
            let body = serde_json::json!({"models": [{"name": name}]});
            assert!(gemini_page(&body).is_err());
        }
    }

    #[test]
    fn optional_windows_remain_unknown() {
        for (native, array, id, window) in [
            (&ANTHROPIC, "data", "id", "max_input_tokens"),
            (&DEEPSEEK, "data", "id", "context_window"),
            (&GEMINI, "models", "name", "inputTokenLimit"),
        ] {
            let mut body = serde_json::json!({"has_more": false});
            body[array] = serde_json::json!([
                {(id): "absent"},
                {(id): "null", (window): null},
                {(id): "zero", (window): 0},
            ]);
            let (listed, _) = (native.page)(&body).unwrap();
            assert_eq!(listed.len(), 3);
            assert!(listed.iter().all(|model| model.context_window.is_none()));
        }
    }

    #[test]
    fn anthropic_requires_complete_paging_metadata() {
        assert!(anthropic_page(&serde_json::json!({"data": []})).is_err());
        for has_more in [
            Value::Null,
            serde_json::json!("false"),
            serde_json::json!(0),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            let body = serde_json::json!({"data": [], "has_more": has_more});
            assert!(anthropic_page(&body).unwrap_err().contains("has_more"));
        }
        assert!(anthropic_page(&serde_json::json!({"data": [], "has_more": true})).is_err());
        for last_id in [
            Value::Null,
            serde_json::json!(42),
            serde_json::json!(false),
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!(""),
            serde_json::json!("   "),
        ] {
            let body = serde_json::json!({"data": [], "has_more": true, "last_id": last_id});
            assert!(anthropic_page(&body).unwrap_err().contains("last_id"));
        }
    }

    #[test]
    fn gemini_accepts_proto_json_defaults() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({"models": null}),
            serde_json::json!({"nextPageToken": null}),
            serde_json::json!({"models": null, "nextPageToken": null}),
            serde_json::json!({"models": [], "nextPageToken": null}),
        ] {
            assert_eq!(gemini_page(&body).unwrap(), (vec![], None));
        }
    }

    #[test]
    fn gemini_rejects_malformed_envelopes() {
        for body in [Value::Null, serde_json::json!([]), serde_json::json!(42)] {
            assert!(gemini_page(&body).unwrap_err().contains("object"));
        }
        for models in [
            serde_json::json!(true),
            serde_json::json!(42),
            serde_json::json!("not an array"),
            serde_json::json!({}),
        ] {
            let body = serde_json::json!({"models": models});
            assert!(gemini_page(&body).unwrap_err().contains("models"));
        }
    }

    #[test]
    fn gemini_rejects_non_string_tokens() {
        for token in [
            serde_json::json!(42),
            serde_json::json!(false),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            let body = serde_json::json!({"models": [], "nextPageToken": token});
            assert!(gemini_page(&body).unwrap_err().contains("nextPageToken"));
        }
    }

    #[test]
    fn repeated_cursors_are_rejected_without_disclosing_them() {
        for cursors in [
            vec!["secret-a", "secret-a"],
            vec!["secret-a", "secret-b", "secret-a"],
        ] {
            let mut seen = HashSet::new();
            record_cursor(None, &mut seen).unwrap();
            for cursor in &cursors[..cursors.len() - 1] {
                record_cursor(Some(cursor), &mut seen).unwrap();
            }
            let error = record_cursor(cursors.last().copied(), &mut seen).unwrap_err();
            assert!(error.contains("cursor repeated"));
            assert!(!error.contains("secret"));
        }
        let mut seen = HashSet::new();
        record_cursor(Some("last"), &mut seen).unwrap();
        record_cursor(None, &mut seen).unwrap();
    }
}
