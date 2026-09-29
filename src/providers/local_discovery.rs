//! Local LLM auto-discovery: probes well-known localhost ports for Ollama,
//! LM Studio and generic OpenAI-compatible servers (vLLM/LocalAI), and keeps
//! a live catalog of the models they serve under `local/<backend>/<model>`.
//!
//! All three speak the OpenAI `GET /v1/models` listing, so one probe shape
//! covers them; only the port and the catalog slug differ.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::Value;

/// Per-probe budget: a closed localhost port refuses instantly, so this only
/// bounds a wedged listener.
pub const SCAN_TIMEOUT: Duration = Duration::from_millis(500);
pub const SCAN_INTERVAL: Duration = Duration::from_secs(15);
const HOST: &str = "http://127.0.0.1";
const ID_PREFIX: &str = "local/";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocalBackend {
    Ollama,
    LmStudio,
    OpenaiCompat,
}

impl LocalBackend {
    pub const ALL: [LocalBackend; 3] = [
        LocalBackend::Ollama,
        LocalBackend::LmStudio,
        LocalBackend::OpenaiCompat,
    ];

    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            LocalBackend::Ollama => "ollama",
            LocalBackend::LmStudio => "lm-studio",
            LocalBackend::OpenaiCompat => "openai-compat",
        }
    }

    #[must_use]
    pub fn default_port(self) -> u16 {
        match self {
            LocalBackend::Ollama => 11434,
            LocalBackend::LmStudio => 1234,
            LocalBackend::OpenaiCompat => 8080,
        }
    }

    fn from_slug(slug: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|b| b.slug() == slug)
    }
}

/// One reachable local endpoint and the raw model ids it reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub base_url: String,
    pub models: Vec<String>,
}

/// A catalog id resolved back to where and how to send it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTarget {
    pub backend: LocalBackend,
    pub base_url: String,
    /// The id the backend itself expects (no `local/<backend>/` prefix).
    pub model: String,
}

/// Live view of discovered local models, shared between the scanner task,
/// `/v1/models` and dispatch.
#[derive(Debug, Default)]
pub struct LocalCatalog {
    endpoints: RwLock<HashMap<LocalBackend, Endpoint>>,
}

impl LocalCatalog {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Replaces a backend's entry; `None` removes it (backend went away).
    pub fn set(&self, backend: LocalBackend, endpoint: Option<Endpoint>) {
        let Ok(mut map) = self.endpoints.write() else {
            return;
        };
        match endpoint {
            Some(e) => {
                map.insert(backend, e);
            }
            None => {
                map.remove(&backend);
            }
        }
    }

    /// Catalog ids (`local/<backend>/<model>`), sorted.
    #[must_use]
    pub fn model_ids(&self) -> Vec<String> {
        let Ok(map) = self.endpoints.read() else {
            return Vec::new();
        };
        let mut ids: Vec<String> = map
            .iter()
            .flat_map(|(backend, e)| {
                e.models
                    .iter()
                    .map(move |m| format!("{ID_PREFIX}{}/{m}", backend.slug()))
            })
            .collect();
        ids.sort();
        ids
    }

    /// Resolves a `local/<backend>/<model>` id against the live catalog.
    /// Unknown backend, unreachable backend, or a model the backend no
    /// longer lists all yield `None`, so the request falls through to normal
    /// routing rather than erroring on a stale name.
    #[must_use]
    pub fn resolve(&self, id: &str) -> Option<LocalTarget> {
        let rest = id.strip_prefix(ID_PREFIX)?;
        let (slug, model) = rest.split_once('/')?;
        let backend = LocalBackend::from_slug(slug)?;
        let map = self.endpoints.read().ok()?;
        let endpoint = map.get(&backend)?;
        endpoint
            .models
            .iter()
            .any(|m| m == model)
            .then(|| LocalTarget {
                backend,
                base_url: endpoint.base_url.clone(),
                model: model.to_string(),
            })
    }
}

