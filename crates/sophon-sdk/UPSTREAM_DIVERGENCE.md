# Maintained Grok Build divergences

Upstream-owned paths match `UPSTREAM_GROK_BUILD_COMMIT` except for four
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
- Native image generation, image editing, and video generation can use the
  runtime-only `ImagineProviderConfig`; those clients do not install the active
  session key provider when explicit media credentials are present.

The tools, wire formats, polling, storage, and model defaults remain
upstream-owned. Approved files (digest: `provider-routing.sha256`):

- `crates/codegen/xai-grok-shell/src/agent/config.rs`
- `crates/codegen/xai-grok-shell/src/agent/config_tests.rs`
- `crates/codegen/xai-grok-shell/src/agent/mvp_agent/agent_ops.rs`
- `crates/codegen/xai-grok-shell/src/agent/mvp_agent/tests.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/recap.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/spawn.rs`
- `crates/codegen/xai-grok-shell/src/session/acp_session_tests/web_search_e2e_tests.rs`
- `crates/codegen/xai-grok-shell/src/session/agent_rebuild.rs`
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

Approved files (digest: `hermetic-discovery.sha256`):

- `crates/codegen/xai-grok-agent/src/builder.rs`
- `crates/codegen/xai-grok-agent/src/discovery.rs`
- `crates/codegen/xai-grok-agent/src/plugins/discovery.rs`
- `crates/codegen/xai-grok-agent/src/prompt/agents_md.rs`
- `crates/codegen/xai-grok-agent/src/prompt/skills.rs`
- `crates/codegen/xai-grok-config/src/hermetic.rs`
- `crates/codegen/xai-grok-config/src/lib.rs`
- `crates/codegen/xai-grok-shell/src/agent/app.rs`
- `crates/codegen/xai-grok-shell/src/agent/config.rs`
- `crates/codegen/xai-grok-shell/src/agent/folder_trust.rs`
- `crates/codegen/xai-grok-shell/src/config/mod.rs`
- `crates/codegen/xai-grok-shell/src/config/watcher.rs`
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

Approved file (digest: `public-snapshot-repairs.sha256`):

- `crates/codegen/xai-grok-shell/src/upload/memory_tests.rs`

## Updating upstream

1. Import the complete public snapshot and update
   `UPSTREAM_GROK_BUILD_COMMIT` and `SOURCE_REV`.
2. Reconcile only the four groups above with the new upstream paths.
3. Run the focused provider, hermetic-discovery, Windows compile, and SDK
   checks.
4. Regenerate each digest independently using the corresponding exact array
   and `git diff` command in `scripts/check-upstream-sync.sh`.

If upstream gains an equivalent seam, remove that patch group rather than
maintaining a duplicate implementation.
