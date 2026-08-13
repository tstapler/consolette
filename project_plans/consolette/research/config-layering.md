# Consolette: Layered TOML config from `~/.config/consolette/conf.d/*.toml`

Research question: how should Consolette load layered TOML configuration
(`defaults < conf.d/*.toml (lexical, deep-merged) < env vars`) into serde
structs, with a schema that a future Python `tomllib` port can read unchanged?

---

## Recommendation (TL;DR)

**Use `figment`** (v0.10.19) as the config-layering engine, with the `toml`
and `env` features. It maps the required precedence chain
`defaults < conf.d files < env` directly onto its provider model, and its
per-value **source metadata** is the single best fit for the "fail-fast, name
the offending file + key" requirement (question 4) — that is figment's headline
feature, not an add-on.

Concretely:

```
Figment::new()
    .merge(Serialized::defaults(Config::default()))   // layer 0: defaults
    .merge(Toml::file(sorted_conf_d_file[0]))          // layer 1..N: conf.d
    .merge(Toml::file(sorted_conf_d_file[1]))          //   (lexically sorted)
    ...
    .merge(Env::prefixed("CONSOLETTE_").split("__"))   // layer N+1: env
    .extract::<Config>()
```

- `.merge()` gives exactly the required deep-merge semantics out of the box:
  **tables union recursively, scalars replace, arrays replace** (later wins).
- The on-disk shape is plain TOML with no serde renames or figment-only tricks,
  so Python `tomllib` reads the same files unchanged. Env overlay lives only in
  the process environment, not on disk, so it never touches the TOML schema.
- You must **sort the glob results yourself** — this is true for `figment` and
  `config` alike (see Q1 gotcha). The repo already depends on `glob = "0.3"`.

**Runner-up:** the `config` crate (v0.15.25, actively maintained in 2026) is a
perfectly viable alternative and has slightly more turnkey env-list parsing. It
loses on error precision (weaker at naming the specific file/key) and its
layering model is less explicit than figment's `defaults/merge/env` triple.
Choose it only if you value 2026-active maintenance over error quality;
figment's error metadata is the deciding factor here.

**Scope env overrides narrowly.** Overlaying env vars onto nested tables works,
but onto **arrays of tables** (`[[upstreams]]`, `[[routes]]`) it is not
ergonomic in either crate. Recommend: env overrides only a small allowlist of
top-level scalars (port, tokens, log level, timeouts). Keep upstreams/routes
file-only.

**Hot-reload: defer.** Ship startup-load first. `notify` + `ArcSwap<Config>` is
the right pattern when you add it, but it is not needed for v1 and adds real
correctness surface (debounce, partial-write races, validation-on-reload).

---

## Crate versions (crates.io, checked 2026-07-17)

