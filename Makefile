VERSION := $(shell sed -n 's/^version: "\(.*\)"/\1/p' addon/config.yaml)
IMAGE := ghcr.io/nick-tgcs/legit-lp-for-ha
# Line-coverage floor (percent). A ratchet: raise it as coverage climbs, never
# lower it to make a red run pass. Current is ~91% (see `make coverage`).
COVERAGE_FLOOR := 88

.PHONY: promote release-build build test coverage
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
# Cutting a release is PIPELINE-driven — there's normally nothing to run here.
# Bump addon/config.yaml `version:` on develop (via a PR); merging that bump is a
# develop push, and the `promote` workflow opens the develop -> main release PR
# for you. You review + merge it on GitHub; merging main triggers release.yml to
# build + tag + publish off main. Your merge is the release gate. See
# docs/RELEASING.md.
#
# This target is only a manual nudge: dispatch the same `promote` workflow (e.g.
# to (re)open the PR without pushing to develop). It opens the PR; it never merges.
promote:
	gh workflow run promote.yml --ref develop

# Escape hatch: (re)build + publish + tag off main WITHOUT a new PR — e.g. main
# advanced but the push-triggered release.yml didn't run, or a build needs a
# re-run. No-op if v<version> is already tagged. The normal path is the
# pipeline-opened develop -> main PR (the `promote` workflow opens it; you merge).
release-build:
	gh workflow run release.yml --ref main
