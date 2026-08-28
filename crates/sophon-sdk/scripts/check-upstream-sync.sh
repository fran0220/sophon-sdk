#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
pin="$(tr -d '[:space:]' < "$root/UPSTREAM_GROK_BUILD_COMMIT")"
digest_file="$root/crates/sophon-sdk/upstream-patches/provider-routing.sha256"
approved=(
  crates/codegen/xai-grok-shell/src/agent/config.rs
  crates/codegen/xai-grok-shell/src/agent/mvp_agent/agent_ops.rs
  crates/codegen/xai-grok-shell/src/agent/mvp_agent/tests.rs
  crates/codegen/xai-grok-shell/src/session/acp_session_impl/recap.rs
  crates/codegen/xai-grok-shell/src/session/acp_session_impl/spawn.rs
  crates/codegen/xai-grok-shell/src/session/acp_session_tests/web_search_e2e_tests.rs
  crates/codegen/xai-grok-shell/src/session/agent_rebuild.rs
  crates/codegen/xai-grok-tools/src/implementations/grok_build/image_gen/mod.rs
  crates/codegen/xai-grok-tools/src/implementations/grok_build/video_gen/mod.rs
  crates/codegen/xai-grok-tools/src/implementations/web_search/client.rs
  crates/codegen/xai-grok-tools/src/implementations/web_search/types.rs
  crates/codegen/xai-grok-workspace/src/session/tool_config.rs
)

git -C "$root" cat-file -e "$pin^{commit}"
git -C "$root" diff --exit-code "$pin" -- \
  . \
  ':(exclude).agents' \
  ':(exclude)Cargo.lock' \
  ':(exclude)Cargo.toml' \
  ':(exclude)README.md' \
  ':(exclude)UPSTREAM_GROK_BUILD_COMMIT' \
  ':(exclude)crates/sophon-sdk' \
  ':(exclude)crates/codegen/xai-grok-shell/src/agent/config.rs' \
  ':(exclude)crates/codegen/xai-grok-shell/src/agent/mvp_agent/agent_ops.rs' \
  ':(exclude)crates/codegen/xai-grok-shell/src/agent/mvp_agent/tests.rs' \
  ':(exclude)crates/codegen/xai-grok-shell/src/session/acp_session_impl/recap.rs' \
  ':(exclude)crates/codegen/xai-grok-shell/src/session/acp_session_impl/spawn.rs' \
  ':(exclude)crates/codegen/xai-grok-shell/src/session/acp_session_tests/web_search_e2e_tests.rs' \
  ':(exclude)crates/codegen/xai-grok-shell/src/session/agent_rebuild.rs' \
  ':(exclude)crates/codegen/xai-grok-tools/src/implementations/grok_build/image_gen/mod.rs' \
  ':(exclude)crates/codegen/xai-grok-tools/src/implementations/grok_build/video_gen/mod.rs' \
  ':(exclude)crates/codegen/xai-grok-tools/src/implementations/web_search/client.rs' \
  ':(exclude)crates/codegen/xai-grok-tools/src/implementations/web_search/types.rs' \
  ':(exclude)crates/codegen/xai-grok-workspace/src/session/tool_config.rs'

expected="$(tr -d '[:space:]' < "$digest_file")"
actual="$({
  git -C "$root" diff --no-ext-diff --no-color --no-renames \
    --src-prefix=a/ --dst-prefix=b/ "$pin" -- "${approved[@]}"
} | sha256sum | cut -d' ' -f1)"

if [[ "$actual" != "$expected" ]]; then
  echo "approved provider-routing patch drifted" >&2
  echo "expected: $expected" >&2
  echo "actual:   $actual" >&2
  exit 1
fi
