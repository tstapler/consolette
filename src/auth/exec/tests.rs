#![allow(clippy::unwrap_used)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use super::*;

const UPSTREAM: &str = "test-upstream";
const METHOD: &str = "POST";
const URL: &str = "https://example.invalid/v1/messages";

fn write_helper(dir: &tempfile::TempDir, name: &str, script: &str) -> PathBuf {
    let path = dir.path().join(name);
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(script.as_bytes()).unwrap();
    drop(file);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[tokio::test]
async fn successful_helper_returns_headers() {
    let dir = tempfile::tempdir().unwrap();
    let helper = write_helper(
        &dir,
        "helper.sh",
        r#"#!/bin/sh
cat >/dev/null
echo '{"headers":{"X-Test":"value1"}}'
"#,
    );

    let (headers, ttl) = run_helper(
        UPSTREAM,
        &helper.to_string_lossy(),
        &[],
        Duration::from_secs(5),
        METHOD,
        URL,
    )
    .await
    .unwrap();

    assert_eq!(headers.get("X-Test").unwrap(), "value1");
    assert_eq!(ttl, None);
}

#[tokio::test]
async fn helper_can_override_cache_ttl() {
    let dir = tempfile::tempdir().unwrap();
    let helper = write_helper(
        &dir,
        "helper.sh",
        r#"#!/bin/sh
cat >/dev/null
echo '{"headers":{"X-Test":"value1"},"cache_ttl_secs":42}'
"#,
    );

    let (_, ttl) = run_helper(
        UPSTREAM,
        &helper.to_string_lossy(),
        &[],
        Duration::from_secs(5),
        METHOD,
        URL,
    )
    .await
    .unwrap();

    assert_eq!(ttl, Some(42));
}

#[tokio::test]
async fn non_zero_exit_is_exec_error() {
    let dir = tempfile::tempdir().unwrap();
    let helper = write_helper(
        &dir,
        "helper.sh",
        r#"#!/bin/sh
cat >/dev/null
echo "secret leak attempt" >&2
exit 1
"#,
    );

    let result = run_helper(
        UPSTREAM,
        &helper.to_string_lossy(),
        &[],
        Duration::from_secs(5),
        METHOD,
        URL,
    )
    .await;

    let err = result.unwrap_err();
    assert!(matches!(err, AuthError::Exec(_)));
    assert!(!err.to_string().contains("secret leak attempt"));
}

#[tokio::test]
async fn unparseable_stdout_is_exec_error() {
    let dir = tempfile::tempdir().unwrap();
    let helper = write_helper(
        &dir,
        "helper.sh",
        "#!/bin/sh
cat >/dev/null
echo 'not json'
",
    );

    let result = run_helper(
        UPSTREAM,
        &helper.to_string_lossy(),
        &[],
        Duration::from_secs(5),
        METHOD,
        URL,
    )
    .await;

    assert!(matches!(result, Err(AuthError::Exec(_))));
}

#[tokio::test]
async fn slow_helper_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let helper = write_helper(
        &dir,
        "helper.sh",
        r#"#!/bin/sh
cat >/dev/null
sleep 5
echo '{"headers":{}}'
"#,
    );

    let result = run_helper(
        UPSTREAM,
        &helper.to_string_lossy(),
        &[],
        Duration::from_millis(50),
        METHOD,
        URL,
    )
    .await;

    assert!(matches!(result, Err(AuthError::Exec(_))));
}

#[tokio::test]
async fn world_writable_helper_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let helper = write_helper(
        &dir,
        "helper.sh",
        r#"#!/bin/sh
cat >/dev/null
echo '{"headers":{}}'
"#,
    );
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o707)).unwrap();

    let result = run_helper(
        UPSTREAM,
        &helper.to_string_lossy(),
        &[],
        Duration::from_secs(5),
        METHOD,
        URL,
    )
    .await;

    assert!(matches!(result, Err(AuthError::Exec(_))));
}

#[tokio::test]
async fn missing_command_is_exec_error() {
    let result = run_helper(
        UPSTREAM,
        "consolette-definitely-not-a-real-command",
        &[],
        Duration::from_secs(5),
        METHOD,
        URL,
    )
    .await;

    assert!(matches!(result, Err(AuthError::Exec(_))));
}

#[tokio::test]
async fn cache_avoids_second_subprocess_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let counter_file = dir.path().join("count");
    std::fs::write(&counter_file, "0").unwrap();
    let helper = write_helper(
        &dir,
        "helper.sh",
        &format!(
            r#"#!/bin/sh
cat >/dev/null
n=$(cat "{path}")
n=$((n + 1))
echo "$n" > "{path}"
echo '{{"headers":{{"X-Count":"'"$n"'"}}}}'
"#,
            path = counter_file.display()
        ),
    );

    let cache = ExecCredentialCache::new();
    let first = cache
        .get_or_run(
            UPSTREAM,
            &helper.to_string_lossy(),
            &[],
            Duration::from_mins(5),
            Duration::from_secs(5),
            METHOD,
            URL,
        )
        .await
        .unwrap();
    let second = cache
        .get_or_run(
            UPSTREAM,
            &helper.to_string_lossy(),
            &[],
            Duration::from_mins(5),
            Duration::from_secs(5),
            METHOD,
            URL,
        )
        .await
        .unwrap();

    assert_eq!(
        first.get("X-Count").unwrap(),
        second.get("X-Count").unwrap()
    );
    assert_eq!(std::fs::read_to_string(&counter_file).unwrap().trim(), "1");
}

#[tokio::test]
async fn cache_clear_forces_a_fresh_run() {
    let dir = tempfile::tempdir().unwrap();
    let counter_file = dir.path().join("count");
    std::fs::write(&counter_file, "0").unwrap();
    let helper = write_helper(
        &dir,
        "helper.sh",
        &format!(
            r#"#!/bin/sh
cat >/dev/null
n=$(cat "{path}")
n=$((n + 1))
echo "$n" > "{path}"
echo '{{"headers":{{"X-Count":"'"$n"'"}}}}'
"#,
            path = counter_file.display()
        ),
    );

    let cache = ExecCredentialCache::new();
    let first = cache
        .get_or_run(
            UPSTREAM,
            &helper.to_string_lossy(),
            &[],
            Duration::from_mins(5),
            Duration::from_secs(5),
            METHOD,
            URL,
        )
        .await
        .unwrap();
    cache.clear();
    let second = cache
        .get_or_run(
            UPSTREAM,
            &helper.to_string_lossy(),
            &[],
            Duration::from_mins(5),
            Duration::from_secs(5),
            METHOD,
            URL,
        )
        .await
        .unwrap();

    assert_ne!(
        first.get("X-Count").unwrap(),
        second.get("X-Count").unwrap()
    );
}

#[test]
fn resolve_command_finds_absolute_path() {
    let path = resolve_command("/bin/sh").unwrap();
    assert_eq!(path, PathBuf::from("/bin/sh"));
}

#[test]
fn resolve_command_searches_path_for_bare_name() {
    let path = resolve_command("sh").unwrap();
    assert!(path.is_file());
}

#[test]
fn resolve_command_errors_when_not_found() {
    let result = resolve_command("consolette-definitely-not-a-real-command");
    assert!(matches!(result, Err(AuthError::Exec(_))));
}
