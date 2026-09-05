# Maintained Grok Build divergences

Upstream-owned paths match `UPSTREAM_GROK_BUILD_COMMIT` except for six
explicitly reviewed patch groups. Each group has its own file list and SHA-256
digest under `upstream-patches/`; `scripts/check-upstream-sync.sh` rejects both
changes outside these lists and drift within a listed group.

## Provider routing

Embedding providers retain their complete route: endpoint, wire model, static
credential, custom headers, and query parameters. The active session bearer is
used only when the selected route is session-owned.

- Web search keeps its selected Responses provider instead of accepting the
  active chat model's key.
- Prompt suggestions resolve the selected catalog model before applying the
  upstream reasoning-effort configuration. An unresolved route falls back to
  the actual active model and endpoint.
- Session summaries use the same safe fallback rule. Compaction and turn
  summaries share that client.
- Image understanding uses the complete auxiliary-model resolver.
- Authored and explicitly replaced system prompts survive model/effort changes
  and attach-time restoration; upstream effort selection remains authoritative.
- Native image generation, image editing, and video generation can use the
  runtime-only `ImagineProviderConfig`; those clients do not install the active
  session key provider when explicit media credentials are present.

The tools, wire formats, polling, storage, and model defaults remain
upstream-owned. Approved files (digest: `provider-routing.sha256`):

- `crates/codegen/xai-grok-shell/src/agent/config.rs`
- `crates/codegen/xai-grok-shell/src/agent/config_tests.rs`
- `crates/codegen/xai-grok-shell/src/agent/handlers/model_switch.rs`
- `crates/codegen/xai-grok-shell/src/agent/mvp_agent/agent_ops.rs`
- `crates/codegen/xai-grok-shell/src/agent/mvp_agent/session_setup.rs`
- `crates/codegen/xai-grok-shell/src/agent/mvp_agent/tests.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/recap.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/spawn.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_tests/idle_resume_tests.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_tests/inline_auto_compact_flow_tests.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_tests/memory_config_tests.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_tests/replace_system_prompt_tests.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_tests/replay_buffer_send_update_tests.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_tests/web_search_e2e_tests.rs`
- `crates/codegen/xai-grok-shell/src/session/agent_rebuild.rs`
- `crates/codegen/xai-grok-shell/src/session/compaction_inline_auto_compact_flow_tests.rs`
- `crates/codegen/xai-grok-tools/src/implementations/grok_build/image_gen/mod.rs`
- `crates/codegen/xai-grok-tools/src/implementations/grok_build/video_gen/mod.rs`
- `crates/codegen/xai-grok-tools/src/implementations/web_search/client.rs`
- `crates/codegen/xai-grok-tools/src/implementations/web_search/types.rs`
- `crates/codegen/xai-grok-workspace/src/session/tool_config.rs`

## Hermetic embedded discovery

The process-global hermetic mode lets an embedding make the resolved
`GROK_HOME` and explicitly injected paths its only capability configuration
sources. Ambient project/vendor configs, rules, MCP/LSP servers, hooks,
plugins, workflows, and subprocess-environment overlays are excluded;
workspace files and `AGENTS.md` remain available to the agent.

At 1.0.16 the same boundary excludes system Grok policies, Claude managed
settings and macOS MDM at their shared discovery sources, before any process
cache. Embedding-owned managed config and requirements remain effective.
The first-party policy supervisor/fetch is disabled, and orphan cleanup cannot
delete those host-owned files merely because no grok.com account is signed in.
Detached, redirect-following sampler origin prewarming is skipped in this mode;
ordinary routed sampling and connection pooling are unchanged.

Approved files (digest: `hermetic-discovery.sha256`):

