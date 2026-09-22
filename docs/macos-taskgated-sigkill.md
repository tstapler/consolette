# macOS SIGKILLs locally-built binaries: `Taskgated Invalid Signature`

## Symptom

A `cargo build`/`cargo install`-produced `consolette` binary gets killed the
moment it's executed:

```
[1]    12345 killed     consolette --version
```

`echo $?` reports 137 (128 + SIGKILL). The macOS crash report written to
`~/Library/Logs/DiagnosticReports/consolette-*.ips` shows:

```json
"exception": {"type": "EXC_CRASH", "signal": "SIGKILL (Code Signature Invalid)"},
"termination": {"namespace": "CODESIGNING", "indicator": "Taskgated Invalid Signature"}
```

## Root cause

This is **not** CrowdStrike Falcon or any other EDR product — it's macOS's
own `taskgated`/AMFI code-signing enforcement, and it's a widely reported
macOS 26 ("Tahoe") behavior, not specific to this project. Other tools have
hit the identical crash signature:

- [astral-sh/uv#16726](https://github.com/astral-sh/uv/issues/16726) — `uv`'s Python installs SIGKilled the same way
- [1jehuang/jcode#1233](https://github.com/1jehuang/jcode/issues/1233) — `com.apple.provenance` xattr blocking ad-hoc-signed launchers
- [anthropics/claude-code#28903](https://github.com/anthropics/claude-code/issues/28903) — Claude Code itself hitting `Taskgated Invalid Signature`
- [astra.pizza: "com.apple.provenance: the xattr you can't remove"](https://astra.pizza/posts/2026-03-14-provenance-xattr/) — background on the xattr itself
- [Apple Developer Forums thread on `com.apple.provenance`](https://developer.apple.com/forums/thread/723397)

The mechanism: macOS tags files written by certain toolchains (`cargo`,
`rustc`, and others) with a kernel-managed `com.apple.provenance` extended
attribute. Combined with an ad-hoc or self-signed code signature, `taskgated`
caches a negative trust verdict for that specific file/path the first time
it's executed — and every subsequent exec of that same file gets SIGKilled,
even if the file's *content* is later replaced. A byte-identical binary
copied to a fresh filename runs fine, because it never accumulated a bad
verdict.

`xattr -d com.apple.provenance <file>` exits 0 and looks like it worked, but
is a silent no-op — the attribute is kernel-managed and removal is denied
without any error, confirmed independently by multiple of the reports above.

## Fix

Force a fresh ad-hoc re-sign of the binary. This makes `taskgated` recompute
the trust verdict instead of reusing the cached bad one — no Developer ID
certificate or notarization required:

```sh
codesign --force --deep --sign - /path/to/consolette
```

`consolette install` (`src/service.rs`) does this automatically to the
resolved binary before writing the LaunchAgent plist, so a fresh
`cargo install`/`cargo build --release` followed by `consolette install`
self-heals without manual intervention.

If you hit this outside of `consolette install` (e.g. running a freshly
built binary directly), apply the same `codesign` command to that exact path
before retrying.
