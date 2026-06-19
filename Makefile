VERSION := $(shell sed -n 's/^version: "\(.*\)"/\1/p' addon/config.yaml)
IMAGE := ghcr.io/nick-tgcs/legit-lp-for-ha
# Line-coverage floor (percent). A ratchet: raise it as coverage climbs, never
# lower it to make a red run pass. Current is ~91% (see `make coverage`).
COVERAGE_FLOOR := 88

.PHONY: release release-build build test coverage
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
# Cut a release. Bump addon/config.yaml `version:` on develop first (via a PR),
# then:
#   make release   # open the develop -> main PR with YOUR credentials (no bot).
# CI validates it; you approve + merge it on GitHub. Merging main is a push that
# triggers release.yml to build + tag + publish off main. Your merge is the
# release gate. See docs/RELEASING.md.
release:
	gh pr create --base main --head develop \
	  --title "Release: promote develop -> main" \
	  --body "Promote develop's validated tip to main. CI runs the required \`test\` check; approve + merge to release — merging main triggers release.yml to build off main."

# Escape hatch: (re)build + publish + tag off main WITHOUT a new PR — e.g. main
# advanced but the push-triggered release.yml didn't run, or a build needs a
# re-run. No-op if v<version> is already tagged. The normal path is the
# develop -> main PR (`make release` opens it; you approve + merge it).
release-build:
	gh workflow run release.yml --ref main
