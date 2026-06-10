VERSION := $(shell sed -n 's/^version: "\(.*\)"/\1/p' addon/config.yaml)
IMAGE := ghcr.io/nick-tgcs/legit-lp-for-ha

.PHONY: release build test
test:
	cd scheduler && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
build:
	docker build -t $(IMAGE):$(VERSION) .
# Releases go through CI (multi-arch build, GHCR push, develop -> main
# promotion, tag + GitHub release). See docs/RELEASING.md.
release:
	gh workflow run release.yml --ref develop