/// Extracts model ids from an OpenAI-style `/v1/models` body, dropping
/// entries without a non-empty string `id` and deduplicating.
#[must_use]
pub fn normalize_models(body: &Value) -> Vec<String> {
    let mut ids: Vec<String> = body
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("id").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Probes one endpoint; `None` on refusal, timeout, non-2xx, bad JSON, or an
/// empty model list (nothing to route to).
pub async fn scan_endpoint(
    client: &reqwest::Client,
    base_url: &str,
    timeout: Duration,
) -> Option<Endpoint> {
    let resp = client
        .get(format!("{base_url}/v1/models"))
        .timeout(timeout)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let body: Value = resp.json().await.ok()?;
    let models = normalize_models(&body);
    (!models.is_empty()).then(|| Endpoint {
        base_url: base_url.to_string(),
        models,
    })
}

/// Rescans every backend once and updates the catalog.
pub async fn scan_all(client: &reqwest::Client, catalog: &LocalCatalog) {
    let scans = LocalBackend::ALL.map(|backend| async move {
        let base_url = format!("{HOST}:{}", backend.default_port());
        (
            backend,
            scan_endpoint(client, &base_url, SCAN_TIMEOUT).await,
        )
    });
    for (backend, endpoint) in futures_util::future::join_all(scans).await {
        catalog.set(backend, endpoint);
    }
}

/// Background task: keeps `catalog` current until the runtime shuts down.
pub async fn run_local_discovery(catalog: Arc<LocalCatalog>, interval: Duration) {
    let client = reqwest::Client::new();
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        scan_all(&client, &catalog).await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn endpoint(models: &[&str]) -> Endpoint {
        Endpoint {
            base_url: "http://127.0.0.1:11434".to_string(),
            models: models.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn normalize_models_should_sort_dedup_and_skip_malformed_entries() {
        let body = json!({"data": [
            {"id": "qwen3:8b"}, {"id": "llama3.2"}, {"id": "llama3.2"},
            {"id": ""}, {"name": "no-id"}, {"id": 7}
        ]});
        assert_eq!(normalize_models(&body), vec!["llama3.2", "qwen3:8b"]);
    }

    #[test]
    fn normalize_models_should_return_empty_for_unexpected_shape() {
        assert!(normalize_models(&json!({"models": []})).is_empty());
        assert!(normalize_models(&json!("nope")).is_empty());
    }

    #[test]
    fn catalog_should_list_prefixed_ids_per_backend() {
        let catalog = LocalCatalog::default();
        catalog.set(LocalBackend::Ollama, Some(endpoint(&["llama3.2"])));
        catalog.set(LocalBackend::LmStudio, Some(endpoint(&["gemma"])));
        assert_eq!(
            catalog.model_ids(),
            vec!["local/lm-studio/gemma", "local/ollama/llama3.2"]
        );
    }

    #[test]
    fn catalog_should_drop_backend_when_set_to_none() {
        let catalog = LocalCatalog::default();
        catalog.set(LocalBackend::Ollama, Some(endpoint(&["llama3.2"])));
        catalog.set(LocalBackend::Ollama, None);
        assert!(catalog.model_ids().is_empty());
    }

    #[test]
    fn resolve_should_strip_prefix_and_keep_colons_in_model_ids() {
        let catalog = LocalCatalog::default();
        catalog.set(
            LocalBackend::Ollama,
            Some(endpoint(&["qwen3:8b", "org/model"])),
        );
        let t = catalog.resolve("local/ollama/qwen3:8b").unwrap();
        assert_eq!(t.model, "qwen3:8b");
        assert_eq!(t.base_url, "http://127.0.0.1:11434");
        assert_eq!(
            catalog.resolve("local/ollama/org/model").unwrap().model,
            "org/model"
        );
    }

    #[test]
    fn resolve_should_return_none_for_unknown_backend_model_or_prefix() {
        let catalog = LocalCatalog::default();
        catalog.set(LocalBackend::Ollama, Some(endpoint(&["llama3.2"])));
        assert!(catalog.resolve("local/ollama/missing").is_none());
        assert!(catalog.resolve("local/lm-studio/llama3.2").is_none());
        assert!(catalog.resolve("local/bogus/llama3.2").is_none());
        assert!(catalog.resolve("claude-opus-4-5").is_none());
        assert!(catalog.resolve("local/ollama").is_none());
    }

    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn scan_endpoint_should_parse_models_from_live_server() {
        let app = axum::Router::new().route(
            "/v1/models",
            axum::routing::get(|| async { axum::Json(json!({"data": [{"id": "llama3.2"}]})) }),
        );
        let base = serve(app).await;
        let found = scan_endpoint(&reqwest::Client::new(), &base, SCAN_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(found.models, vec!["llama3.2"]);
        assert_eq!(found.base_url, base);
    }

    #[tokio::test]
    async fn scan_endpoint_should_return_none_when_port_is_closed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let found = scan_endpoint(
            &reqwest::Client::new(),
            &format!("http://{addr}"),
            SCAN_TIMEOUT,
        )
        .await;
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn scan_endpoint_should_time_out_on_a_wedged_listener() {
        let app = axum::Router::new().route(
            "/v1/models",
            axum::routing::get(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                axum::Json(json!({"data": [{"id": "late"}]}))
            }),
        );
        let base = serve(app).await;
        let started = std::time::Instant::now();
        let found = scan_endpoint(&reqwest::Client::new(), &base, Duration::from_millis(100)).await;
        assert!(found.is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn scan_endpoint_should_return_none_for_non_2xx_and_empty_lists() {
        let app = axum::Router::new().route(
            "/v1/models",
            axum::routing::get(|| async { axum::Json(json!({"data": []})) }),
        );
        let base = serve(app).await;
        assert!(scan_endpoint(&reqwest::Client::new(), &base, SCAN_TIMEOUT)
            .await
            .is_none());
        let bad = serve(axum::Router::new()).await; // 404 on /v1/models
        assert!(scan_endpoint(&reqwest::Client::new(), &bad, SCAN_TIMEOUT)
            .await
            .is_none());
    }
}
