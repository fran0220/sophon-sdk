#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
pin="$(tr -d '[:space:]' < "$root/UPSTREAM_GROK_BUILD_COMMIT")"
digest_dir="$root/crates/sophon-sdk/upstream-patches"

provider_routing=(
  crates/codegen/xai-grok-shell/src/agent/config.rs
  crates/codegen/xai-grok-shell/src/agent/config_tests.rs
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

hermetic_discovery=(
  crates/codegen/xai-grok-agent/src/builder.rs
  crates/codegen/xai-grok-agent/src/discovery.rs
  crates/codegen/xai-grok-agent/src/plugins/discovery.rs
  crates/codegen/xai-grok-agent/src/prompt/agents_md.rs
  crates/codegen/xai-grok-agent/src/prompt/skills.rs
  crates/codegen/xai-grok-config/src/hermetic.rs
  crates/codegen/xai-grok-config/src/lib.rs
  crates/codegen/xai-grok-shell/src/agent/app.rs
  crates/codegen/xai-grok-shell/src/agent/config.rs
  crates/codegen/xai-grok-shell/src/agent/folder_trust.rs
  crates/codegen/xai-grok-shell/src/config/mod.rs
  crates/codegen/xai-grok-shell/src/config/watcher.rs
  crates/codegen/xai-grok-shell/src/session/workflow/registry.rs
  crates/codegen/xai-grok-shell/src/util/config/mcp.rs
  crates/codegen/xai-grok-shell/src/util/hooks.rs
  crates/codegen/xai-grok-tools/src/implementations/cursor_rules_on_read.rs
  crates/codegen/xai-grok-tools/src/implementations/lsp/config.rs
  crates/codegen/xai-grok-tools/src/implementations/skills/discovery.rs
  crates/codegen/xai-grok-tools/src/types/compat.rs
  crates/codegen/xai-grok-workspace/src/envrc.rs
  crates/codegen/xai-grok-workspace/src/folder_trust.rs
  crates/codegen/xai-grok-workspace/src/permission/claude_settings.rs
  crates/codegen/xai-grok-workspace/src/project_config.rs
)

windows_portability=(
  crates/build/xai-proto-build/src/lib.rs
  crates/codegen/xai-grok-shell-terminal/Cargo.toml
  crates/codegen/xai-grok-shell-terminal/src/streaming_local_terminal.rs
  crates/codegen/xai-grok-shell/src/session/acp_session_tests/tool_layer_images_bridge_tests.rs
)

public_snapshot_repairs=(
  crates/codegen/xai-grok-shell/src/upload/memory_tests.rs
)

git -C "$root" cat-file -e "$pin^{commit}"

all_approved=(
  "${provider_routing[@]}"
  "${hermetic_discovery[@]}"
  "${windows_portability[@]}"
  "${public_snapshot_repairs[@]}"
)
exclusions=(
  ':(exclude).agents'
  ':(exclude)Cargo.lock'
  ':(exclude)Cargo.toml'
  ':(exclude)README.md'
  ':(exclude)UPSTREAM_GROK_BUILD_COMMIT'
  ':(exclude)crates/sophon-sdk'
)
for path in "${all_approved[@]}"; do
  exclusions+=(":(exclude)$path")
done

git -C "$root" diff --exit-code "$pin" -- . "${exclusions[@]}"

verify_digest() {
  local name="$1"
  local digest_file="$2"
  shift 2
  local expected actual
  expected="$(tr -d '[:space:]' < "$digest_file")"
  actual="$({
    git -C "$root" diff --no-ext-diff --no-color --no-renames \
      --src-prefix=a/ --dst-prefix=b/ "$pin" -- "$@"
  } | sha256sum | cut -d' ' -f1)"

  if [[ "$actual" != "$expected" ]]; then
    echo "approved $name patch drifted" >&2
    echo "expected: $expected" >&2
    echo "actual:   $actual" >&2
    return 1
  fi
}

verify_digest \
  provider-routing \
  "$digest_dir/provider-routing.sha256" \
  "${provider_routing[@]}"
verify_digest \
  hermetic-discovery \
  "$digest_dir/hermetic-discovery.sha256" \
  "${hermetic_discovery[@]}"
verify_digest \
  windows-portability \
  "$digest_dir/windows-portability.sha256" \
  "${windows_portability[@]}"
verify_digest \
  public-snapshot-repairs \
  "$digest_dir/public-snapshot-repairs.sha256" \
  "${public_snapshot_repairs[@]}"
