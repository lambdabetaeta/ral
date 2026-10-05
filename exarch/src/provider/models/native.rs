//! The listings Anthropic, Gemini and `DeepSeek` serve themselves, which —
//! unlike genai's — report each model's context window.

use super::Listed;
use crate::provider::error::body_detail;
use crate::provider::identity::Account;
use futures_util::{TryStreamExt, stream};
use genai::adapter::AdapterKind;
use reqwest::{Client, RequestBuilder};
use serde_json::Value;

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
    page: fn(&Value) -> Page,
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
            Some(None),
            move |pending: Option<Option<String>>| async move {
                let Some(cursor) = pending else {
                    return Ok(None);
                };
                let (models, next) = self.fetch(client, account, key, cursor.as_deref()).await?;
                Ok(Some((models, next.map(Some))))
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
        Ok((self.page)(&body))
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

fn id_of(entry: &Value, key: &str) -> Option<String> {
    entry.get(key)?.as_str().map(str::to_owned)
}

/// One page of Anthropic's `GET /models`, and the `after_id` of the next.
fn anthropic_page(body: &Value) -> Page {
    let listed = body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(Listed {
                id: id_of(m, "id")?,
                context_window: window(&m["max_input_tokens"]),
            })
        })
        .collect();
    let next = body["has_more"]
        .as_bool()
        .unwrap_or(false)
        .then(|| id_of(body, "last_id"))
        .flatten();
    (listed, next)
}

/// `DeepSeek`'s `GET /models`, which has no paging.
fn deepseek_page(body: &Value) -> Page {
    let listed = body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(Listed {
                id: id_of(m, "id")?,
                context_window: window(&m["context_window"]),
            })
        })
        .collect();
    (listed, None)
}

/// One page of Gemini's `GET /models`, and the `pageToken` of the next.
fn gemini_page(body: &Value) -> Page {
    let listed = body["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let name = id_of(m, "name")?;
            Some(Listed {
                id: name
                    .strip_prefix("models/")
                    .map_or_else(|| name.clone(), str::to_owned),
                context_window: window(&m["inputTokenLimit"]),
            })
        })
        .collect();
    (listed, id_of(body, "nextPageToken"))
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
        let (listed, next) = anthropic_page(&body);
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
        assert_eq!(anthropic_page(&last).1, None);
    }

    #[test]
    fn deepseek_page_reads_windows() {
        let body = serde_json::json!({"data": [
            {"id": "deepseek-flash", "context_window": 1_000_000},
            {"id": "deepseek-zero", "context_window": 0},
        ]});
        let (listed, next) = deepseek_page(&body);
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
        let (listed, next) = gemini_page(&body);
        assert_eq!(
            listed,
            vec![Listed {
                id: "gemini-3-pro".into(),
                context_window: Some(1_048_576)
            }]
        );
        assert_eq!(next.as_deref(), Some("tok"));
    }
}
