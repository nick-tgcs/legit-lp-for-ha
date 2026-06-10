VERSION := $(shell sed -n 's/^version: "\(.*\)"/\1/p' addon/config.yaml)
IMAGE := ghcr.io/nick-tgcs/legit-lp-for-ha

.PHONY: release build test
test:
	cd scheduler && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
build:
	docker build -t $(IMAGE):$(VERSION) .
release: test build
	docker push $(IMAGE):$(VERSION)
