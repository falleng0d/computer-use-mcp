#!/usr/bin/env just --justfile

set shell := ["sh", "-eu", "-c"]
set windows-shell := ["sh", "-eu", "-c"]

dev_image := "computer-use-mcp:dev"
default_branch := "master"
ci_workflow := "ci.yml"
release_plz_workflow := "release-plz.yml"
release_workflow := "release.yml"

default:
  @just --list

status:
  git status --short --branch

clean:
  cargo clean

fmt:
  cargo fmt

fmt-check:
  cargo fmt --check

lint:
  cargo clippy --locked --all-targets --all-features -- -D warnings

lint-fix:
  cargo clippy --fix --allow-dirty --allow-staged --all-targets --all-features

test:
  cargo test --locked

check: fmt-check lint test

build:
  cargo build

build-release:
  cargo build --release --locked -p computer-use-mcp

run *args:
  cargo run -p computer-use-mcp -- {{args}}

info:
  cargo run -q -p computer-use-mcp -- info

install:
  cargo install --force --locked --path crates/computer-use-mcp

image:
  docker build -f crates/computerd/Dockerfile -t {{dev_image}} .

rm:
    docker stop computer-use && docker rm computer-use

image-check: image
  docker run --rm {{dev_image}} check-tools
  docker rm -f computer-use-mcp-check >/dev/null 2>&1 || true
  docker run -d --rm --name computer-use-mcp-check -e COMPUTERD_TOKEN=check -p 127.0.0.1:17070:7070 {{dev_image}} >/dev/null
  sleep 2
  curl -fsS -H 'Authorization: Bearer check' http://127.0.0.1:17070/health; echo
  docker rm -f computer-use-mcp-check >/dev/null

version:
  release-plz update --config release-plz.toml

tag-prerelease version:
  echo "{{version}}" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+-[0-9A-Za-z.-]+$' || { echo "expected a pre-release version such as 0.2.0-beta.1" >&2; exit 1; }
  git tag -a "v{{version}}" -m "v{{version}}"
  git push origin "v{{version}}"

gh-ci ref=default_branch:
  gh workflow run {{ci_workflow}} --ref {{ref}}

gh-release-pr:
  gh workflow run {{release_plz_workflow}} --ref {{default_branch}}

gh-rebuild-release tag:
  gh workflow run {{release_workflow}} --ref {{default_branch}} -f tag={{tag}}

gh-runs workflow=ci_workflow limit="10":
  gh run list --workflow {{workflow}} --limit {{limit}}

gh-watch run_id:
  gh run watch {{run_id}} --exit-status
