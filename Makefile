VERSION := $(shell sed -n 's/^version: "\(.*\)"/\1/p' addon/config.yaml)
IMAGE := ghcr.io/nick-tgcs/legit-lp-for-ha
# Line-coverage floor (percent). A ratchet: raise it as coverage climbs, never
# lower it to make a red run pass. Current is ~91% (see `make coverage`).
COVERAGE_FLOOR := 88

.PHONY: release build test coverage
test:
	cd scheduler && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
# Line coverage via cargo-tarpaulin. CI's `coverage` job runs exactly this, so
# a green `make coverage` locally means a green gate. `--follow-exec` makes the
# tracer follow the binary the e2e test spawns, so main.rs counts as the tested
# code it is. Writes an HTML report under scheduler/target/tarpaulin/.
coverage:
	cd scheduler && cargo tarpaulin --timeout 180 --follow-exec \
	  --out Stdout --out Html --out Lcov \
	  --output-dir target/tarpaulin \
	  --fail-under $(COVERAGE_FLOOR)
build:
	docker build -t $(IMAGE):$(VERSION) .
# Releases go through CI (multi-arch build, GHCR push, develop -> main
# promotion, tag + GitHub release). See docs/RELEASING.md.
release:
	gh workflow run release.yml --ref develop
