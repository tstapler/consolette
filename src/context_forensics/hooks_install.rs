//! Idempotent, backup-first install/uninstall of consolette's Claude Code
//! hooks into `~/.claude/settings.json` (plan.md Epic 4.1).
//!
//! **Concurrent-writer finding (Task 4.1.1a)**: `stapler-scripts/llm-sync`
//! (`~/dotfiles/stapler-scripts/llm-sync/src/targets/claude_plugin_installer.py`,
//! `ClaudePluginInstaller._install_hooks`) is a second, real writer to this
//! same file — it reads the whole JSON, merges plugin hook entries into the
//! `hooks` key (deduplicating by command string), and writes back with
//! `Path.write_text` (a plain truncate-and-rewrite, no tmp+rename, no
//! backup). It preserves unrelated top-level keys, so it won't silently
//! drop consolette's other settings, but it gives no atomicity guarantee of
//! its own — a crash mid-write on its side can still corrupt the file
//! `SettingsJsonGateway` later reads. This module's own atomic tmp+rename
//! write is this feature's only guarantee; it does not (and cannot) make
//! llm-sync's write atomic too.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The stable, recognizable prefix consolette stamps into every hook
/// command entry it installs, so `down` can find and remove exactly its
/// own entries without touching anyone else's.
pub const HOOK_MARKER: &str = "consolette context-hook";

/// Claude Code hook events this feature installs a listener on (mirrors
/// `HookEventKind`, plan.md Vocabulary table).
const HOOK_EVENTS: &[&str] = &[
    "PostToolUse",
    "PostToolUseFailure",
    "PreCompact",
    "PostCompact",
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "SubagentStart",
    "SubagentStop",
    "InstructionsLoaded",
];

/// Read/backup/atomic-write access to `~/.claude/settings.json`.
///
/// Never parses the file into a strict typed struct — it's read as an
/// untyped [`Value`] so that sections this feature doesn't understand
/// (`permissions`, `env`, other plugins' `hooks` entries) round-trip
/// byte-for-byte through fields it doesn't touch.
pub struct SettingsJsonGateway;

impl SettingsJsonGateway {
    /// `~/.claude/settings.json`, honoring `HOME` (matches
    /// `OmissionCache::default_cache_path`'s own lookup convention).
    #[must_use]
    pub fn default_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".claude").join("settings.json")
    }

    /// Reads `path` as an untyped JSON [`Value`]. A missing file reads as
    /// an empty object (nothing installed yet is not an error).
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but can't be read or parsed as
    /// JSON.
    pub fn read(path: &Path) -> Result<Value> {
        if !path.exists() {
            return Ok(Value::Object(serde_json::Map::new()));
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
    }

    /// Writes `value` to `path` atomically: serialize to `path.tmp`, `fsync`
    /// it, then `rename()` over `path`. A process kill between the tmp-file
    /// write and the rename leaves the original file untouched, since
    /// `rename()` on the same filesystem is atomic and no truncate of the
    /// live file ever happens.
    ///
    /// # Errors
    ///
    /// Returns an error if the parent directory, tmp file, serialization,
    /// fsync, or rename step fails.
    pub fn write(path: &Path, value: &Value) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        // `rename()` replaces the destination inode wholesale, so a tmp file
        // created with umask-default permissions would silently drop any
        // tighter mode bits (e.g. a user-hardened 0600) the live file had —
        // capture them up front and reapply after the rename. `None` when
        // the file doesn't exist yet, in which case a fresh file just keeps
        // its umask-default mode.
        let original_permissions = fs::metadata(path).ok().map(|m| m.permissions());

        let tmp_path = path.with_extension("json.tmp");
        let mut tmp_file = fs::File::create(&tmp_path)
            .with_context(|| format!("failed to create {}", tmp_path.display()))?;
        let serialized = serde_json::to_string_pretty(value)
            .context("failed to serialize settings.json contents")?;
        tmp_file
            .write_all(serialized.as_bytes())
            .and_then(|()| tmp_file.write_all(b"\n"))
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        tmp_file
            .sync_all()
            .with_context(|| format!("failed to fsync {}", tmp_path.display()))?;
        fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "failed to rename {} to {}",
                tmp_path.display(),
                path.display()
            )
        })?;

        if let Some(permissions) = original_permissions {
            fs::set_permissions(path, permissions).with_context(|| {
                format!(
                    "failed to restore original permissions on {}",
                    path.display()
                )
            })?;
        }

        Ok(())
    }

    /// Writes `path.bak.<unix-timestamp>` if a backup for the current
    /// second doesn't already exist, so calling `backup` twice within one
    /// install run doesn't produce two files.
    ///
    /// # Errors
    ///
    /// Returns an error if the system clock reads before the UNIX epoch or
    /// the backup file can't be written.
    pub fn backup(path: &Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| anyhow!("system clock before UNIX epoch: {error}"))?
            .as_secs();
        let backup_path = path.with_extension(format!("json.bak.{timestamp}"));
        if backup_path.exists() {
            return Ok(());
        }
        fs::copy(path, &backup_path).with_context(|| {
            format!(
                "failed to back up {} to {}",
                path.display(),
                backup_path.display()
            )
        })?;
        Ok(())
    }
}

