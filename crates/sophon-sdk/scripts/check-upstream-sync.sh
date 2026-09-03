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

goal_reliability=(
  crates/codegen/xai-grok-shell/src/session/acp_session_impl/goal.rs
  crates/codegen/xai-grok-shell/src/session/acp_session_impl/goal_support.rs
  crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn.rs
  crates/codegen/xai-grok-shell/src/session/acp_session_tests/goal/goal_planner_e2e_tests.rs
)

typed_management=(
  crates/codegen/xai-grok-agent/src/builder.rs
  crates/codegen/xai-grok-pager/src/app/acp_handler/tests/queue_and_adoption.rs
  crates/codegen/xai-grok-pager/src/app/app_view.rs
  crates/codegen/xai-grok-shell/src/agent/activity.rs
  crates/codegen/xai-grok-shell/src/agent/mvp_agent/acp_agent.rs
  crates/codegen/xai-grok-shell/src/agent/mvp_agent/agent_ops.rs
  crates/codegen/xai-grok-shell/src/agent/mvp_agent/subagent_spawn.rs
  crates/codegen/xai-grok-shell/src/agent/mvp_agent/tests.rs
  crates/codegen/xai-grok-shell/src/agent/subagent/attempt_runner.rs
  crates/codegen/xai-grok-shell/src/agent/subagent/handle_request.rs
  crates/codegen/xai-grok-shell/src/agent/subagent/mod.rs
  crates/codegen/xai-grok-shell/src/agent/subagent/spawn.rs
  crates/codegen/xai-grok-shell/src/session/acp_session_impl/model_switch.rs
  crates/codegen/xai-grok-shell/src/session/acp_session_impl/parent_message.rs
  crates/codegen/xai-grok-shell/src/session/acp_session_impl/prompt_queue.rs
  crates/codegen/xai-grok-shell/src/session/acp_session_impl/rewind.rs
  crates/codegen/xai-grok-shell/src/session/acp_session_impl/run_loop.rs
  crates/codegen/xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs
  crates/codegen/xai-grok-shell/src/session/acp_session_impl/spawn.rs
  crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn.rs
  crates/codegen/xai-grok-shell/src/session/acp_session_tests/fs_injection_regression_tests.rs
  crates/codegen/xai-grok-shell/src/session/acp_session_tests/support.rs
  crates/codegen/xai-grok-shell/src/session/acp_session_tests/web_search_e2e_tests.rs
  crates/codegen/xai-grok-shell/src/session/acp_types.rs
  crates/codegen/xai-grok-shell/src/session/agent_rebuild.rs
  crates/codegen/xai-grok-shell/src/session/commands.rs
  crates/codegen/xai-grok-shell/src/session/compaction.rs
  crates/codegen/xai-grok-shell/src/session/handle.rs
  crates/codegen/xai-grok-shell/src/session/message_delivery.rs
  crates/codegen/xai-grok-shell/src/session/prompt_queue.rs
  crates/codegen/xai-grok-shell/src/test_support/lsp_runtime.rs
  crates/codegen/xai-grok-shell/src/tools/notification_bridge.rs
  crates/codegen/xai-grok-shell/src/tools/notification_bridge_tests.rs
  crates/codegen/xai-grok-shell/src/tools/tool_context.rs
  crates/codegen/xai-grok-subagent-resolution/src/overrides.rs
  crates/codegen/xai-grok-tools/src/implementations/grok_build/scheduler/actor.rs
  crates/codegen/xai-grok-tools/src/implementations/grok_build/scheduler/types.rs
  crates/codegen/xai-grok-tools/src/implementations/grok_build/task/mod.rs
  crates/codegen/xai-grok-tools/src/implementations/grok_build/task/types.rs
  crates/codegen/xai-grok-tools/src/lib.rs
  crates/codegen/xai-grok-tools/src/management/admission.rs
  crates/codegen/xai-grok-tools/src/management/mod.rs
  crates/codegen/xai-grok-tools/src/management/scheduler_ingress.rs
  crates/codegen/xai-grok-tools/src/notification/types.rs
  crates/codegen/xai-grok-tools/src/registry/types.rs
  crates/codegen/xai-grok-workspace/src/session/tool_config.rs
  crates/codegen/xai-prompt-queue/Cargo.toml
  crates/codegen/xai-prompt-queue/src/lib.rs
  crates/codegen/xai-prompt-queue/src/types.rs
)

git -C "$root" cat-file -e "$pin^{commit}"

all_approved=(
  "${provider_routing[@]}"
  "${hermetic_discovery[@]}"
  "${windows_portability[@]}"
  "${public_snapshot_repairs[@]}"
  "${goal_reliability[@]}"
  "${typed_management[@]}"
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
verify_digest \
  goal-reliability \
  "$digest_dir/goal-reliability.sha256" \
  "${goal_reliability[@]}"
verify_digest \
  typed-management \
  "$digest_dir/typed-management.sha256" \
  "${typed_management[@]}"
