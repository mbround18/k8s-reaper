# k8s-reaper developer tasks. `make help` lists them.

GC_DAYS        ?= 14
GC_DOCKER_KEEP ?= 40GB
IMAGE          ?= k8s-reaper:dev
CLUSTER        ?= k8s-reaper-dev
STORY          ?=
FLUX_VERSION   ?= v2.7.0
DEV_REGISTRY   ?= localhost:5001
# Every dev command names its cluster; the current context is never used.
# k3d (make dev-cluster) by default; an existing kind cluster also works:
#   make dev-kind KUBE_CONTEXT=kind-<name>
KUBE_CONTEXT   ?= k3d-$(CLUSTER)
REGISTRY_IMAGE ?= registry:2.8.3
KUBECTL         = kubectl --context $(KUBE_CONTEXT)
FLUX            = flux --context $(KUBE_CONTEXT)

.PHONY: help build test test-integration lint image gc dev-guard dev-cluster dev-kind dev-registry dev-fixtures dev-push dev-down dev-clean dev-load dev-run dev-notify

help: ## List targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-18s %s\n", $$1, $$2}'

build: gc ## Build the workspace (runs gc first)
	cargo build --workspace --locked

test: lint ## fmt check, clippy, unit + contract tests
	cargo test --workspace --locked

lint: ## fmt check and clippy -D warnings
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets --locked -- -D warnings

test-integration: dev-guard ## Dev-cluster integration suite; STORY=us1_report runs one story
	REAPER_TEST_CONTEXT=$(KUBE_CONTEXT) cargo test -p k8s-reaper --locked $(if $(STORY),--test $(STORY),--tests) -- --ignored --test-threads=1

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

dev-guard:
	@case "$(KUBE_CONTEXT)" in kind-*|k3d-*) ;; \
		*) echo "KUBE_CONTEXT=$(KUBE_CONTEXT) is not a kind or k3d context; refusing"; exit 1 ;; esac

dev-cluster: dev-guard ## New k3d cluster with Flux, then dev-fixtures
	k3d cluster create --config deploy/dev/k3d.yaml
	$(FLUX) install --version=$(FLUX_VERSION)
	$(MAKE) dev-fixtures

dev-kind: dev-guard dev-registry dev-fixtures ## Existing kind cluster that already runs Flux: registry, then dev-fixtures

dev-registry: ## Registry for the Flux payloads on the kind network (k3d makes its own)
	@if docker inspect k3d-reaper-registry >/dev/null 2>&1; then \
		docker start k3d-reaper-registry >/dev/null; \
	else \
		docker run -d --name k3d-reaper-registry --network kind \
			-p 127.0.0.1:5001:5000 $(REGISTRY_IMAGE) >/dev/null; \
	fi

dev-fixtures: dev-guard ## CRD, Flux payloads and the deploy/dev fixtures, on a cluster with Flux
	cargo run -q -p k8s-reaper -- crd | $(KUBECTL) apply -f -
	$(KUBECTL) wait --for=condition=Established --timeout=60s crd/reaperconfigs.reaper.boop.ninja
	$(MAKE) dev-push
	$(KUBECTL) apply -k deploy/dev
	$(KUBECTL) -n partial rollout status deploy/rollout --timeout=120s
	$(KUBECTL) -n partial patch deploy rollout --type=strategic --patch-file deploy/dev/fixtures/partial-break.yaml

dev-push: ## Push the Flux fixture payloads (kustomize dir, crashloop chart) to the dev registry
	flux push artifact oci://$(DEV_REGISTRY)/dev/kustomize:latest \
		--path=deploy/dev/flux/kustomize --source=local --revision=dev
	helm package deploy/dev/flux/charts/crashloop -d target/dev-charts
	helm push target/dev-charts/crashloop-0.1.0.tgz oci://$(DEV_REGISTRY)/charts --plain-http

dev-down: ## Delete the k3d dev cluster and its registry
	k3d cluster delete --config deploy/dev/k3d.yaml
	-k3d registry delete k3d-reaper-registry 2>/dev/null

dev-clean: dev-guard ## Remove what dev-kind added to a shared cluster (keeps the cluster)
	-$(KUBECTL) delete -k deploy/dev/notify --ignore-not-found
	-$(KUBECTL) delete -f deploy/dev/reaper.yaml --ignore-not-found
	-$(KUBECTL) delete -k deploy/dev --ignore-not-found
	-$(KUBECTL) delete crd reaperconfigs.reaper.boop.ninja --ignore-not-found
	-docker rm -f k3d-reaper-registry >/dev/null 2>&1

dev-load: dev-guard ## Load the image into the dev cluster
ifneq ($(filter kind-%,$(KUBE_CONTEXT)),)
	kind load docker-image $(IMAGE) --name $(patsubst kind-%,%,$(KUBE_CONTEXT))
else
	k3d image import $(IMAGE) -c $(CLUSTER)
endif

dev-run: dev-guard ## Run the reaper in-cluster (namespace k8s-reaper, report mode)
	$(KUBECTL) apply -f deploy/dev/reaper.yaml
	$(KUBECTL) -n k8s-reaper rollout restart deploy/k8s-reaper
	$(KUBECTL) -n k8s-reaper rollout status deploy/k8s-reaper --timeout=120s

dev-notify: dev-guard ## Notification catchers (echo, Mailpit), the channel Secret, and ReapPlanned notifications
	$(KUBECTL) apply -k deploy/dev/notify
	$(KUBECTL) patch reaperconfig default --type=merge --patch-file deploy/dev/notify/reaperconfig-patch.yaml
	$(KUBECTL) -n notify-dev rollout status deploy/echo --timeout=120s
	$(KUBECTL) -n notify-dev rollout status deploy/mailpit --timeout=120s
