# Convenience targets. Everything also works with plain cargo; this exists so
# the common sequences are one word long.

CARGO ?= cargo
HELM  ?= helm

.PHONY: help build release test test-ebpf lint fmt bpf ui helm-lint docker

help: ## Show this help
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

build: ## Debug build of the whole workspace
	$(CARGO) build --workspace

release: ## Optimised build (thin LTO, stripped)
	$(CARGO) build --workspace --release

test: ## Unit, doc and integration tests
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

helm-lint: ## Lint the chart
	$(HELM) lint deploy/helm/lidlesseye

docker: ## Build the container image
	docker build -f deploy/docker/Dockerfile -t lidlesseye:dev .
