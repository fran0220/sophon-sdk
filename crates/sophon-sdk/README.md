# Sophon SDK

`sophon-sdk` is a thin, provider-aware Rust embedding facade over the public
[`xai-org/grok-build`](https://github.com/xai-org/grok-build) source. It keeps
Grok Build as the agent implementation while giving an application a small,
stable `Agent` / `Session` API. It is an independent redistribution, not an
official xAI SDK.

Current source identity:

- public product source baseline: 1.0.16
- public Grok Build commit: `72a61251fcffb464bcc687aeb5a998e5a98ec0c9`
- public crate metadata: 1.0.16
- embedded monorepo revision: `a549186d9d39311f2d3ee4208db62af8c65aa476`

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

while let Ok(event) = events.try_recv() {
    if let Event::Session {
        update: SessionUpdate::AssistantText(text),
        ..
    } = event
    {
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
or session credential refresh. The provider-routing divergence lets native
image/video clients opt out of the
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

## Stable typed management

`management` is the stable embedded control plane. It does not expose ACP or
`serde_json::Value`: IDs are newtypes, extensible enums are non-exhaustive,
mutations carry actor generations/revisions, and failures are structured.

```rust
use std::time::Duration;
use sophon_sdk::management::{
    OperationId, QueueMutation, QueueMutationRequest, QueueMutationResult,
};

# async fn manage(agent: &sophon_sdk::Agent, session: &sophon_sdk::Session)
#     -> Result<(), Box<dyn std::error::Error>> {
let mut management_events = agent.subscribe_management();
let queue = session.queue_snapshot().await?;
if let Some(entry) = queue.pending.first() {
    let result = session
        .mutate_queue(QueueMutationRequest {
            operation_id: OperationId::new("host-remove-42"),
            expected: queue.version.clone(),
            mutation: QueueMutation::Remove {
                id: entry.id.clone(),
                expected_entry_version: entry.version,
                owner: None,
            },
        })
        .await?;
    match result {
        QueueMutationResult::Committed { snapshot, .. } => {
            println!("queue revision {}", snapshot.version.revision);
        }
        QueueMutationResult::Conflict { snapshot, .. } => {
            // Rebase the intended edit on this authoritative snapshot.
            println!("queue changed to revision {}", snapshot.version.revision);
        }
        QueueMutationResult::OperationIdReused { .. } => {
            // The same idempotency key was reused for a different request.
        }
        _ => {}
    }
}

// A lag or sequence gap is explicit. Subscribe first, then take the domain
// snapshot; refetch that snapshot after RecvError::Lagged or snapshot_required.
let _ = management_events.try_recv();

let report = agent.quiesce(Duration::from_secs(30)).await?;
assert!(report.drained());
# Ok(())
# }
```

The management invariants are:

- **One authority.** FIFO, scheduler, rewind, terminal tasks, subagents, hooks,
  MCP, and effective session state are read or mutated through their Grok Build
  actors. The SDK stores no second queue, task database, or runtime mirror.
- **Linearizable admission.** Human prompts, peer messages, internal scheduler
  fires, and old `Session` handles acquire from one Agent-owned fence. Closing
  it and admitting work share one lock. Quiesce then waits for all accepted
  permits, native FIFO/running prompts, interactions, background tasks,
  live workflows (including between turns), subagents, and completion presentations.
  Shutdown refuses to proceed after a
  timed-out drain.
- **CAS plus idempotency.** Queue and scheduler writes require the exact actor
  generation/revision and a caller operation ID. Conflicts return the current
  snapshot. Successful operation receipts are replayable within that actor
  incarnation; reusing an ID for a different request is rejected.
- **Ordered observation.** `Agent::subscribe` publishes typed management and
  raw Session/extension events in one causal order. In particular, the prior
  prompt's terminal state precedes successor promotion, and promotion precedes
  successor content. A lag in that stream is unrecoverable event-history loss.
  The management-only stream has an Agent-global monotonic sequence; queue
  events carry full versioned snapshots, scheduler and effective-config events
  carry native versions, and other domains explicitly set `snapshot_required`.
  A management-only lag is recovered from the authoritative typed snapshot.
  Queue rows expose their typed human, scheduler, or internal origin, and
  scheduler-owned prompts emit a typed durable `SessionUpdate::TurnCompleted`
  terminal, so an embedding never parses native prompt IDs or terminal JSON to
  adopt autonomous work.
- **No credentials.** Effective configuration reports routing/model/protocol,
  context, media/auxiliary choices, and header/query *names*. API keys, bearer
  values, header/query values, credential files, and browser state are absent.
  Session snapshots distinguish the overrides mounted on the active FIFO batch
  from those that will apply after all currently pending rows drain.

## Public API

- `AgentConfig`, `ModelConfig`, `ProviderConfig`, and `ProviderProtocol` define
  an explicit fixed model catalog. One Agent may route models to different
  providers. Optional `MediaConfig` routes upstream image/video tools without
  reimplementing them.
- `Agent::start`, `subscribe`, `subscribe_management`, `runtime_health`,
  `subscribe_runtime_health`, `quiesce`, session creation/attachment/listing,
  and `shutdown` own the embedding lifecycle. Raw initialization and attach
  responses remain available for forward-compatible capability discovery, but
  are not the lifecycle or management contract.
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
- `management` and typed `Agent` / `Session` methods cover the stable embedded
  management surface: native FIFO, scheduler, rewind, effective configuration,
  usage/info, hooks, skills/workflows, MCP inventory/status, background tasks,
  and subagents.
- `Agent::extension`, `Agent::notify_extension`, and `Session::extension`
  retain the complete Grok Build `x.ai/*` JSON seam for new, experimental, or
  uncommon capabilities. Stable consumers do not need to spell the typed
  management method names or parse their responses.
- `Event` carries typed management plus common user/assistant/thought,
  tool-call, plan, and durable turn-terminal updates in one causal stream.
  Unmirrored standard updates remain available as JSON through
  `SessionUpdate::Other`; xAI extension notifications remain available through
  `Event::Extension`. This is the forward-compatible escape hatch for upstream
  additions, not a second protocol model.
- `PermissionPolicy` supports fail-closed `DenyAll` (the default), `AllowAll`,
  or host-delegated decisions. `ClientHandler` also receives blocking
  agent-to-host extension calls such as ask-user, folder trust, plan exit,
  hooks, and SDK MCP calls. Grok Build remains responsible for tool execution.
- `source_provenance()` reports the exact public and embedded source commits.

For example, the raw seam remains available for deliberately untyped areas:

```rust
let models = agent.extension("x.ai/models/list", serde_json::json!({})).await?;
let session_matches = agent
    .extension("x.ai/session/search", serde_json::json!({ "query": "provider" }))
    .await?;
let resource = session
    .extension("x.ai/mcp/read_resource", serde_json::json!({
        "server": "docs",
        "uri": "docs://experimental"
    }))
    .await?;
# let _ = (models, session_matches, resource);
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
| Runtime lifecycle and lossless Agent replacement | typed health watch, Agent-wide admission fence, `quiesce`, and loss-refusing `shutdown` |
| Native prompt FIFO | typed running/pending snapshot with prompt origin; CAS/idempotent remove, reorder, clear, edit, interject, hold, and release; versioned queue events |
| Scheduler | typed versioned records and snapshot; CAS/idempotent create, update, and delete; versioned upsert/fire/removal events; typed durable terminal for directly admitted foreground occurrences |
| Background terminal tasks | typed records/list and kill outcomes; snapshot-required start/completion events |
| Subagents | typed running list, inspect, cancel, status/results, and snapshot-required lifecycle events |
| Rewind | typed points, generation/revision CAS, modes, file conflicts, result, and cross-compaction replay reporting |
| Session info and usage | typed live identity/context and persisted usage/model totals |
| Effective configuration | credential-free Agent and Session snapshots plus versioned invalidation; active batch versus next empty-FIFO state |
| Hooks, skills and workflows | typed inventories/config/action outcomes and skill mutations; hook invalidation events |
| MCP | typed credential-free inventory, transport facts, tool/status/auth/setup state, and snapshot-required status events |
| Session search/state/history/import/fork/repair/delete | raw `x.ai/session/*` / `x.ai/sessions/*`; content and persistence workflows are not runtime authority |
| Models, modes, commands, workspaces, prompt history | typed model/mode switching plus `x.ai/models/*`, `x.ai/commands/*`, `x.ai/workspaces/*` |
| Local file/content/code search, filesystem and terminal/PTY control | `x.ai/search/*`, `x.ai/code/*`, `x.ai/fs/*`, `x.ai/terminal/*` |
| MCP tools/resources/auth/setup/toggles/config | typed inventory/status; raw mutation/auth/setup/resource calls and reverse `ClientHandler` calls |
| Skills, workflows, plugins, marketplaces and hooks | typed skills/workflows/hooks; raw plugins and marketplaces |
| Tasks, scheduler and subagents | typed records, supported mutations/results, snapshots, and events |
| Git, diffs/staging/commits and linked worktrees | upstream tools plus `x.ai/git/*` and `x.ai/git/worktree/*` |
| Memory, manual/automatic compaction, recap and suggestions | `x.ai/memory/*`, `x.ai/compact_conversation*`, `x.ai/recap`, `x.ai/suggest*` |
| Permissions, ask-user, folder trust, plan exit, client hooks and SDK MCP | `PermissionPolicy::Delegate` and `ClientHandler` |
| New or uncommon standard/extension updates | `SessionUpdate::Other`, `Event::Extension`, and raw request/notification methods |

### Raw extension audit

The following remain raw by design. This is an explicit stability decision,
not an unimplemented management DTO:

| Raw route family | Reason |
|---|---|
| `x.ai/fs/*`, `search/*`, `code/*`, `terminal/*`, `git/*`, worktree and hunk operations | Imperative workspace/tool and UI plumbing; the agent's normal tool loop owns execution. |
| account, auth, billing/credits, privacy/consent, feedback, sharing/cloud and rollout/survey routes | First-party control-plane or product-specific contracts, often identity-bearing. |
| `plugins/*` and `marketplace/*` install/reload/action routes | Experimental product catalog and executable installation lifecycle. |
| session content/search/import/fork/repair/history/state/delete, memory, compact, recap and suggest routes | Content/persistence workflows rather than runtime lifecycle authority; shapes evolve with upstream storage. |
| MCP call/read-resource, mutation, auth, setup, toggle/upsert/delete routes | Provider interaction and credential/setup flows; typed inventory/status is stable and secret-free. |
| models, commands, workspaces and prompt-history catalogs | Discovery/UI catalogs not required for lifecycle correctness; model switching itself is typed. |
| debug/internal/telemetry notifications | Diagnostic and explicitly unstable implementation details. |
| raw queue/scheduler/rewind/task/subagent routes | Compatibility only. The typed native actor methods are the stable path and preserve CAS, idempotency, and recovery semantics. |

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
| Stable typed `Agent` / `Session` lifecycle and management facade | FIFO, scheduler, rewind, task/subagent and hook/MCP actors | Pager/TUI APIs or dependencies |
| Prompt block conversion | Session persistence and replay | A second session store or journal |
| Typed recoverable management events + raw fallback | Skills, plugins, hooks, MCP discovery | Product/control-plane and experimental extension schemas |
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
and other ordinary settings therefore continue to come from `GROK_HOME`. Before
that first load, the SDK fixes capability discovery to hermetic mode: the
resolved Grok home plus explicit or injected paths remain, while project config,
vendor home directories, rules, MCP/LSP servers, hooks, plugins, workflows and
subprocess-environment overlays are not discovered from the ambient workspace.
Ordinary workspace files and project `AGENTS.md` instructions remain visible.
System Grok configuration, Claude managed settings and macOS MDM discovery are
also excluded; managed configuration and requirements under the embedding's
`GROK_HOME` remain effective, including native model/MCP/plugin policy.
The SDK then overlays only its explicit model/media routes and headless embedding
mode. Set `GROK_HOME` before starting an Agent to give the embedding its own
upstream data directory rather than the default `~/.grok`.

## What the 1.0.16 source sync contributes

The complete public snapshot adds safe-point parent-to-child steering and
startup-ready subagent messaging, workflow liveness, MCP OAuth deadlock repair,
rmcp 3.2 / MCP 2026-07-28 support and multi-round elicitation, bind-time MCP
injection, and serialized model/reasoning configuration options. The facade
retains authored and explicitly replaced system prompts across those changes.
Effort-only mutations advance the effective-config clock like model changes.

Session creation can return before deferred MCP startup completes; the native
actor still gates prompt promotion on readiness, and teardown cancels that
startup work before awaiting cleanup. Quiesce snapshots now expose `active_work`
so workflow-only sessions cannot falsely report a completed drain.

Memory consolidation runs on launch/periodically rather than blocking session
close. Compaction retains scheduled loops, live workflows and goal context.
Native task/subagent output waits support a one-hour ceiling (omitted timeout
on get-output remains nonblocking). Existing provider protocols and media
routing remain unchanged.

The upstream connection-prewarm optimization is skipped in hermetic embeddings:
it issues a detached, redirect-following origin GET outside the Agent drain
contract. Normal sampling still uses the shared connection pool. Auth prewarm
and external OTEL exporter initialization are not added to SDK startup; existing
telemetry configuration remains upstream-owned.

Dock, spinner, slash-menu and keyboard/copy improvements remain in the upstream
application source, not in the SDK dependency closure or public API.

## Upstream sync policy

Upstream-owned directories remain byte-for-byte equal to the commit in
`UPSTREAM_GROK_BUILD_COMMIT`, except for the separately digested provider
routing, hermetic embedded discovery, Windows portability, public snapshot
repair, Goal reliability, and typed-management authority groups documented in
[`UPSTREAM_DIVERGENCE.md`](UPSTREAM_DIVERGENCE.md). The sync check validates the
untouched tree and each approved patch independently. An upgrade imports the
complete public snapshot, updates provenance, reconciles those boundaries,
then adapts only this facade for public API changes.

```sh
crates/sophon-sdk/scripts/check-upstream-sync.sh
CARGO_INCREMENTAL=0 cargo clippy --locked -p xai-prompt-queue -p xai-grok-tools -p xai-grok-shell -p sophon-sdk --all-targets -- -D warnings
CARGO_INCREMENTAL=0 cargo test --locked -p xai-prompt-queue
CARGO_INCREMENTAL=0 cargo test --locked -p xai-grok-tools
CARGO_INCREMENTAL=0 cargo test --locked -p xai-grok-shell
CARGO_INCREMENTAL=0 cargo test --locked -p sophon-sdk
```

The crate is consumed from this repository because Grok Build's workspace
crates are not independently published to crates.io. Official npm packages and
precompiled installers are separate release artifacts; their version labels do
not prove source equivalence. The public commit and `SOURCE_REV` are this SDK's
authoritative pin.
