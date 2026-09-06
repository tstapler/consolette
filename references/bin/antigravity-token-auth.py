#!/usr/bin/env python3
"""Exec credential helper (ADR-007 sec 2) for the Gemini/Cloud Code Assist
upstream (ADR-001).

Reads the Antigravity IDE's plain-JSON OAuth token file, checks its expiry,
and emits the headers `GeminiProvider` needs on stdout as one JSON line.
Stdlib-only, no dependencies of any kind (Rust or Python) per ADR-001.

Contract (ADR-007 sec 2/sec 6): on success, print exactly one JSON line of
the form `{"headers": {...}}` to stdout and exit 0. On any failure, print a
human-readable message to stderr (for manual/debug use only -- consolette's
`run_helper` never surfaces helper stdout/stderr into its own logs or error
text) and exit non-zero with no stdout at all.
"""

import json
import re
import sys
from datetime import datetime, timezone
from pathlib import Path

TOKEN_PATH = Path.home() / ".gemini" / "antigravity-cli" / "antigravity-oauth-token"


def _read_stdin_request_context() -> None:
    """Read and discard the ADR-007 stdin request context.

    The helper doesn't need the request's upstream/method/url -- the same
    headers apply to every Cloud Code Assist call -- but stdin must still be
    drained so the parent process's write doesn't block.
    """
    sys.stdin.read()


def main() -> int:
    _read_stdin_request_context()

    try:
        raw = TOKEN_PATH.read_text(encoding="utf-8")
        data = json.loads(raw)
        token = data["token"]
        access_token = token["access_token"]
        expiry = token["expiry"]
    except FileNotFoundError as exc:
        print(
            f"antigravity-token-auth: no token file at {TOKEN_PATH} — run "
            f"'antigravity-cli login' (or reopen the Antigravity IDE): {exc}",
            file=sys.stderr,
        )
        return 1
    except (json.JSONDecodeError, KeyError, TypeError) as exc:
        print(
            f"antigravity-token-auth: malformed token file at {TOKEN_PATH}: {exc}",
            file=sys.stderr,
        )
        return 1
    except OSError as exc:
        print(
            f"antigravity-token-auth: cannot read token file at {TOKEN_PATH}: {exc}",
            file=sys.stderr,
        )
        return 1

    # datetime.fromisoformat only accepts "+00:00"-style offsets (not a
    # trailing "Z") and, on Python < 3.11, only 0/3/6-digit fractional
    # seconds -- normalize both so this runs on older interpreters and
    # against the real Antigravity token file's actual precision (observed:
    # 9-digit/nanosecond fractions, e.g. "...493304002-07:00").
    expiry_iso = expiry[:-1] + "+00:00" if expiry.endswith("Z") else expiry
    expiry_iso = re.sub(r"(\.\d{6})\d+", r"\1", expiry_iso)
    try:
        expiry_dt = datetime.fromisoformat(expiry_iso)
    except ValueError as exc:
        print(
            f"antigravity-token-auth: unparseable expiry {expiry!r}: {exc}",
            file=sys.stderr,
        )
        return 1

    if expiry_dt <= datetime.now(timezone.utc):
        print(
            f"antigravity-cli token expired at {expiry} — run 'antigravity-cli login' "
            "(or reopen the Antigravity IDE) to mint a fresh token",
            file=sys.stderr,
        )
        return 1

    client_metadata = json.dumps(
        {"ideType": "ANTIGRAVITY", "platform": "LINUX", "pluginType": "GEMINI"},
        separators=(",", ":"),
    )
    headers = {
        "Authorization": f"Bearer {access_token}",
        "X-Goog-Api-Client": "google-cloud-sdk vscode_cloudshelleditor/0.1",
        "Client-Metadata": client_metadata,
    }
    # Compact separators to match the exact byte-for-byte contract in
    # plan.md's acceptance criteria (no spaces after ":"/",").
    print(json.dumps({"headers": headers}, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    sys.exit(main())
