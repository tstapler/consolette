BINS := consolette mcp-proxy cmdcrush readme-check

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
