//! Emulated server-tool (Brave search / browser) counters.

use std::sync::atomic::Ordering;

use serde_json::{json, Value};

use super::ProxyMetrics;

impl ProxyMetrics {
    fn record_server_tool_search_common(&self, backend: &str) {
        self.server_tool_searches_total
            .fetch_add(1, Ordering::Relaxed);
        self.server_tool_backend_hits
            .entry(backend.to_string())
            .or_default()
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_server_tool_search_ok(&self, backend: &str) {
        self.record_server_tool_search_common(backend);
        self.server_tool_searches_ok.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_server_tool_search_failed(&self, backend: &str) {
        self.record_server_tool_search_common(backend);
        self.server_tool_search_failures
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record loop iterations for one emulated request.
    pub fn record_server_tool_iterations(&self, iterations: u64) {
        self.server_tool_iterations_total
            .fetch_add(iterations, Ordering::Relaxed);
    }

    /// Snapshot the server-tool emulation counters, including the
    /// per-backend (`brave`/`browser`) breakdown for the D4 dashboard.
    pub(super) fn server_tools_json(&self) -> Value {
        let backends: serde_json::Map<String, Value> = self
            .server_tool_backend_hits
            .iter()
            .map(|entry| {
                (
                    entry.key().clone(),
                    json!(entry.value().load(Ordering::Relaxed)),
                )
            })
            .collect();
        json!({
            "searches_total": self.server_tool_searches_total.load(Ordering::Relaxed),
            "searches_ok": self.server_tool_searches_ok.load(Ordering::Relaxed),
            "search_failures": self.server_tool_search_failures.load(Ordering::Relaxed),
            "iterations_total": self.server_tool_iterations_total.load(Ordering::Relaxed),
            "by_backend": backends
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::ProxyMetrics;
    use std::sync::atomic::Ordering;

    #[test]
    fn record_server_tool_search_should_label_backends_separately() {
        let m = ProxyMetrics::new();
        m.record_server_tool_search_ok("brave");
        m.record_server_tool_search_ok("brave");
        m.record_server_tool_search_ok("browser");
        m.record_server_tool_search_failed("unserved");
        m.record_server_tool_iterations(3);

        assert_eq!(m.server_tool_searches_total.load(Ordering::Relaxed), 4);
        assert_eq!(m.server_tool_searches_ok.load(Ordering::Relaxed), 3);
        assert_eq!(m.server_tool_search_failures.load(Ordering::Relaxed), 1);
        assert_eq!(m.server_tool_iterations_total.load(Ordering::Relaxed), 3);

        let json = m.to_json();
        assert_eq!(json["server_tools"]["searches_total"], serde_json::json!(4));
        assert_eq!(
            json["server_tools"]["by_backend"]["brave"],
            serde_json::json!(2)
        );
        assert_eq!(
            json["server_tools"]["by_backend"]["browser"],
            serde_json::json!(1)
        );
        assert_eq!(
            json["server_tools"]["by_backend"]["unserved"],
            serde_json::json!(1)
        );
    }
}
