//! Claude Code auto mode's server-side classifier review (see
//! <https://code.claude.com/docs/en/auto-mode-classifier-billing>).
//!
//! Claude Code asks the Anthropic server to review risky actions by adding a `safeguards` request
//! field and reading `safeguard_results` back from the response. A gateway must pass both through
//! unchanged: the Anthropic and Bedrock paths already forward the body verbatim, so the only
//! consolette-side hazards are an upstream that rebuilds the request (the field is dropped, Claude
//! Code falls back to its own classifier requests and bills them) and a cache replaying one
//! request's verdict for another.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

pub const SAFEGUARDS_FIELD: &str = "safeguards";

#[must_use]
pub fn request_has_safeguards(body: &Value) -> bool {
    body.get(SAFEGUARDS_FIELD).is_some()
}

/// Provider kinds (`Provider::name()`) that translate the request into another API's schema, so
/// `safeguards` cannot reach a server that understands it.
#[must_use]
pub fn drops_safeguards(provider_name: &str) -> bool {
    matches!(provider_name, "openai" | "gemini" | "openrouter")
}

/// Logs once per provider kind when a request carrying `safeguards` is sent to an upstream that
/// cannot honor it, naming the client-side switch that silences the resulting billing notice.
pub fn warn_if_safeguards_dropped(provider_name: &str, body: &Value) {
    if !request_has_safeguards(body) || !drops_safeguards(provider_name) {
        return;
    }
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let first_time = WARNED
        .get_or_init(Mutex::default)
        .lock()
        .is_ok_and(|mut warned| warned.insert(provider_name.to_string()));
    if first_time {
        tracing::warn!(
            upstream_kind = provider_name,
            "request carries `safeguards` but this upstream translates the request, so Claude \
             Code's server-side auto-mode review is unavailable; Claude Code will use its own \
             classifier requests (billed as token usage). Set CLAUDE_CODE_AUTO_MODE_SERVER=0 in \
             the client to stop it asking."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_has_safeguards_should_detect_the_field_by_presence() {
        assert!(request_has_safeguards(&json!({"safeguards": {}})));
        assert!(request_has_safeguards(&json!({"safeguards": null})));
        assert!(!request_has_safeguards(&json!({"model": "m"})));
    }

    #[test]
    fn drops_safeguards_should_flag_only_translating_provider_kinds() {
        for kind in ["openai", "gemini", "openrouter"] {
            assert!(drops_safeguards(kind), "{kind}");
        }
        for kind in ["anthropic", "bedrock"] {
            assert!(!drops_safeguards(kind), "{kind}");
        }
    }
}
