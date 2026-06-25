# Build from the REPO ROOT: docker build -t ghcr.io/nick-tgcs/legit-lp-for-ha:VERSION .
# The final stage must be an HA *Debian* base (glibc matches the rust:bookworm
# builder; the default HA base is Alpine/musl and the binary will not run there).
# HA publishes per-arch base images, so select via TARGETARCH stage aliases —
# this is what lets one `docker buildx --platform amd64,arm64` build both.
ARG BUILD_FROM_AMD64=ghcr.io/home-assistant/amd64-base-debian:bookworm
ARG BUILD_FROM_ARM64=ghcr.io/home-assistant/aarch64-base-debian:bookworm
# io.hass.arch is per-arch (and HA spells it aarch64, not arm64), so it lives
# on the alias stages; the final stage inherits its base's labels.
# io.hass.version is stamped by the release workflow's `labels:` input.
FROM ${BUILD_FROM_AMD64} AS base-amd64
LABEL io.hass.arch="amd64"
FROM ${BUILD_FROM_ARM64} AS base-arm64
LABEL io.hass.arch="aarch64"

# ---- build stage: full Rust toolchain (+ C++/CMake to compile HiGHS) ----
FROM rust:1-bookworm AS build
# clang/libclang-dev: highs-sys generates bindings with bindgen (needs libclang)
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential cmake clang libclang-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY scheduler/ scheduler/
RUN cargo build --release --manifest-path scheduler/Cargo.toml

# ---- final stage: the per-arch HA Debian base selected above ----
ARG TARGETARCH
FROM base-${TARGETARCH}
LABEL io.hass.type="addon"
COPY --from=build /src/scheduler/target/release/legit-lp-scheduler /usr/local/bin/legit-lp-scheduler
COPY addon/run.sh /run.sh
RUN chmod a+x /run.sh
# Supervisor uses the container health state (the manifest `watchdog:` key is
# obsolete). /health is 200 only while the solve loop is live.
HEALTHCHECK --interval=60s --timeout=10s --start-period=30s \
  CMD curl -fsS http://127.0.0.1:8099/health || exit 1
CMD ["/run.sh"]
