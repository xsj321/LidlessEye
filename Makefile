# Convenience targets. Everything also works with plain cargo; this exists so
# the common sequences are one word long.

CARGO ?= cargo
HELM  ?= helm

.PHONY: help build release test test-ebpf lint fmt bpf ui ui-test helm-lint docker

help: ## Show this help
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

# The dashboard is embedded at compile time and is not committed, so the Rust
# targets depend on a built copy. The file target only re-runs when a source or
# a config file is newer than the last export; `make ui` always rebuilds.
UI_SOURCES := $(shell find web/app web/components web/lib web/types -type f 2>/dev/null)
UI_CONFIG  := web/package.json web/package-lock.json web/next.config.mjs web/tsconfig.json web/postcss.config.mjs

web/dist/index.html: $(UI_SOURCES) $(UI_CONFIG)
	cd web && npm install --no-audit --no-fund
	cd web && npm run build && npm run sync

build: web/dist/index.html ## Debug build of the whole workspace
	$(CARGO) build --workspace

release: web/dist/index.html ## Optimised build (thin LTO, stripped)
	$(CARGO) build --workspace --release

test: web/dist/index.html ## Unit, doc and integration tests
	$(CARGO) test --workspace

test-ebpf: ## Privileged eBPF end-to-end tests (Linux + root, or scripts/dev.sh)
	$(CARGO) test -p lidlesseye-ebpf --test ebpf_e2e -- --ignored --nocapture --test-threads=1

lint: ## clippy with warnings denied, plus rustfmt in check mode
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	$(CARGO) fmt --all --check

fmt: ## Format the workspace
	$(CARGO) fmt --all

bpf: ## Rebuild the eBPF object from profile.bpf.c (needs clang)
	LIDLESSEYE_BUILD_BPF=1 $(CARGO) build -p lidlesseye-ebpf

ui: ## Force a dashboard rebuild (npm install + next build + sync)
	cd web && npm install --no-audit --no-fund && npm run build && npm run sync

ui-test: ## Drive the embedded dashboard in a real browser (needs Docker)
	./scripts/ui-smoke.sh

helm-lint: ## Lint the chart
	$(HELM) lint deploy/helm/lidlesseye

docker: ## Build the container image
	docker build -f deploy/docker/Dockerfile -t lidlesseye:dev .
