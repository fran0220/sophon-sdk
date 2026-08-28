# Maintained Grok Build divergence

The fork intentionally carries one provider-routing divergence from the commit
in `UPSTREAM_GROK_BUILD_COMMIT`: explicitly configured embedding providers keep
their own credentials and routing across Grok Build's auxiliary/native tools.

Upstream normally gives those clients the active session/model key provider,
which overwrites their configured key on every request. Sophon adds a
runtime-only `ImagineProviderConfig` and marks the resulting image/video client
configuration not to use that dynamic provider. The native tools, request
formats, polling, storage, and model defaults remain upstream-owned.

The same invariant applies to model-backed auxiliary work. Web search retains
the selected Responses model's key and query parameters instead of replacing
them with the active chat credential. Prompt suggestions resolve the complete
selected model route rather than changing only the model slug on the active
chat client. Session summaries and image understanding already used Grok
Build's complete auxiliary-model resolver and require no fork change.

The approved patch is limited to:

- `crates/codegen/xai-grok-shell/src/agent/config.rs`
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

`scripts/check-upstream-sync.sh` verifies that all other upstream paths match
the pin and that the exact diff of these files matches
`upstream-patches/provider-routing.sha256`.

For an upstream update:

1. import the complete new snapshot and update both provenance files;
2. reconcile only this credential/routing seam with the new upstream paths;
3. run the focused media, web-search, prompt-suggestion, and SDK tests;
4. regenerate the digest with the exact `git diff` command in the sync script.

Do not add unrelated behavior to this divergence. If upstream gains an
equivalent independent provider seam, remove the patch and this exception.