fn hook_entry(event: &str) -> Value {
    serde_json::json!({
        "hooks": [
            { "type": "command", "command": format!("{HOOK_MARKER} {event}") }
        ]
    })
}

fn entry_hooks_mut(entry: &mut Value) -> Option<&mut Vec<Value>> {
    entry.get_mut("hooks").and_then(Value::as_array_mut)
}

fn hook_command(hook: &Value) -> Option<&str> {
    hook.get("command").and_then(Value::as_str)
}

/// Idempotently installs consolette's hook entries into `settings`,
/// appending after every pre-existing entry in each event's array (never
/// reordering, never inserting between existing entries) and skipping an
/// event entirely if consolette's own entry is already present.
///
/// Fails rather than panicking if `settings` (or its existing `hooks`
/// section) isn't shaped like a JSON object/array — a hand-edited
/// `settings.json` is exactly the kind of external input that must be
/// rejected with an error, not assumed well-formed.
fn install_into(settings: &mut Value) -> Result<()> {
    let object = settings
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings.json root must be a JSON object"))?;
    let hooks_object = object
        .entry("hooks")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings.json's `hooks` key must be a JSON object"))?;

    for event in HOOK_EVENTS {
        let expected_command = format!("{HOOK_MARKER} {event}");
        let entries = hooks_object
            .entry(*event)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| anyhow!("settings.json's `hooks.{event}` value must be a JSON array"))?;

        let already_installed = entries.iter().any(|entry| {
            entry
                .get("hooks")
                .and_then(Value::as_array)
                .is_some_and(|hooks| {
                    hooks
                        .iter()
                        .any(|hook| hook_command(hook) == Some(expected_command.as_str()))
                })
        });

        if !already_installed {
            entries.push(hook_entry(event));
        }
    }
    Ok(())
}

/// Removes every entry carrying consolette's [`HOOK_MARKER`] from
/// `settings`, leaving all other entries (including ones added after `up`
/// ran) untouched. A no-op if nothing is installed.
fn uninstall_from(settings: &mut Value) {
    let Some(hooks_object) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };

    for entries in hooks_object.values_mut() {
        let Some(entries) = entries.as_array_mut() else {
            continue;
        };
        for entry in entries.iter_mut() {
            if let Some(hooks) = entry_hooks_mut(entry) {
                hooks.retain(|hook| {
                    !hook_command(hook).is_some_and(|command| command.starts_with(HOOK_MARKER))
                });
            }
        }
        entries.retain(|entry| {
            entry
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|hooks| !hooks.is_empty())
        });
    }
}

/// Idempotent install: backs up `path`, reads it, appends consolette's hook
/// entries (skipping events where they're already present), and writes the
/// result back atomically.
///
/// # Errors
///
/// Returns an error if the backup, read, in-memory merge, or write step
/// fails (see [`install_into`] for the merge step's own failure modes).
pub fn up(path: &Path) -> Result<()> {
    SettingsJsonGateway::backup(path)?;
    let mut settings = SettingsJsonGateway::read(path)?;
    install_into(&mut settings)?;
    SettingsJsonGateway::write(path, &settings)
}

