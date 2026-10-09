BINS := consolette mcp-proxy cmdcrush readme-check

# ui/dist is gitignored; without a real build, build.rs embeds a "Loading..." stub as the
# dashboard. Stamp files (not index.html, which build.rs may create as that stub) record
# that npm/ng actually ran, so make rebuilds only when UI inputs change.
UI_SRCS := $(shell find ui/src ui/public -type f 2>/dev/null) \
	ui/angular.json ui/tsconfig.json ui/tsconfig.app.json ui/package.json

ui/node_modules/.stamp: ui/package-lock.json
	cd ui && npm ci
	@touch $@

ui/dist/.stamp: ui/node_modules/.stamp $(UI_SRCS)
	cd ui && npm run build
	@touch $@

.PHONY: ui
ui: ui/dist/.stamp

# `cargo install` on this machine reliably invalidates the linker's ad-hoc
# signature on the copy it puts in ~/.cargo/bin (fresh `cargo build` output
# is unaffected) — macOS then SIGKILLs it with "Code Signature Invalid" on
# every launch. Re-sign explicitly after install rather than trusting the
# copy.
.PHONY: install
install: ui
	cargo install --path . --force
	@for b in $(BINS); do codesign --sign - -f "$$HOME/.cargo/bin/$$b"; done

.PHONY: build
build: ui
	cargo build --release
