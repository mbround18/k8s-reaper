# k8s-reaper developer tasks. `make help` lists them.

GC_DAYS        ?= 14
GC_DOCKER_KEEP ?= 40GB
IMAGE          ?= k8s-reaper:dev
CLUSTER        ?= k8s-reaper-dev
STORY          ?=

.PHONY: help build test test-integration lint image gc dev-cluster dev-load dev-run

help: ## List targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-18s %s\n", $$1, $$2}'

build: gc ## Build the workspace (runs gc first)
	cargo build --workspace --locked

test: lint ## fmt check, clippy, unit + contract tests
	cargo test --workspace --locked

lint: ## fmt check and clippy -D warnings
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets --locked -- -D warnings

test-integration: ## k3d + Flux integration suite; STORY=us1_report runs one story
	cargo test -p k8s-reaper --locked $(if $(STORY),--test $(STORY),--tests) -- --ignored --test-threads=1

image: ## Build the container image
	docker build -t $(IMAGE) .

# Build hygiene. Never fails the build.
gc: ## Sweep stale target/ output and cap the BuildKit cache
	@if command -v cargo-sweep >/dev/null 2>&1; then \
		cargo sweep --time $(GC_DAYS) || true; \
	else \
		echo "gc: cargo-sweep not installed (cargo install cargo-sweep --locked); skipping"; \
	fi
	@if docker info >/dev/null 2>&1; then \
		docker builder prune -f --max-used-space $(GC_DOCKER_KEEP) >/dev/null || true; \
	else \
		echo "gc: docker not running; skipping build cache prune"; \
	fi

dev-cluster: ## k3d cluster with Flux, the CRD and the deploy/dev fixtures
	k3d cluster create --config deploy/dev/k3d.yaml
	flux install
	cargo run -q -p k8s-reaper -- crd | kubectl apply -f -
	kubectl apply -k deploy/dev

dev-load: ## Import the image into the k3d cluster
	k3d image import $(IMAGE) -c $(CLUSTER)

dev-run: ## Run the reaper in-cluster (namespace k8s-reaper, report mode)
	kubectl apply -f deploy/dev/reaper.yaml
	kubectl -n k8s-reaper rollout status deploy/k8s-reaper