- `crates/codegen/xai-grok-agent/src/builder.rs`
- `crates/codegen/xai-grok-agent/src/discovery.rs`
- `crates/codegen/xai-grok-agent/src/plugins/discovery.rs`
- `crates/codegen/xai-grok-agent/src/prompt/agents_md.rs`
- `crates/codegen/xai-grok-agent/src/prompt/skills.rs`
- `crates/codegen/xai-grok-config/src/hermetic.rs`
- `crates/codegen/xai-grok-config/src/lib.rs`
- `crates/codegen/xai-grok-config/src/macos_managed.rs`
- `crates/codegen/xai-grok-config/src/paths.rs`
- `crates/codegen/xai-grok-shell/src/agent/app.rs`
- `crates/codegen/xai-grok-shell/src/agent/config.rs`
- `crates/codegen/xai-grok-shell/src/agent/folder_trust.rs`
- `crates/codegen/xai-grok-shell/src/agent/mvp_agent/sampler_prewarm.rs`
- `crates/codegen/xai-grok-shell/src/config/mod.rs`
- `crates/codegen/xai-grok-shell/src/config/watcher.rs`
- `crates/codegen/xai-grok-shell/src/managed_config/store.rs`
- `crates/codegen/xai-grok-shell/src/managed_config/supervisor.rs`
- `crates/codegen/xai-grok-shell/src/session/workflow/registry.rs`
- `crates/codegen/xai-grok-shell/src/util/config/mcp.rs`
- `crates/codegen/xai-grok-shell/src/util/hooks.rs`
- `crates/codegen/xai-grok-tools/src/implementations/cursor_rules_on_read.rs`
- `crates/codegen/xai-grok-tools/src/implementations/lsp/config.rs`
- `crates/codegen/xai-grok-tools/src/implementations/skills/discovery.rs`
- `crates/codegen/xai-grok-tools/src/types/compat.rs`
- `crates/codegen/xai-grok-workspace/src/envrc.rs`
- `crates/codegen/xai-grok-workspace/src/folder_trust.rs`
- `crates/codegen/xai-grok-workspace/src/permission/claude_settings.rs`
- `crates/codegen/xai-grok-workspace/src/project_config.rs`

`agent/config.rs` is intentionally in both groups because it carries both the
runtime media provider and the resolved hermetic compatibility bit. Both
digests therefore detect changes to that shared boundary.

## Windows portability

The portability patch replaces `/dev/stdout`/`/dev/null` protoc dependency
scanning with temporary files, keeps `process-wrap` on the Windows-compatible
9.0.0 type boundary, and retains compile-regression coverage.

Approved files (digest: `windows-portability.sha256`):

- `crates/build/xai-proto-build/src/lib.rs`
- `crates/codegen/xai-grok-shell-terminal/Cargo.toml`
- `crates/codegen/xai-grok-shell-terminal/src/streaming_local_terminal.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_tests/tool_layer_images_bridge_tests.rs`

## Public snapshot test repair

The post-1.0.13 public snapshot adds a memory-archive test with stale imports
for two helpers that are not present in the public source. The repair removes
only those nonexistent/unused imports so the upstream shell test target
compiles; the test and production implementation are unchanged.
The 1.0.16 image payload regression uses `if let` instead of a single-arm
`match` to satisfy the repository's Clippy policy without changing its assertion.

Approved files (digest: `public-snapshot-repairs.sha256`):

- `crates/codegen/xai-grok-shell/src/upload/memory_tests.rs`
- `crates/codegen/xai-grok-tools/src/implementations/grok_build/read_file/mod.rs`

## Goal reliability

Goal planning keeps its fail-closed contract on every host:

- publishing a staged plan and its immutable baseline uses Windows
  extended-length paths, so a valid plan is not discarded merely because the
  embedding's Session directory exceeds legacy `MAX_PATH`;
- publication errors retain their source, destination and operating-system
  detail in logs instead of collapsing into an unexplained pause;
- when initial planning does fail, `/goal <objective>` ends that Turn with the
  canonical paused message instead of running ordinary inference under a Goal
  that is no longer active, matching the existing `/goal resume` behavior.

Approved files (digest: `goal-reliability.sha256`):

- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/goal.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/goal_support.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_tests/goal/goal_planner_e2e_tests.rs`

`turn.rs` intentionally overlaps typed management because both Goal slash
control flow and typed prompt admission share that boundary. Both digests
therefore detect changes to it.

## Typed management authority

The embedded SDK has a stable provider-aware management plane without copying
Grok Build state or requiring downstream JSON extension DTOs. Grok Build's
actors remain authoritative:

- one Agent-owned admission controller linearizes human, peer, scheduler and
  old-session prompt admission with quiesce, then actor snapshots prove FIFO,
  interactions, background tasks, subagents and completion presentation drain;
- native FIFO and scheduler actors expose generation/revision CAS, bounded
  incarnation-local idempotency receipts, structured conflicts and versioned
  notifications;
- rewind CAS is serialized with prompt acceptance, compaction and rewind, and
  reports cross-compaction replay;
- actor snapshots expose credential-free effective configuration, including
  active FIFO batch versus next empty-FIFO overrides;
- existing terminal-task and subagent authorities expose typed list/inspect/
  cancel/kill paths without a second store.

SDK correctness repairs retain legacy queue-edit behavior while making failed
typed entry-version mutations side-effect-free. Targeted cancellation checks
front identity atomically with the finalization claim; unknown-session mode
changes fail instead of hanging. The gateway optionally cancels orphaned
permission callbacks; the default remains unchanged for native TUI/stdio users.

The SDK projection, public DTOs, and JSON parsing of fixed legacy extension
routes remain under `crates/sophon-sdk` and are excluded from upstream-path
validation. Approved upstream files (digest: `typed-management.sha256`):

- `crates/codegen/xai-acp-lib/src/gateway.rs`
- `crates/codegen/xai-grok-agent/src/builder.rs`
- `crates/codegen/xai-grok-pager/src/app/acp_handler/tests/queue_and_adoption.rs`
- `crates/codegen/xai-grok-pager/src/app/app_view.rs`
- `crates/codegen/xai-grok-shell/src/agent/activity.rs`
- `crates/codegen/xai-grok-shell/src/agent/mvp_agent/acp_agent.rs`
- `crates/codegen/xai-grok-shell/src/agent/mvp_agent/agent_ops.rs`
- `crates/codegen/xai-grok-shell/src/agent/mvp_agent/subagent_spawn.rs`
- `crates/codegen/xai-grok-shell/src/agent/mvp_agent/tests.rs`
- `crates/codegen/xai-grok-shell/src/agent/subagent/attempt_runner.rs`
- `crates/codegen/xai-grok-shell/src/agent/subagent/handle_request.rs`
- `crates/codegen/xai-grok-shell/src/agent/subagent/mod.rs`
- `crates/codegen/xai-grok-shell/src/agent/subagent/spawn.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/cancel.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/model_switch.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/parent_message.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/parent_message_tests.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/prompt_queue.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/rewind.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/run_loop.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/spawn.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_tests/cancel_running_task_tests.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_tests/fs_injection_regression_tests.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_tests/prompt_queue_actor_tests.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_tests/support.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_tests/web_search_e2e_tests.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_types.rs`
- `crates/codegen/xai-grok-shell/src/session/agent_rebuild.rs`
- `crates/codegen/xai-grok-shell/src/session/commands.rs`
- `crates/codegen/xai-grok-shell/src/session/compaction.rs`
- `crates/codegen/xai-grok-shell/src/session/handle.rs`
- `crates/codegen/xai-grok-shell/src/session/message_delivery.rs`
- `crates/codegen/xai-grok-shell/src/session/prompt_queue.rs`
- `crates/codegen/xai-grok-shell/src/test_support/lsp_runtime.rs`
- `crates/codegen/xai-grok-shell/src/tools/notification_bridge.rs`
- `crates/codegen/xai-grok-shell/src/tools/notification_bridge_tests.rs`
- `crates/codegen/xai-grok-shell/src/tools/tool_context.rs`
- `crates/codegen/xai-grok-subagent-resolution/src/overrides.rs`
- `crates/codegen/xai-grok-tools/src/implementations/grok_build/scheduler/actor.rs`
- `crates/codegen/xai-grok-tools/src/implementations/grok_build/scheduler/types.rs`
- `crates/codegen/xai-grok-tools/src/implementations/grok_build/task/mod.rs`
- `crates/codegen/xai-grok-tools/src/implementations/grok_build/task/types.rs`
- `crates/codegen/xai-grok-tools/src/lib.rs`
- `crates/codegen/xai-grok-tools/src/management/admission.rs`
- `crates/codegen/xai-grok-tools/src/management/mod.rs`
- `crates/codegen/xai-grok-tools/src/management/scheduler_ingress.rs`
- `crates/codegen/xai-grok-tools/src/notification/types.rs`
- `crates/codegen/xai-grok-tools/src/registry/types.rs`
- `crates/codegen/xai-grok-workspace/src/session/tool_config.rs`
- `crates/codegen/xai-prompt-queue/Cargo.toml`
- `crates/codegen/xai-prompt-queue/src/lib.rs`
- `crates/codegen/xai-prompt-queue/src/types.rs`

`builder.rs`, `agent_ops.rs`, session spawn/rebuild paths and `tool_config.rs`
intentionally overlap earlier groups. Each digest covers the complete pinned
diff for its exact file set, so either boundary detects drift.

## Updating upstream

1. Import the complete public snapshot and update
   `UPSTREAM_GROK_BUILD_COMMIT` and `SOURCE_REV`.
2. Reconcile only the six groups above with the new upstream paths.
3. Run the focused provider, hermetic-discovery, Windows compile, actor
   management, and SDK checks.
4. Regenerate each digest independently using the corresponding exact array
   and `git diff` command in `scripts/check-upstream-sync.sh`.

If upstream gains an equivalent seam, remove that patch group rather than
maintaining a duplicate implementation.

## 1.0.16 validation (2026-09-05, Linux x86_64)

- `scripts/check-upstream-sync.sh`: untouched paths and all six digests pass.
  The imported public commit is also a Git ancestor, so a full clone retains
  the pinned object instead of depending on a previous orb's fetch.
- `cargo test --locked -p sophon-sdk`: 20 unit tests and the three-protocol
  Agent/Session integration test pass. The integration covers host-owned policy
  survival, no detached startup provider connections, model/prompt ownership,
  independent provider credentials and management behavior.
- Config, MCP, memory, sampler, tools, prompt queue and message-delivery test
  targets pass (including 3,151 tools unit tests). Compaction's focused
  `xai-chat-state` run passes 166 tests; workspace managed policy passes 17.
- Production `cargo clippy --locked -p sophon-sdk -p xai-grok-shell
  -p xai-grok-tools -p xai-prompt-queue --lib -- -D warnings` passes.
  All-target Clippy is not green: existing test-only disallowed raw spawn and
  HTTP-client construction remain in `computer/local/lifecycle.rs` and
  `util/shared_http.rs`. A separate SDK/shell all-target run also reports
  five existing test lints in shell `leader/mod.rs` and
  `session/unified_list/mod.rs` (raw spawn, boolean assertions and identical
  branches). All four files are unchanged by this upgrade. No lint was suppressed.
- Full shell unit execution completes with 6,781 passing, 30 failing and
  5 ignored tests under a four-thread runner. Of the 30 failures, 23 pass when
  isolated (crypto-provider/global-state interference); five worktree fixtures
  pass with test-only Git configuration disabling inherited commit signing
  and selecting `main`. Two still fail in isolation: `provider_expiry_source_precedence`
  (JWT provider initialization) and `parse_list_req_forces_kind_under_process_chat_mode_only`
  (empty kind filter expectation). Both source files are unchanged by this
  upgrade; these failures remain visible rather than weakening their assertions.
  New peer Queue/Steer, workflow drain, startup cancellation and reasoning/head
  ownership regressions pass. External-auth and image-recovery integration
  tests pass after removing the orb's inherited `GROK_AUTH` from their process.
- Use `env -u GROK_AUTH RUST_MIN_STACK=33554432 CARGO_INCREMENTAL=0` for shell
  test commands here; the default test-thread stack overflows on the large
  debug actor future. For Git fixtures also set process-local
  `GIT_CONFIG_COUNT=2 GIT_CONFIG_KEY_0=commit.gpgsign GIT_CONFIG_VALUE_0=false
  GIT_CONFIG_KEY_1=init.defaultBranch GIT_CONFIG_VALUE_1=main`.
- Pager and PTY scenario binaries build. The native `welcome.yaml` and
  `slash_resize_storm.yaml` scenarios pass with no reported bugs; normal and
  tiny-terminal captures were visually inspected. Pager remains outside the
  SDK's normal dependency closure, as do `ratatui` and `crossterm`.
- Windows/macOS execution and live external-provider credentials were not
  exercised in this Linux orb; portability patches are retained.
