//! REQ-16/REQ-17 (Epic 1.2, ADR-001): subprocess-level tests for
//! `references/bin/antigravity-token-auth.py`, the exec credential helper
//! `GeminiProvider` will shell out to via `AuthMethod::Exec`
//! (`src/auth/exec.rs`). These invoke the real script as a child process —
//! matching validation.md's "Integration (subprocess)" designation for
//! REQ-16/REQ-17 — with `HOME` overridden to an isolated temp directory so
//! the tests never touch this machine's real
//! `~/.gemini/antigravity-cli/antigravity-oauth-token`.

use std::fs;
use std::process::Command;

use tempfile::TempDir;

/// Path to the script under test, resolved from the crate root so the test
/// doesn't depend on the current working directory.
fn script_path() -> String {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/references/bin/antigravity-token-auth.py"
    )
    .to_string()
}

/// Builds an isolated `$HOME` (a fresh `TempDir`) and, if `token_json` is
/// `Some`, writes it to `~/.gemini/antigravity-cli/antigravity-oauth-token`
/// (mirroring the real file's location). Returns the `TempDir` guard --
/// keep it alive for the duration of the test, since dropping it deletes
/// the directory.
#[allow(clippy::expect_used)] // test setup helper — a failure here is a broken test
                              // environment, not something under test
fn fake_home(token_json: Option<&str>) -> TempDir {
    let home = TempDir::new().expect("failed to create temp HOME dir");
    if let Some(json) = token_json {
        let dir = home.path().join(".gemini").join("antigravity-cli");
        fs::create_dir_all(&dir).expect("failed to create fake .gemini/antigravity-cli dir");
        fs::write(dir.join("antigravity-oauth-token"), json)
            .expect("failed to write fake token file");
    }
    home
}

fn run_script(home: &TempDir) -> std::process::Output {
    Command::new("python3")
        .arg(script_path())
        .env("HOME", home.path())
        .output()
        .unwrap_or_else(|e| panic!("failed to invoke antigravity-token-auth.py: {e}"))
}

#[test]
#[allow(clippy::expect_used)] // assertion-adjacent parsing of the subprocess's own output — a
                              // parse failure here is itself a test failure, not a setup bug
fn antigravity_token_auth_should_emit_single_json_line_with_headers_and_exit_zero_given_valid_token(
) {
    let token_json = r#"{"token":{"access_token":"ya29.abc123","token_type":"Bearer","refresh_token":"1//xyz","expiry":"2027-01-01T00:00:00Z"},"auth_method":"consumer"}"#;
    let home = fake_home(Some(token_json));

    let output = run_script(&home);

    assert!(
        output.status.success(),
        "expected exit 0, got {:?}; stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout was not valid UTF-8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "expected exactly one line of stdout, got: {stdout:?}"
    );

    let parsed: serde_json::Value =
        serde_json::from_str(lines[0]).expect("stdout line was not valid JSON");
    let headers = parsed
        .get("headers")
        .expect("response missing \"headers\" key")
        .as_object()
        .expect("\"headers\" was not a JSON object");

    assert_eq!(
        headers.get("Authorization").and_then(|v| v.as_str()),
        Some("Bearer ya29.abc123")
    );
    assert_eq!(
        headers.get("X-Goog-Api-Client").and_then(|v| v.as_str()),
        Some("google-cloud-sdk vscode_cloudshelleditor/0.1")
    );
    let client_metadata_raw = headers
        .get("Client-Metadata")
        .and_then(|v| v.as_str())
        .expect("\"Client-Metadata\" header missing or not a string");
    let client_metadata: serde_json::Value =
        serde_json::from_str(client_metadata_raw).expect("Client-Metadata was not valid JSON");
    assert_eq!(client_metadata["ideType"], "ANTIGRAVITY");
    assert_eq!(client_metadata["platform"], "LINUX");
    assert_eq!(client_metadata["pluginType"], "GEMINI");
}

#[test]
fn antigravity_token_auth_should_exit_nonzero_with_no_stdout_when_token_file_missing() {
    let home = fake_home(None);

    let output = run_script(&home);

    assert!(
        !output.status.success(),
        "expected non-zero exit when token file is missing"
    );
    assert!(
        output.stdout.is_empty(),
        "expected zero stdout on failure, got: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
#[allow(clippy::expect_used)] // assertion-adjacent parsing of the subprocess's own output — a
                              // parse failure here is itself a test failure, not a setup bug
fn antigravity_token_auth_should_exit_nonzero_with_actionable_remediation_message_when_token_expired(
) {
    let expiry = "2026-08-20T00:00:00Z";
    let token_json = format!(
        r#"{{"token":{{"access_token":"ya29.abc123","token_type":"Bearer","refresh_token":"1//xyz","expiry":"{expiry}"}},"auth_method":"consumer"}}"#
    );
    let home = fake_home(Some(&token_json));

    let output = run_script(&home);

    assert!(
        !output.status.success(),
        "expected non-zero exit for an expired token"
    );
    assert!(
        output.stdout.is_empty(),
        "expected zero stdout on failure, got: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr was not valid UTF-8");
    assert!(
        stderr.contains(&format!("antigravity-cli token expired at {expiry}")),
        "stderr should name the expiry timestamp; got: {stderr:?}"
    );
    assert!(
        stderr.contains("antigravity-cli login"),
        "stderr should name the 'antigravity-cli login' remediation; got: {stderr:?}"
    );
    assert!(
        stderr.contains("reopen the Antigravity IDE"),
        "stderr should also name reopening the Antigravity IDE; got: {stderr:?}"
    );
}
