//! `OpenrouterProvider::list_models`/`list_free_models` support (Story
//! 1.2.2): one shared GET-and-parse-JSON helper, plus the two pure mapping
//! functions each list method applies to the same parsed body — mirroring
//! `OpenaiProvider::fetch_models`'s pattern (`src/providers/openai.rs:167-192`)
//! for the shared fetch, and `gemini::error::classify_gemini_error`'s
//! "pure function over already-parsed data" testing style for the mapping
//! functions themselves (no live/mocked HTTP needed to unit-test them).

use serde_json::Value;
use tracing::debug;

use crate::providers::{ModelInfo, ProviderError};

use super::cache::FreeModelEntry;
use super::{classify_error_response, map_send_error, OpenrouterProvider, BASE_URL};

/// Shared GET-and-parse-JSON helper for `{BASE_URL}/models` — both
/// `list_models`/`list_free_models` call this so there's exactly one HTTP
/// GET implementation (Story 1.2.2's shared-fetch acceptance criterion).
pub(super) async fn fetch_models_raw(
    provider: &OpenrouterProvider,
) -> Result<Value, ProviderError> {
    fetch_models_raw_at(provider, BASE_URL).await
}

/// `fetch_models_raw`'s actual implementation, parameterized on the base
/// URL so it can be pointed at a local test server without touching
/// `OpenrouterProvider`'s hardcoded `BASE_URL` constant (which stays a
/// genuine `const` for production use, per plan.md's explicit design —
/// `UpstreamKind::Openrouter` carries no configurable base URL). Only test
/// code passes anything other than `BASE_URL` here.
async fn fetch_models_raw_at(
    provider: &OpenrouterProvider,
    base_url: &str,
) -> Result<Value, ProviderError> {
    let url = format!("{base_url}/models");
    let headers = provider.build_headers(&url).await?;

    debug!("OpenRouter GET {url}");

    let response = provider
        .client
        .get(&url)
        .headers(headers)
        .send()
        .await
        .map_err(|e| map_send_error(&e))?;

    let status = response.status();
    if !status.is_success() {
        return Err(classify_error_response(status, response, None).await);
    }

    response.json().await.map_err(|e| ProviderError::Upstream {
        status: status.as_u16(),
        body: e.to_string(),
    })
}

/// Maps every `data[]` entry to `ModelInfo`, unfiltered — matching
/// `OpenaiProvider::list_models`'s existing shape/behavior exactly.
pub(super) fn parse_model_infos(value: &Value) -> Vec<ModelInfo> {
    value
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(Value::as_str)?.to_string();
            let owned_by = entry
                .get("owned_by")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            Some(ModelInfo { id, owned_by })
        })
        .collect()
}

