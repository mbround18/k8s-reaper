# syntax=docker/dockerfile:1
ARG RUST_VERSION=1.99

FROM lukemathwalker/cargo-chef:latest-rust-${RUST_VERSION} AS chef
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS cook
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json

FROM cook AS builder
COPY . .
RUN cargo build --release --locked -p k8s-reaper \
 && cp target/release/k8s-reaper /k8s-reaper

# Same Debian release as the cargo-chef builder (13), so glibc matches.
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=builder /k8s-reaper /k8s-reaper
# Numeric, so runAsNonRoot can verify it (distroless nonroot is 65532).
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/k8s-reaper"]
