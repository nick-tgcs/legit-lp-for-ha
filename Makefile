VERSION := $(shell sed -n 's/^version: "\(.*\)"/\1/p' addon/config.yaml)
IMAGE := ghcr.io/nick-tgcs/legit-lp-for-ha
# Line-coverage floor (percent). A ratchet: raise it as coverage climbs, never
# lower it to make a red run pass. Current is ~91% (see `make coverage`).
COVERAGE_FLOOR := 88

.PHONY: release promote build test coverage
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
# Cut a release (bump addon/config.yaml `version:` on develop first):
#   make release   # CI: test gate + multi-arch image -> GHCR, then stops
#   make promote   # FF main onto develop, tag v<version>, GitHub release
# See docs/RELEASING.md.
release:
	gh workflow run release.yml --ref develop

# Finish the release the CI build started: fast-forward main onto develop's tip,
# then publish the tag + GitHub release. Run once `make release`'s build has
# pushed the image (while develop's tip is still the built commit). Uses SSH
# (your key), which — unlike the Actions GITHUB_TOKEN — may advance main across
# .github/workflows/ changes. Refuses if the tag exists or the image isn't on
# GHCR yet.
promote:
	@set -e; v="$(VERSION)"; \
	git fetch --quiet origin; \
	sha=$$(git rev-parse origin/develop); \
	if git rev-parse -q --verify "refs/tags/v$$v" >/dev/null; then \
	  echo "tag v$$v already exists — bump addon/config.yaml version on develop first"; exit 1; fi; \
	if ! docker manifest inspect "$(IMAGE):$$v" >/dev/null 2>&1; then \
	  echo "image $(IMAGE):$$v not on GHCR yet — run 'make release' and wait for the build"; exit 1; fi; \
	echo "promote: main -> $$sha (develop tip); tag v$$v"; \
	git push origin "$$sha:refs/heads/main"; \
	git tag "v$$v" "$$sha"; \
	git push origin "v$$v"; \
	gh release create "v$$v" --target "$$sha" --title "v$$v" --generate-notes; \
	echo "released v$$v"
