# Convenience targets. Everything also works with plain cargo; this exists so
# the common sequences are one word long.

CARGO ?= cargo
HELM  ?= helm

.PHONY: help build release test test-ebpf lint fmt bpf ui ui-test helm-lint docker

help: ## Show this help
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

# The dashboard is embedded at compile time, so every Rust target builds it
# first. The dependency is the phony `ui` target rather than the export file:
# an export left behind by an interrupted build, a branch switch or a restored
# directory has a fresh mtime and would otherwise be trusted, and pinning the
# Next.js build id is not an option because hashed assets are served immutable.
#
# `npm install` is the only step that costs real time, so it is gated on the
# lockfile through the marker npm writes into node_modules; `next build` itself
# runs every time, which is what makes the guarantee real.
.PHONY: ui
ui: web/node_modules/.package-lock.json ## Build the dashboard export (always runs before a Rust target)
	@command -v npm >/dev/null 2>&1 || { \
		echo "npm not found: the dashboard is built before the Rust code (Node 20+), see README" >&2; \
		exit 1; \
	}
	cd web && npm run build && npm run sync

web/node_modules/.package-lock.json: web/package-lock.json
	cd web && npm install --no-audit --no-fund

build: ui ## Debug build of the whole workspace
	$(CARGO) build --workspace

release: ui ## Optimised build (thin LTO, stripped)
	$(CARGO) build --workspace --release

test: ui ## Unit, doc and integration tests
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

ui-test: ## Drive the embedded dashboard in a real browser (needs Docker)
	./scripts/ui-smoke.sh

helm-lint: ## Lint the chart
	$(HELM) lint deploy/helm/lidlesseye

docker: ## Build the container image
	docker build -f deploy/docker/Dockerfile -t lidlesseye:dev .
