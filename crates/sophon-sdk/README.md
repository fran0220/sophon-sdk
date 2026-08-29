# Sophon SDK

`sophon-sdk` is a thin, provider-aware Rust embedding facade over the public
[`xai-org/grok-build`](https://github.com/xai-org/grok-build) source. It keeps
Grok Build as the agent implementation while giving an application a small,
stable `Agent` / `Session` API. It is an independent redistribution, not an
official xAI SDK.

Current source identity:

- released product baseline: 1.0.13
- public Grok Build commit: `bc7f02eddd3d84085849dc19ed216f11c23b0571`
- public crate metadata: 1.0.12
- embedded monorepo revision: `d5a0335a47221e8c9519936cb693e9b6450227ec`

## Use it

The application supplies an explicit endpoint, credential, provider wire model,
and protocol. The model ID is the stable name used by Sessions; it does not
need to equal the provider's wire model.

```rust
use sophon_sdk::{
    Agent, AgentConfig, Event, MediaConfig, MediaProviderConfig, ModelConfig,
    ProviderConfig, SessionConfig, SessionUpdate, StopReason,
};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let provider = ProviderConfig::openai_responses(
    "https://api.example.com/v1",
    std::env::var("MODEL_API_KEY")?,
    "provider-model-slug",
)
.header("x-tenant", "my-application");

let media = MediaConfig::new(MediaProviderConfig::new(
    "https://media.example.com/v1",
    std::env::var("MEDIA_API_KEY")?,
)
.header(
    "x-media-tenant",
    "my-application",
));

let agent = Agent::start(
    AgentConfig::new(ModelConfig::new("default", provider)).media(media),
)
.await?;
let mut events = agent.subscribe();
let session = agent
    .create_session(SessionConfig::new(std::env::current_dir()?))
    .await?;

let result = session.prompt("Inspect this repository and summarize it").await?;
assert_eq!(result.stop_reason, StopReason::EndTurn);

while let Ok(Event::Session { update, .. }) = events.try_recv() {
    if let SessionUpdate::AssistantText(text) = update {
        print!("{text}");
    }
}

session.close().await?;
agent.shutdown().await?;
# Ok(())
# }
```

Three provider protocols are supported:

| Constructor | Endpoint operation | Authentication |
|---|---|---|
| `ProviderConfig::openai_chat` | OpenAI Chat Completions | `Authorization: Bearer …` |
| `ProviderConfig::openai_responses` | OpenAI Responses | `Authorization: Bearer …` |
| `ProviderConfig::anthropic` | Anthropic Messages | `x-api-key: …` |

Provider base URLs include the API prefix but not the operation path. Custom
headers and query parameters are supported; authentication headers cannot be
overridden. API keys and custom values are redacted from `Debug` output.

Primary inference and auxiliary agent work use the same explicit model catalog.
The default configured model is also the default for automatic session titles,
turn/compaction summaries, image attachment understanding, and prompt
suggestions. Each can be routed independently with
`session_summary_model`, `image_description_model`, or
`prompt_suggestion_model`. Native web search is opt-in through
`web_search_model` and must name an OpenAI Responses route because Grok Build's
search implementation calls the Responses API with its `web_search` tool.
Leaving it unset does not disable the separately configured web-fetch tool.

### Image and video provider routing

`MediaConfig` routes Grok Build's native Imagine-compatible tools to an
explicit `MediaProviderConfig::base_url`:

| Tool | Request |
|---|---|
| Image generation | `POST {base_url}/images/generations` |
| Image edit | `POST {base_url}/images/edits` |
| Image-to-video / reference-to-video | `POST {base_url}/videos/generations`, then `GET {base_url}/videos/{id}` |

Image generation, image edit, and video generation can be enabled separately.
The two image operations accept independent model overrides. This Grok Build
baseline uses its fixed `grok-imagine-video-1.5` model for both video tools, so
the SDK does not expose a video-model setting that upstream would ignore.

`MediaProviderConfig` has its own API key and custom headers. Media requests use
`Authorization: Bearer <media api_key>` and are not affected by model switching
or session credential refresh. This is the fork's one intentional Grok Build
provider-routing divergence: native image/video clients can opt out of the
active session key provider when an embedding supplies an independent media
provider. The same narrow patch keeps web-search credentials/query parameters
and prompt-suggestion routes attached to their selected model provider. The
native request implementation, polling, output storage, and agent tools are
otherwise unchanged. Media query parameters are not supported at this pin.

When `AgentConfig::media` is absent, the SDK force-disables all three media
capabilities instead of allowing Grok Build to inherit a first-party endpoint
or ambient environment flags. The SDK only routes existing upstream tools; it
does not add parallel `generate_image` / `generate_video` APIs or own polling
state.

## Public API

- `AgentConfig`, `ModelConfig`, `ProviderConfig`, and `ProviderProtocol` define
  an explicit fixed model catalog. One Agent may route models to different
  providers. Optional `MediaConfig` routes upstream image/video tools without
  reimplementing them.
- `Agent::start`, `subscribe`, `create_session`, `load_session`,
  `resume_session`, `list_sessions`, and `shutdown` own the embedding
  lifecycle. Session listing preserves Grok Build's cursor pagination. Raw
  initialization and attach responses retain advertised capabilities, model
  and mode state, commands, MCP state, and future fields.
- `SessionConfig` accepts raw upstream metadata and MCP server definitions.
  This preserves agent profiles, plugin directories, tool overrides,
  reasoning settings, SDK-provided MCP servers, and future additions without
  reproducing their schemas in the facade.
- `Session::prompt` and `prompt_blocks` send text, image, audio, linked or
  embedded resources, and forward-compatible raw content. Per-turn metadata,
  model/mode switching, title rename, cancellation, and close map directly to
  upstream operations. Prompt results and session events retain their opaque
  upstream metadata; cancellation metadata exposes subagent cancellation and
  optional rewind controls.
- `Agent::extension`, `Agent::notify_extension`, and the session-scoped
  `Session::extension` expose the complete Grok Build `x.ai/*` request and
  notification surface as JSON. This deliberately replaces hundreds of
  one-line SDK wrappers with one forward-compatible seam.
- `Event` projects common user/assistant/thought, tool-call, and plan updates.
  Unmirrored standard updates remain available as JSON through
  `SessionUpdate::Other`; xAI extension notifications remain available through
  `Event::Extension`. This is the forward-compatible escape hatch for upstream
  additions, not a second protocol model.
- `PermissionPolicy` supports fail-closed `DenyAll` (the default), `AllowAll`,
  or host-delegated decisions. `ClientHandler` also receives blocking
  agent-to-host extension calls such as ask-user, folder trust, plan exit,
  hooks, and SDK MCP calls. Grok Build remains responsible for tool execution.
- `source_provenance()` reports the exact public and embedded source commits.

For example, the same raw seam covers current methods and future additions:

```rust
let models = agent.extension("x.ai/models/list", serde_json::json!({})).await?;
let session_matches = agent
    .extension("x.ai/session/search", serde_json::json!({ "query": "provider" }))
    .await?;
let mcp = session.extension("x.ai/mcp/list", serde_json::json!({})).await?;
# let _ = (models, session_matches, mcp);
# Ok::<(), sophon_sdk::Error>(())
```

### Agent capability coverage

The audit boundary is Grok Build's `MvpAgent`, not pager/TUI commands. Every
non-TUI capability reachable from that agent has an SDK path:

| Grok Build capability | SDK access |
|---|---|
| Prompt loop; text/image/audio/resource input; image understanding | typed `Session` prompt methods plus raw blocks/metadata |
| Native repository, terminal, web-fetch, web-search, image and video tools | executed by the upstream agent; configured through `GROK_HOME` and provider routes |
| Automatic titles, summaries, compaction, prompt suggestions | explicit auxiliary model routes; raw summary/compaction extensions |
| Session list/info/search/state/history/import/fork/repair/usage/rename/delete/rewind | typed lifecycle/list/rename where useful; otherwise `x.ai/session/*`, `x.ai/sessions/*`, and `x.ai/rewind/*` |
| Models, modes, commands, workspaces, prompt history | typed model/mode switching plus `x.ai/models/*`, `x.ai/commands/*`, `x.ai/workspaces/*` |
| Local file/content/code search, filesystem and terminal/PTY control | `x.ai/search/*`, `x.ai/code/*`, `x.ai/fs/*`, `x.ai/terminal/*` |
| MCP tools/resources/auth/setup/toggles/config | `SessionConfig::mcp_server`, `x.ai/mcp/*`, events, and reverse `ClientHandler` calls |
| Skills, workflows, plugins, marketplaces and hooks | effective upstream config plus `x.ai/skills/*`, `x.ai/workflows/*`, `x.ai/plugins/*`, `x.ai/marketplace/*`, `x.ai/hooks/*` |
| Tasks, scheduler and subagents | agent tools plus `x.ai/task/*`, `x.ai/scheduler/*`, `x.ai/subagent/*` and events |
| Git, diffs/staging/commits and linked worktrees | upstream tools plus `x.ai/git/*` and `x.ai/git/worktree/*` |
| Memory, manual/automatic compaction, recap and suggestions | `x.ai/memory/*`, `x.ai/compact_conversation*`, `x.ai/recap`, `x.ai/suggest*` |
| Permissions, ask-user, folder trust, plan exit, client hooks and SDK MCP | `PermissionPolicy::Delegate` and `ClientHandler` |
| New or uncommon standard/extension updates | `SessionUpdate::Other`, `Event::Extension`, and raw request/notification methods |

One declared upstream feature is not part of this usable public-source
baseline: Grok Build declares Cargo feature `local-workspace`, but the public
snapshot omits its `gateway_bridge` module and the feature does not compile.
The SDK therefore does not claim the private Computer Hub own/attach path.
Ordinary local repositories, git worktrees, session rehydration, tools, and all
compiled `MvpAgent` extensions remain covered. Reimplementing that missing
private service would violate the thin-wrapper boundary.

Image/video generation remains a native agent tool rather than a parallel
imperative SDK API: the SDK supplies its provider and the upstream agent owns
tool selection, validation, polling, persistence, and emitted tool events. The
pager's `/imagine` slash-command UI is not an `MvpAgent` API and is therefore
not copied into the SDK.

`Agent` runs upstream `MvpAgent` on a private local executor because that agent
is intentionally `!Send`; the public handles remain `Send + Sync`. Event
delivery uses Tokio broadcast semantics, including an explicit lag error when a
receiver falls behind the bounded buffer.

## Ownership boundary

| Owned by this SDK | Kept in Grok Build | Deliberately not copied into this SDK |
|---|---|---|
| Model and independently credentialed media routing | Agent and model loop | ACP request/transport types |
| `Agent` / `Session` lifecycle facade | Built-in tools and terminal execution | Pager/TUI APIs or dependencies |
| Prompt block conversion | Session persistence and replay | A second session store or journal |
| Common event projection + raw JSON fallback | Skills, plugins, hooks, MCP discovery | Typed mirrors of every extension schema |
| Provider routing + host callback boundary | Subagents, tasks, scheduler, workflows, compaction, worktrees | Kernels, harnesses, artifact or workflow platforms |

ACP is used only as a private in-process adapter because the pinned
`MvpAgent`'s complete session lifecycle is implemented on that trait. No ACP
type appears in the public API, downstream crates do not add an ACP dependency,
and no stdio transport or sidecar process is started. The internal compile
closure still contains `agent-client-protocol` and `xai-acp-lib`; removing them
would require feature-gating/refactoring the upstream agent rather than a thin
facade change. The pager/TUI, `ratatui`, and `crossterm` are absent from the
normal `sophon-sdk` dependency closure.

At startup the SDK loads and resolves Grok Build's effective configuration; it
does not replace it with defaults. Persisted sessions, web fetch, tools, skills,
plugins, hooks, MCP, subagents, workflows, compaction, worktrees, feature gates,
and other ordinary settings therefore continue to come from `GROK_HOME`. The
SDK overlays only its explicit model/media routes and headless embedding mode.
Set `GROK_HOME` before starting an Agent when an embedding needs an isolated
upstream data directory.

## What the 1.0.13 baseline contributes

The full upstream source delta through the public snapshot underlying 1.0.13 is
retained. Agent-facing improvements inherited by the facade include automatic
continuation after length-truncated responses, execution of completed tool calls
before continuation, transient sampler retries, faster subagent delivery, MCP
form/URL elicitation and non-blocking startup, and detailed session-close timing
spans. Configured command and HTTP `PreToolUse` hooks can now ask, defer, and add
post-tool model context; the SDK continues to expose Grok Build's native hook
configuration rather than mirroring that schema.

TUI-only additions such as the credit-limit Try Again action, iTerm2 pasted-image
pixel previews, prompt stashing, modal/catalog changes, and selection behavior
remain in the upstream application source but are intentionally not SDK
concepts.

## Upstream sync policy

Upstream-owned directories remain byte-for-byte equal to the commit in
`UPSTREAM_GROK_BUILD_COMMIT`, except for the provider-routing patch recorded
under [`upstream-patches/`](upstream-patches/). The sync check validates both
the untouched tree and the exact approved patch. An upgrade imports the
complete public snapshot, updates the provenance files, reconciles that one
patch, then adapts only this small facade for public API changes. Do not fork
other upstream agent behavior into `sophon-sdk`.

```sh
crates/sophon-sdk/scripts/check-upstream-sync.sh
CARGO_INCREMENTAL=0 cargo check --locked -p sophon-sdk --all-targets
CARGO_INCREMENTAL=0 cargo test --locked -p sophon-sdk
```

The crate is consumed from this repository because Grok Build's workspace
crates are not independently published to crates.io. Official npm packages and
precompiled installers are separate release artifacts; their version labels do
not prove source equivalence. The public commit and `SOURCE_REV` are this SDK's
authoritative pin.
