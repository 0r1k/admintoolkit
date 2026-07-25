BIN     := atk
RELEASE := target/release/$(BIN)
LOCAL   := $(HOME)/.local/bin/$(BIN)

.DEFAULT_GOAL := build

.PHONY: build release run fmt fmt-check check test ci clean install uninstall help

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

build: ## Debug build
	cargo build

release: ## Optimized release build
	cargo build --release

run: ## Run the debug build (cargo run -- <args> to pass args)
	cargo run --

fmt: ## Reformat source with rustfmt
	cargo fmt

fmt-check: ## Check formatting without modifying files (CI-friendly)
	cargo fmt -- --check

check: ## Lint with clippy, warnings as errors
	cargo clippy -- -D warnings

test: ## Run the test suite
	cargo test

ci: fmt-check check test ## Everything CI should run before a merge

clean: ## Remove build artifacts
	cargo clean

install: release ## Install the release binary to ~/.local/bin (or $DESTDIR$LOCAL)
	install -Dm755 $(RELEASE) $(DESTDIR)$(LOCAL)

uninstall: ## Remove the installed binary
	rm -f $(DESTDIR)$(LOCAL)
