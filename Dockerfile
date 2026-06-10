# Build from the REPO ROOT: docker build -t ghcr.io/nick-tgcs/legit-lp-for-ha:VERSION .
# ---- build stage: full Rust toolchain (+ C++/CMake to compile HiGHS) ----
FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential cmake && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY scheduler/ scheduler/
RUN cargo build --release --manifest-path scheduler/Cargo.toml

# ---- final stage: HA *Debian* base (glibc, matches the builder). The default
# HA base is Alpine/musl: a glibc binary will NOT run there.
ARG BUILD_FROM=ghcr.io/home-assistant/amd64-base-debian:bookworm
FROM ${BUILD_FROM}
COPY --from=build /src/scheduler/target/release/legit-lp-scheduler /usr/local/bin/legit-lp-scheduler
COPY addon/run.sh /run.sh
RUN chmod a+x /run.sh
CMD ["/run.sh"]