| Crate | Latest | Released | Notes |
|---|---|---|---|
| `figment` | **0.10.19** | 2024-05-17 | Latest stable. No release since 2024 but stable and widely used (Rocket's config engine). Features: `toml`, `env`, `json`, `yaml`, `parse-value`, `test`. |
| `config` | **0.15.25** | 2026-06-26 | Actively maintained (maintained by Ed Page). |
| `notify` | **8.2.0** | 2025-08-03 | Latest *stable* (9.0.0 is still `-rc.4`, 2026-05). Use 8.x for now. |
| `arc-swap` | **1.9.2** | 2026-06-28 | For lock-free config swap on hot-reload. |
| `toml` | 0.8 | (already in repo) | Underlies figment's `Toml` provider anyway. |
| `glob` | 0.3 | (already in repo) | Reused for directory enumeration. |

Repo already depends on: `toml = "0.8"`, `serde` (derive), `serde_json`,
`glob = "0.3"`, `anyhow`, `thiserror`, `once_cell`, `dashmap`, `clap`. So adding
figment pulls very few *new* transitive deps (it re-uses `toml`/`serde`).

Suggested `Cargo.toml` addition:

```toml
figment = { version = "0.10", default-features = false, features = ["toml", "env"] }
```

(Drop the default `yaml`/`json` features you don't need.)

---

## Q1. figment vs config: dir globbing, deep-merge, env overlay, nested keys

### (a) Globbing a directory of TOML in lexical order

**Neither crate globs a directory for you in guaranteed lexical order.** The
`glob` crate returns paths in filesystem-iteration order, which is **not
guaranteed sorted**. You must collect and `sort()` explicitly in both cases.
This is the single most important cross-cutting gotcha.

figment sketch (recommended):

```rust
use figment::{Figment, providers::{Format, Toml, Env, Serialized}};
use std::path::PathBuf;

fn conf_d_files(dir: &std::path::Path) -> anyhow::Result<Vec<PathBuf>> {
    let pattern = dir.join("*.toml");
    let mut files: Vec<PathBuf> = glob::glob(pattern.to_str().unwrap())?
        .filter_map(Result::ok)
        .collect();
    files.sort();                       // <-- lexical filename order, REQUIRED
    Ok(files)
}

fn load(conf_d: &std::path::Path) -> anyhow::Result<Config> {
    let mut fig = Figment::new()
        .merge(Serialized::defaults(Config::default()));   // defaults layer
    for f in conf_d_files(conf_d)? {
        fig = fig.merge(Toml::file(f));                     // each file, in order
    }
    fig = fig.merge(Env::prefixed("CONSOLETTE_").split("__")); // env last
    Ok(fig.extract()?)                                     // deserialize + validate
}
```

`config` crate equivalent:

```rust
use config::{Config as ConfigBuilder, File, FileFormat, Environment};

let mut builder = ConfigBuilder::builder()
    .add_source(File::from_str(DEFAULT_TOML, FileFormat::Toml)); // or set_default per key
let mut files: Vec<_> = glob::glob(&pattern)?.filter_map(Result::ok).collect();
files.sort();                                                    // same requirement
for f in files {
    builder = builder.add_source(File::from(f).format(FileFormat::Toml).required(true));
}
let settings: Config = builder
    .add_source(Environment::with_prefix("CONSOLETTE").separator("__"))
    .build()?
    .try_deserialize()?;
```

Both are ~equal in ergonomics here. figment's `Serialized::defaults(Config::default())`
is a cleaner defaults layer than the `config` crate's `set_default`-per-key or
embedded-string approach, and keeps defaults in Rust (type-checked) rather than a
TOML blob.

### (b) Deep-merging tables

- **figment**: nested tables are **always unioned recursively**, regardless of
  strategy. `.merge()` = later provider wins on scalar conflicts. This is exactly
  "tables merge, scalars replace." Confirmed from docs.
- **config**: sources added later override earlier; maps are merged recursively,
  scalars replaced. Same net effect.

Both satisfy the requirement. figment's model is more explicitly documented (see
the merge/join/adjoin/admerge table in Q2).

### (c) Env overlay with a prefix

- **figment**: `Env::prefixed("CONSOLETTE_")` — strips the prefix, lowercases
  keys. Chain `.split("__")` to descend into nested tables. `.only(&[...])` /
  `.ignore(&[...])` to restrict which keys env may set (use `.only()` to enforce
  the "env touches only an allowlist" recommendation).
- **config**: `Environment::with_prefix("CONSOLETTE").separator("__")`. Plus
  `.list_separator(",")` + `.with_list_parse_key("some.key")` for arrays of
  scalars — slightly more built-in list support than figment.

### (d) Mapping nested keys to env vars

Both use a double-underscore convention: `CONSOLETTE_SERVER__PORT` →
`server.port`. The `__` separator is used (not single `_`) because table/field
names themselves contain underscores (`request_timeout`), so a single-underscore
separator would be ambiguous. This is an env-only convention and does **not**
appear in the TOML files, so Python `tomllib` compatibility is unaffected.

---

## Q2. Deep-merge semantics (tables merge, scalars replace, arrays replace)

figment gives four strategies (from `Figment` docs):

| Strategy | Precedence | Arrays | Scalars / other |
|---|---|---|---|
| `join`    | existing wins | keep existing | keep existing |
| `adjoin`  | existing wins | **concatenate** | keep existing |
| `merge`   | **incoming wins** | **use incoming (replace)** | use incoming |
| `admerge` | incoming wins | **concatenate** | use incoming |

Nested dictionaries are **always unioned recursively** in all four.

**Use `.merge()`** — it is precisely "tables merge, scalars replace, arrays
replace, later file wins," which is the stated requirement.

### Arrays-of-tables gotcha (`[[upstreams]]`, `[[routes]]`)

This is the biggest semantic subtlety and it applies to both crates:

- With `.merge()` (figment) or default `config` behavior, an array of tables is
  treated as a **single non-composite value**. If `00-providers.toml` defines
  `[[upstreams]]` and `10-routing.toml` also defines `[[upstreams]]`, the later
  file's array **completely replaces** the earlier one. There is **no
  element-wise / by-index / by-key merge**.
- This matches the stated requirement ("arrays replace"), so it is the *correct*
  default — but document it loudly, because operators will expect splitting
  `[[upstreams]]` across files to *accumulate*. It does not.
- If you ever want accumulation, figment offers `.admerge()` (concatenate
  arrays). Do **not** use it by default — concatenation across layers makes
  "override a single upstream" impossible and breaks the mental model. Keep each
  logical array owned by exactly one file.

**Practical rule for the schema:** give each array-of-tables a single owning
file (`00-providers.toml` owns `[[upstreams]]`, `10-routing.toml` owns
`[[routes]]`). Merge then behaves intuitively: routing edits never clobber
provider arrays because they live in different top-level keys.

---

## Q3. Env overlay onto nested / array config — pragmatic scope

Env → nested table works fine (`CONSOLETTE_SERVER__PORT`). Env → **array of
tables** is where it falls apart:

- figment's `Env` has no first-class syntax for "the 2nd element of
  `upstreams`." You'd need index-encoded keys and a custom `.map()`, which is
  fragile and undocumented for arrays-of-tables.
- config's `list_separator` handles arrays *of scalars* (e.g.
  `CONSOLETTE_ALLOWED_IPS=a,b,c`) but not arrays *of tables*.

