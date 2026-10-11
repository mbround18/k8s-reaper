# syntax=docker/dockerfile:1
ARG RUST_VERSION=1.99

FROM lukemathwalker/cargo-chef:latest-rust-${RUST_VERSION} AS chef
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# PROFILE=dev builds a debug binary, which has the test-only hooks
# (`make image-debug`); never ship one.
FROM chef AS cook
ARG PROFILE=release
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --profile "$PROFILE" --locked --recipe-path recipe.json

FROM cook AS builder
ARG PROFILE=release
COPY . .
RUN cargo build --profile "$PROFILE" --locked -p k8s-reaper \
 && cp "target/$([ "$PROFILE" = dev ] && echo debug || echo "$PROFILE")/k8s-reaper" /k8s-reaper

# Same Debian release as the cargo-chef builder (13), so glibc matches.
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=builder /k8s-reaper /k8s-reaper
# Numeric, so runAsNonRoot can verify it (distroless nonroot is 65532).
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/k8s-reaper"]