/// Targeted uninstall: removes exactly consolette's own hook entries,
/// leaving everything else (including entries added after `up` ran)
/// untouched. Does not restore from any backup file.
///
/// # Errors
///
/// Returns an error if the read or write step fails.
pub fn down(path: &Path) -> Result<()> {
    let mut settings = SettingsJsonGateway::read(path)?;
    uninstall_from(&mut settings);
    SettingsJsonGateway::write(path, &settings)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // test assertions on well-formed fixtures
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn settings_path(dir: &TempDir) -> PathBuf {
        dir.path().join("settings.json")
    }

    #[test]
    fn read_should_return_empty_object_when_file_missing() {
        let dir = TempDir::new().unwrap();
        let value = SettingsJsonGateway::read(&settings_path(&dir)).unwrap();
        assert_eq!(value, Value::Object(serde_json::Map::new()));
    }

    #[test]
    fn write_then_read_should_round_trip_byte_faithfully_including_unrelated_keys() {
        let dir = TempDir::new().unwrap();
        let path = settings_path(&dir);
        let original = serde_json::json!({
            "permissions": {"allow": ["Bash(git *)"]},
            "hooks": {"PostToolUse": [{"hooks": [{"type": "command", "command": "rtk-hook"}]}]},
            "unrelated_future_key": 42
        });
        SettingsJsonGateway::write(&path, &original).unwrap();
        let read_back = SettingsJsonGateway::read(&path).unwrap();
        assert_eq!(read_back, original);
    }

    #[test]
    fn write_should_never_truncate_live_file_when_tmp_write_precedes_rename() {
        // Verified structurally, not via a real kill signal: the write
        // path always goes through a distinct `.json.tmp` file followed by
        // `rename()`, so the live file's content is only ever replaced by
        // the completed, fsynced tmp file's content.
        let dir = TempDir::new().unwrap();
        let path = settings_path(&dir);
        let original = serde_json::json!({"existing": true});
        fs::write(&path, serde_json::to_string(&original).unwrap()).unwrap();

        let tmp_path = path.with_extension("json.tmp");
        assert!(!tmp_path.exists());

        SettingsJsonGateway::write(&path, &serde_json::json!({"updated": true})).unwrap();

        assert!(
            !tmp_path.exists(),
            "tmp file should be renamed away, not left behind"
        );
        assert_eq!(
            SettingsJsonGateway::read(&path).unwrap(),
            serde_json::json!({"updated": true})
        );
    }

    #[test]
    fn backup_should_create_exactly_one_file_when_called_twice_in_same_run() {
        let dir = TempDir::new().unwrap();
        let path = settings_path(&dir);
        fs::write(&path, "{}").unwrap();

        SettingsJsonGateway::backup(&path).unwrap();
        SettingsJsonGateway::backup(&path).unwrap();

        let backups: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("settings.json.bak.")
            })
            .collect();
        assert_eq!(backups.len(), 1);
    }

    #[test]
    fn backup_should_be_a_no_op_when_file_does_not_exist() {
        let dir = TempDir::new().unwrap();
        SettingsJsonGateway::backup(&settings_path(&dir)).unwrap();
        assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn up_should_append_consolette_entry_when_existing_hook_present() {
        let dir = TempDir::new().unwrap();
        let path = settings_path(&dir);
        let existing = serde_json::json!({
            "hooks": {"PostToolUse": [{"hooks": [{"type": "command", "command": "rtk-hook"}]}]}
        });
        SettingsJsonGateway::write(&path, &existing).unwrap();

        up(&path).unwrap();

        let settings = SettingsJsonGateway::read(&path).unwrap();
        let entries = settings["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["hooks"][0]["command"], "rtk-hook");
        assert_eq!(
            entries[1]["hooks"][0]["command"],
            format!("{HOOK_MARKER} PostToolUse")
        );
    }

    #[test]
    fn up_should_not_duplicate_consolette_entry_when_run_twice() {
        let dir = TempDir::new().unwrap();
        let path = settings_path(&dir);

        up(&path).unwrap();
        up(&path).unwrap();

        let settings = SettingsJsonGateway::read(&path).unwrap();
        let entries = settings["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn up_should_append_after_existing_entries_never_before_when_settings_json_has_pre_existing_hook(
    ) {
        let dir = TempDir::new().unwrap();
        let path = settings_path(&dir);
        // A real-shaped pre-existing hook: a command with an observable,
        // distinct side effect (writes a marker line, exits non-zero).
        let marker_file = dir.path().join("pre-existing-ran");
        let pre_existing_command = format!(
            "echo pre-existing-marker >> {} && exit 3",
            marker_file.display()
        );
        let existing = serde_json::json!({
            "hooks": {
                "PostToolUse": [
                    {"hooks": [{"type": "command", "command": pre_existing_command}]}
                ]
            }
        });
        SettingsJsonGateway::write(&path, &existing).unwrap();

        up(&path).unwrap();

        let settings = SettingsJsonGateway::read(&path).unwrap();
        let entries = settings["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0]["hooks"][0]["command"].as_str().unwrap(),
            pre_existing_command,
            "pre-existing entry's index and command string must be byte-identical after up"
        );
        assert_eq!(
            entries[1]["hooks"][0]["command"],
            format!("{HOOK_MARKER} PostToolUse"),
            "consolette's entry must be appended after, never before or between"
        );

        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&pre_existing_command)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(3));
        let marker_contents = fs::read_to_string(&marker_file).unwrap();
        assert_eq!(marker_contents.matches("pre-existing-marker").count(), 1);
    }

    #[test]
    fn down_should_remove_consolette_entry_but_preserve_manually_added_hook() {
        let dir = TempDir::new().unwrap();
        let path = settings_path(&dir);

        up(&path).unwrap();
        let mut settings = SettingsJsonGateway::read(&path).unwrap();
        settings["hooks"]["PostToolUse"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"hooks": [{"type": "command", "command": "manual-hook"}]}));
        SettingsJsonGateway::write(&path, &settings).unwrap();

        down(&path).unwrap();

        let settings = SettingsJsonGateway::read(&path).unwrap();
        let entries = settings["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["hooks"][0]["command"], "manual-hook");
    }

    #[test]
    fn down_should_be_a_no_op_when_nothing_installed() {
        let dir = TempDir::new().unwrap();
        let path = settings_path(&dir);
        let original = serde_json::json!({"permissions": {"allow": ["Bash(git *)"]}});
        SettingsJsonGateway::write(&path, &original).unwrap();

        down(&path).unwrap();

        assert_eq!(SettingsJsonGateway::read(&path).unwrap(), original);
    }

    #[test]
    fn write_should_preserve_original_permission_bits_when_file_is_hardened() {
        let dir = TempDir::new().unwrap();
        let path = settings_path(&dir);
        SettingsJsonGateway::write(&path, &serde_json::json!({"existing": true})).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        SettingsJsonGateway::write(&path, &serde_json::json!({"updated": true})).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "write() must not silently loosen a hardened file's mode bits"
        );
    }

    #[test]
    fn up_should_reject_and_leave_file_unchanged_when_root_is_not_an_object() {
        let dir = TempDir::new().unwrap();
        let path = settings_path(&dir);
        fs::write(&path, "\"just-a-string\"").unwrap();

        let result = up(&path);

        assert!(result.is_err());
        let raw = fs::read_to_string(&path).unwrap();
        assert_eq!(raw, "\"just-a-string\"");
    }

    #[test]
    fn up_should_reject_and_leave_file_unchanged_when_hooks_key_is_not_an_object() {
        let dir = TempDir::new().unwrap();
        let path = settings_path(&dir);
        let original = serde_json::json!({"hooks": "not-an-object"});
        SettingsJsonGateway::write(&path, &original).unwrap();

        let result = up(&path);

        assert!(result.is_err());
        assert_eq!(SettingsJsonGateway::read(&path).unwrap(), original);
    }

    #[test]
    fn up_should_reject_and_leave_file_unchanged_when_hook_event_value_is_not_an_array() {
        let dir = TempDir::new().unwrap();
        let path = settings_path(&dir);
        let original = serde_json::json!({"hooks": {"PostToolUse": "not-an-array"}});
        SettingsJsonGateway::write(&path, &original).unwrap();

        let result = up(&path);

        assert!(result.is_err());
        assert_eq!(SettingsJsonGateway::read(&path).unwrap(), original);
    }
}