**Recommendation:** restrict env overrides to a small allowlist of top-level
scalars — the values an operator flips at launch or in a systemd unit:

- `CONSOLETTE_PORT` (or `SERVER__PORT`)
- token/secret vars (never put secrets in `conf.d/*.toml`)
- `CONSOLETTE_LOG` / verbosity
- request/cooldown timeouts

Enforce this with figment `Env::prefixed("CONSOLETTE_").split("__").only(&[
"port", "log", "request_timeout", ...])`, or by keeping the env layer's target
struct small. Leave `upstreams`/`routes` **file-only**. This also mirrors the
current code, where env already carries only flat scalars
(`PROXY_PORT`, `COOLDOWN_SECONDS`, `AWS_PROFILE`, `CLAUDE_CODE_OAUTH_TOKEN`) —
those map cleanly onto a prefixed-env allowlist.

Migration note: today's names are unprefixed and inconsistent (`PROXY_PORT`,
`STAPLER_COMPRESS`, `CACHE_ALIGNER`). Moving to a `CONSOLETTE_` prefix is a
breaking change; keep a short back-compat shim (read old names, warn) or bump
config version deliberately.

---

## Q4. Fail-fast validation naming the offending file + key

This is figment's strongest differentiator.

- **Every value carries provider Metadata.** figment attaches a source name to
  each collected value: TOML sources are named after the file path (name starts
  with `"TOML"` and includes the path), env sources include the prefix and the
  word `"environment"`. On an extraction/type error, `figment::Error` reports the
  **key path** and the metadata source, so you get "expected integer for
  `server.port` in `10-routing.toml`"-quality diagnostics essentially for free.
- **Bad TOML syntax**: figment surfaces the underlying `toml` parse error tagged
  with the file whose provider produced it.
- **Unknown / misspelled keys**: not caught by default (serde ignores unknown
  fields). Add `#[serde(deny_unknown_fields)]` on your config structs to make a
  typo like `[[upstreems]]` a hard error naming the field. Combine with figment's
  metadata to name the file. (Same technique applies to the `config` crate.)
- **Cross-reference validation** ("route points at an unknown upstream"): neither
  crate does semantic validation. Do it in a post-`extract()` pass — resolve
  route→upstream references, and on failure return an `anyhow`/`thiserror` error
  that names the route and the missing upstream. Optionally use figment's
  `find_metadata()` to attach the source file of the offending value to the
  message.

`config` crate errors also give a key path and a decent message, but they are
less consistent about naming the *specific file* among many merged sources —
which is exactly what a `conf.d/*.toml` layout needs for debuggability. Edge to
figment.

Suggested pattern:

```rust
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config { /* ... */ }

let cfg: Config = fig.extract()               // figment::Error names source+key
    .map_err(|e| anyhow::anyhow!("invalid consolette config: {e}"))?;
cfg.validate_references()?;                    // your semantic pass
```

---

## Q5. Hot-reload (stretch) — verdict: DEFER

Pattern when you do add it:

1. `notify` (8.2.0 stable) `RecommendedWatcher` on the `conf.d/` directory.
2. **Debounce** — editors write via rename/temp-swap and produce bursts of
   events; reload on a settled 200–500ms window, not per event. (`notify-debouncer-mini`
   / `notify-debouncer-full` companion crates handle this; or hand-roll.)
3. On a settled event, re-run the full `Figment` load into a fresh `Config`.
   **Validate before swapping** — a broken edit must not take down a running
   proxy.
4. Publish with `ArcSwap<Config>` (arc-swap 1.9.2): handlers do
   `let cfg = shared.load();` per request (or per request-batch). In-flight
   requests keep their old `Arc<Config>` snapshot; new requests see the new one.
   Lock-free, no reader stalls.

```rust
static CONFIG: Lazy<ArcSwap<Config>> = /* init from startup load */;
// reload task:
if let Ok(new) = load(conf_d).and_then(|c| c.validate_references().map(|_| c)) {
    CONFIG.store(Arc::new(new));
} else { tracing::warn!("config reload rejected, keeping previous"); }
```

**Why defer for v1:**

- The current proxy explicitly documents "no hot-reloading — restart to pick up
  new values." Restart is cheap for a local single-user proxy.
- Hot-reload adds real correctness surface: debounce tuning, partial-write
  races, validation-on-reload, and deciding which settings are even safe to
  change live (changing the listen `port` mid-flight is meaningless; changing
  routing tables live is the actual use case). Design that deliberately, later.
- `ArcSwap` is trivial to retrofit: build the startup path around
  `Arc<Config>` from day one (store it in an `ArcSwap` even if you never call
  `.store()` again), and hot-reload becomes an additive change — no handler
  rewrites.

Recommendation: ship startup-load + `ArcSwap`-wrapped `Arc<Config>` now (so the
plumbing is reload-ready), but do **not** wire `notify` until a concrete need
(live routing edits) justifies it.

---

## Current state being replaced (context)

`src/config.rs` today: a single `Config` struct populated by
`Config::from_env()`, reading ~13 unprefixed env vars with hand-rolled
`parse_env` / `parse_bool_env` / `parse_env_clamped` helpers and inline
defaults. No files, no layering, no prefix, comment says "No hot-reloading."

The figment approach subsumes all of this:
- inline defaults → `Serialized::defaults(Config::default())`
- env parsing/clamping → serde `Deserialize` + a `validate()` pass (clamping
  becomes explicit validation, which is clearer than silent clamping)
- adds the `conf.d` file layer the current code lacks
- keeps env as the top override layer, now prefixed and allowlisted

`Cargo.toml` already carries `toml`, `serde`, `glob`, `anyhow`, `thiserror`,
`once_cell`, `dashmap` — so figment is a small incremental dependency and
reuses the existing `toml`/`serde`/`glob` stack.

---

## Sources

- figment crate + docs: <https://docs.rs/figment/0.10.19/figment/> (Figment
  struct: merge/join/adjoin/admerge semantics, Metadata/source naming), providers
  `Toml`, `Env`, `Serialized`.
- figment on crates.io (version/date): <https://crates.io/crates/figment>
- config crate: <https://crates.io/crates/config> (0.15.25, 2026-06-26),
  <https://docs.rs/config/> (File/glob, Environment separator/list_separator).
- notify crate: <https://crates.io/crates/notify> (8.2.0 stable; 9.0.0-rc),
  notify-debouncer-mini / -full companions.
- arc-swap crate: <https://crates.io/crates/arc-swap> (1.9.2).
- glob crate ordering note: `glob::glob` returns filesystem order, not sorted —
  <https://docs.rs/glob/> (must `sort()` for lexical merge order).
- serde `deny_unknown_fields`: <https://serde.rs/container-attrs.html>
- Existing code: `~/dotfiles/stapler-scripts/claude-proxy-rs/src/config.rs`,
  `~/dotfiles/stapler-scripts/claude-proxy-rs/Cargo.toml`.