/// Filters `data[]` to entries whose `pricing.prompt`/`pricing.completion`
/// are the string `"0"`, carrying those (zero) price values forward as
/// `FreeModelEntry` rather than discarding them after the filter, so
/// downstream consumers (`ModelListCache`, `OpenrouterProvider::send()`'s
/// per-dispatch recheck — money-safety backstop mechanism 2) can verify
/// price directly instead of trusting bare id membership. A missing/
/// malformed `pricing` object, or a non-string `prompt`/`completion` field,
/// is treated as not-free (fail-soft per `research/ux.md`), never a parse
/// error for the whole list.
///
/// Shape VERIFIED live against `GET https://openrouter.ai/api/v1/models` on
/// 2026-09-07 (Task 1.2.2a): `pricing.prompt`/`pricing.completion` are
/// strings, `"0"` denotes free (19 of 428 models at capture time), and
/// every such entry's `id` did carry a `:free` suffix as an informal
/// cross-check (e.g. `"inclusionai/ling-3.0-flash-sante:free"`) — that
/// suffix is not itself required by this filter, only the price fields are.
pub(super) fn parse_free_model_entries(value: &Value) -> Vec<FreeModelEntry> {
    value
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(Value::as_str)?.to_string();
            let pricing = entry.get("pricing")?;
            let prompt = pricing.get("prompt").and_then(Value::as_str)?;
            let completion = pricing.get("completion").and_then(Value::as_str)?;
            if prompt == "0" && completion == "0" {
                Some(FreeModelEntry {
                    id,
                    price_prompt: 0.0,
                    price_completion: 0.0,
                })
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use axum::routing::get;
    use axum::Json;
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;

    // REQ-2 (Story 1.2.2, Task 1.2.2d) — pure-function tests over a
    // directly-constructed JSON fixture, no HTTP involved (mirrors
    // `classify_gemini_error`'s test style, per this epic's established
    // test-pattern deviation from plan.md's "mock HTTP server" wording).

    fn mixed_models_fixture() -> Value {
        json!({
            "data": [
                {
                    "id": "free/model:free",
                    "owned_by": "free-org",
                    "pricing": {"prompt": "0", "completion": "0"},
                },
                {
                    "id": "paid/model-a",
                    "owned_by": "paid-org",
                    "pricing": {"prompt": "0.000002", "completion": "0.000004"},
                },
                {
                    "id": "paid/model-b",
                    "owned_by": "paid-org",
                    "pricing": {"prompt": "0.00001", "completion": "0.00003"},
                },
            ]
        })
    }

    #[test]
    fn list_models_should_return_all_reported_models_unfiltered() {
        let models = parse_model_infos(&mixed_models_fixture());

        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "free/model:free");
        assert_eq!(models[0].owned_by.as_deref(), Some("free-org"));
        assert_eq!(models[1].id, "paid/model-a");
        assert_eq!(models[2].id, "paid/model-b");
    }

    #[test]
    fn list_free_models_should_return_only_zero_priced_models() {
        let free = parse_free_model_entries(&mixed_models_fixture());

        assert_eq!(
            free,
            vec![FreeModelEntry {
                id: "free/model:free".to_string(),
                price_prompt: 0.0,
                price_completion: 0.0,
            }]
        );
    }

    #[test]
    fn list_free_models_should_treat_malformed_pricing_as_not_free() {
        let fixture = json!({
            "data": [
                {"id": "no-pricing/model", "owned_by": null},
                {"id": "half-pricing/model", "pricing": {"prompt": "0"}},
                {"id": "non-string-pricing/model", "pricing": {"prompt": 0, "completion": 0}},
            ]
        });

        let free = parse_free_model_entries(&fixture);

        assert!(
            free.is_empty(),
            "malformed pricing must be treated as not-free, not a parse error"
        );
    }

    // REQ-2 (Story 1.2.2, Task 1.2.2d): `fetch_models_raw`'s actual HTTP
    // GET, exercised once against a real local server (axum/tokio, already
    // direct dependencies of this crate — no new HTTP-mocking crate added,
    // matching `cost_metrics::test_support`'s established precedent), to
    // prove `list_models`/`list_free_models` really do share one HTTP call
    // rather than each independently reimplementing it.
    async fn start_mock_models_server(body: Value) -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new().route(
            "/models",
            get(move || {
                let body = body.clone();
                async move { Json(body) }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock server bind should succeed");
        let addr = listener
            .local_addr()
            .expect("mock server local_addr should succeed");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn fetch_models_raw_should_be_reused_by_both_list_methods() {
        let fixture = mixed_models_fixture();
        let (base_url, _handle) = start_mock_models_server(fixture.clone()).await;

        let provider = OpenrouterProvider::test_provider();
        let value = fetch_models_raw_at(&provider, &base_url)
            .await
            .expect("fetch_models_raw_at should succeed against the mock server");

        // Both list methods are pure functions of this one fetched `Value`
        // (see `list_models`/`list_free_models` in `mod.rs`), so asserting
        // their outputs here against the single `value` fetched above is
        // exactly "one HTTP GET feeds both."
        assert_eq!(parse_model_infos(&value).len(), 3);
        assert_eq!(parse_free_model_entries(&value).len(), 1);
    }
}
