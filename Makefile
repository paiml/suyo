# suyo: Rust server + React/TypeScript UI (ui/). `make` lists the targets.

UI := ui
DB ?= /tmp/suyo-dev.db
LISTEN ?= 127.0.0.1:8080

.DEFAULT_GOAL := help
.PHONY: help ui build test run dev fmt clean

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "} {printf "  \033[1m%-8s\033[0m %s\n", $$1, $$2}'

$(UI)/node_modules: $(UI)/package-lock.json
	cd $(UI) && npm ci
	@touch $@

ui: $(UI)/node_modules ## Build the web UI into ui/dist
	cd $(UI) && npm run build

build: ui ## Build the UI, then the release binary (UI embedded)
	cargo build --release

test: $(UI)/node_modules ## Run Rust tests and the TypeScript typecheck
	cargo test
	cd $(UI) && npm run typecheck

run: build ## Build, then serve the demo on 127.0.0.1:8080 (open /d/demo)
	./target/release/suyo --db $(DB) serve --listen $(LISTEN)

dev: $(UI)/node_modules ## Vite dev server (hot reload); proxies the API to 127.0.0.1:8080
	cd $(UI) && npm run dev

fmt: ## Format Rust code
	cargo fmt

clean: ## Remove build outputs (target/, ui/dist)
	cargo clean
	rm -rf $(UI)/dist
