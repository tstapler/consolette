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
use std::sync::{Mutex, OnceLock, PoisonError};

use serde_json::Value;

use crate::providers::Provider;

pub const SAFEGUARDS_FIELD: &str = "safeguards";

#[must_use]
pub fn request_has_safeguards(body: &Value) -> bool {
    body.get(SAFEGUARDS_FIELD).is_some()
}

/// True the first time a `safeguards` request reaches each provider kind that translates the
/// request; records `provider_name` in `seen`. Checks before inserting so repeat calls don't
/// allocate, and recovers a poisoned lock rather than silencing the warning.
fn first_dropped_for(
    seen: &Mutex<HashSet<String>>,
    provider_name: &str,
    translates_request: bool,
    body: &Value,
) -> bool {
    if !translates_request || !request_has_safeguards(body) {
        return false;
    }
    let mut seen = seen.lock().unwrap_or_else(PoisonError::into_inner);
    !seen.contains(provider_name) && seen.insert(provider_name.to_string())
}

/// Logs once per provider kind when a request carrying `safeguards` is sent to an upstream that
/// cannot honor it, naming the client-side switch that silences the resulting billing notice.
pub fn warn_if_safeguards_dropped(provider: &dyn Provider, body: &Value) {
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = WARNED.get_or_init(Mutex::default);
    if first_dropped_for(seen, provider.name(), provider.translates_request(), body) {
        tracing::warn!(
            upstream_kind = provider.name(),
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

    fn guarded() -> Value {
        json!({"safeguards": {}})
    }

    #[test]
    fn request_has_safeguards_should_detect_the_field_by_presence() {
        assert!(request_has_safeguards(&json!({"safeguards": {}})));
        assert!(request_has_safeguards(&json!({"safeguards": null})));
        assert!(!request_has_safeguards(&json!({"model": "m"})));
    }

    #[test]
    fn first_dropped_for_should_fire_once_per_provider_kind() {
        let seen = Mutex::default();

        assert!(first_dropped_for(&seen, "openai", true, &guarded()));
        assert!(!first_dropped_for(&seen, "openai", true, &guarded()));
        assert!(first_dropped_for(&seen, "gemini", true, &guarded()));
    }

    #[test]
    fn first_dropped_for_should_stay_silent_for_verbatim_providers_and_plain_requests() {
        let seen = Mutex::default();

        assert!(!first_dropped_for(&seen, "anthropic", false, &guarded()));
        assert!(!first_dropped_for(
            &seen,
            "openai",
            true,
            &json!({"model": "m"})
        ));
        assert!(
            first_dropped_for(&seen, "openai", true, &guarded()),
            "skipped calls must not consume the once-per-kind slot"
        );
    }
}
