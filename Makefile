BINS := consolette mcp-proxy cmdcrush readme-check

# The dashboard UI (ui/) is built by build.rs whenever its inputs change; no make step needed.
# `make ui-fresh-check` builds with CONSOLETTE_REQUIRE_UI=1, then runs tests/ui_bundle_fresh.rs,
# which fails if the embedded bundle is the placeholder or older than any input build.rs watches.
# CI runs it after `cargo test` (.github/workflows/ci.yml).

# `cargo install` on this machine reliably invalidates the linker's ad-hoc
# signature on the copy it puts in ~/.cargo/bin (fresh `cargo build` output
# is unaffected) — macOS then SIGKILLs it with "Code Signature Invalid" on
# every launch. Re-sign explicitly after install rather than trusting the
# copy.
.PHONY: install
install:
	cargo install --path . --force
	@for b in $(BINS); do codesign --sign - -f "$$HOME/.cargo/bin/$$b"; done

.PHONY: build
build:
	cargo build --release

.PHONY: ui-fresh-check
ui-fresh-check:
	CONSOLETTE_REQUIRE_UI=1 cargo build
	CONSOLETTE_REQUIRE_UI=1 cargo test --test ui_bundle_fresh
