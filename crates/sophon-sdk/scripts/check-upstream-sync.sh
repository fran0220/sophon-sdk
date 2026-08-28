#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
pin="$(tr -d '[:space:]' < "$root/UPSTREAM_GROK_BUILD_COMMIT")"

git -C "$root" cat-file -e "$pin^{commit}"
git -C "$root" diff --exit-code "$pin" -- \
  . \
  ':(exclude).agents' \
  ':(exclude)Cargo.lock' \
  ':(exclude)Cargo.toml' \
  ':(exclude)README.md' \
  ':(exclude)UPSTREAM_GROK_BUILD_COMMIT' \
  ':(exclude)crates/sophon-sdk'
