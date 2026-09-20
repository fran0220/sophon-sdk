# Native Runtime browser

`sophon-browser` owns Chromium through a multiplexed, loopback-only native CDP
WebSocket. It does not invoke a browser CLI, MCP server, Electron API, or another
agent loop. The published upstream checkout has no browser backend implementation
to reuse; this crate uses Chromium's Accessibility, DOM, Input, Page, Runtime and
Network domains and the SDK's existing `xai-tty-utils::ProcessScope` cleanup.

## Ownership and identity

Create **one `BrowserService` per account/Runtime**, shared across Games. Construct
it using `BrowserConfig { executable, data_dir, artifact_dir, headless,
no_sandbox }`. `new` does not launch Chromium; the first browser operation does.

- `data_dir/profile` is persistent browser identity; an exclusive lock rejects
  competing services. Closing a session/Game must not close this service or delete
  this profile. `close()` terminates the service, not its saved identity.
- `artifact_dir` is a separate persistent Runtime evidence store. The Runtime
  maps returned artifact IDs to the Game/workspace that requested them. Never use
  the profile directory as a Game workspace or include it in workspace uploads.
- Local and cloud use the same API. A cloud host must explicitly choose its own
  account-isolated profile and artifact directories. Nothing copies local cookies,
  storage, passwords, or browser identity into cloud storage.
- Pass an explicit Chromium executable. The library does not install software.
  `no_sandbox` is an explicit isolated-container escape hatch, not a default.
- Host transports must authenticate and isolate Runtime access. This crate does
  not introduce another client permission product or expose CDP to the client.

## Native tools and separate host hooks

`BrowserService::tool_specs()` returns the native `browser` tool definition. The
Runtime registers it directly with its local tool registry. Use
`execute("browser", {"action": ..., ...})`; all actions return JSON or a typed
`Error` with a stable `code()`.

| Action | Arguments / result |
| --- | --- |
| `capabilities` | No Chromium launch; truthful capabilities, audio false |
| `tabs` | `tabs` array of CDP page target metadata (`targetId`, `title`, `url`) |
| `new_tab` | Optional HTTP(S) `url`, defaults to `about:blank`; returns `tab_id` |
| `close_tab` | `tab_id`; recording must be stopped first |
| `navigate` | `tab_id`, HTTP(S) `url`; starts navigation, does not imply load complete |
| `frames` | `tab_id`; CDP frame tree for current document |
| `snapshot` | `tab_id`, optional `frame_id`; accessible nodes, `snapshot_id`, revision and refs |
| `click` | `tab_id`, `ref`; native mouse press/release at the element center |
| `type` | `tab_id`, `ref`, `text`; focus then native text insertion, not replacement |
| `key` | `tab_id`, `key`: Enter, Tab, Escape, Backspace, Delete, or arrow key |
| `scroll` | `tab_id`, `delta_x`, `delta_y`; native wheel input |
| `wait` | `tab_id`, `milliseconds` in 0..10000; cancellable bounded delay |
| `screenshot` | `tab_id`; PNG `artifact_id`, `mime_type`, byte count |
| `events` | `tab_id`; bounded recent console, exception and network metadata |
| `record_start` | `tab_id`; `recording_id`, `audio: false`; requires FFmpeg |
| `record_stop` | `tab_id`; finishes video and returns MP4 `artifact_id` |

Refs belong to one snapshot of one tab/frame. A new snapshot, navigation, input,
or observed DOM mutation invalidates them. A ref used in another tab or after
invalidation returns `stale_ref`; the service never guesses a selector or replays
an action. Same-origin iframe snapshots and input are supported. Cross-origin /
out-of-process iframe automation is not advertised as supported. Browser frames
are discovered afresh rather than treated as durable IDs across navigation.

These **host hooks are intentionally absent from the agent tool schema**:

- `viewport` with `{tab_id, width, height}` sets CSS viewport dimensions (1..4096)
  with device scale factor 1, returning `{width,height,device_scale_factor:1}`.
- `state` with `tab_id` returns `{url,title,can_go_back,can_go_forward,loading}`.
  `back`, `forward` and `reload` initiate navigation and return the same shape
  with `loading:true`. Refresh `state` on subsequent frames/interactions; a
  transient navigation context error does not authorize replaying navigation.
- `clear_site` with an HTTP(S) `origin` closes **all live browser tabs/workers**
  before clearing that origin's saved storage. Other origins' saved data remains.
  The all-tab restart prevents a live page from silently writing cleared storage
  back. Browser cookies have domain scope, not port scope; normal Chrome cookie
  sharing still applies. The result includes `closed_all_tabs:true`.
- `clear_profile` explicitly closes the browser and removes `data_dir/profile`,
  retaining the account/Runtime directory and all artifacts. The next browser
  call starts a fresh profile. Both clear hooks are destructive Settings actions
  for an **explicit user request only**. Never call them on Game removal, close,
  startup failure or recovery. There is no implicit profile reset.
- `stream_start` / `stream_stop` with `tab_id` control CDP screencasting.
- `subscribe_frames()` returns a Tokio broadcast receiver, capacity 8, containing
  `{tab_id, sequence, mime_type: "image/jpeg", base64, metadata}`. Metadata is CDP's
  `deviceWidth`, `deviceHeight`, `pageScaleFactor`, offsets and timestamp. The host
  draws frames and maps pointer coordinates to the viewport's CSS pixels.
- `input` with `{tab_id, kind: "mouse"|"key"|"text", params}` forwards only the
  matching CDP Input method. `params` follows `Input.dispatchMouseEvent`,
  `Input.dispatchKeyEvent` or `Input.insertText`. Display subscription and human
  input are independent of agent tool execution.
- `evaluate` with `{tab_id, expression}` runs a Stage/playtest probe and returns
  CDP's by-value RemoteObject (`type`, `value`, etc.). JS exceptions are errors.
  This is not a sandbox for untrusted host code.
- `artifact` with `artifact_id` reads durable PNG/MP4 evidence as `{mime_type,
  base64}`; IDs are UUIDs, not arbitrary filesystem paths. Inline retrieval is
  capped at 64 MiB. The host owns retention and workspace publication.
- `release_artifact` releases a client reference only; durable evidence remains.

The CDP reader ACKs live frames regardless of subscribers. Frame traffic never
shares the control reply queue or the console/network log. Slow subscribers get
`RecvError::Lagged`; drain to the latest frame and continue. Never route lossy
frames through a queue that can drop native execution/control events. No screenshot
polling is needed to render Stage. CDP screencasting is damage-driven JPEG delivery,
not a fixed-FPS media transport or an audio channel.

## Recording, artifacts, and failure semantics

Silent recordings retain timestamped JPEG frames locally, then encode H.264 MP4
with FFmpeg at stop. Quiet periods hold the last frame, preserving elapsed time;
slow capture may drop frames but must not shorten the clip. Limits are five
minutes, 128 MiB of input frames, one active recording, and a 45-second encoder
deadline. A static page can record its last received frame. Missing FFmpeg,
encoding failure, or no captured frames is an error, never fabricated success.
Audio capture/playback is unsupported. macOS and Windows execution is unverified.

PNG and MP4 files are published by rename only when complete and remain readable
after service restart. No Electron-local path is returned. `close()` cancels an
unfinished recording and checks encoder/browser termination. Interrupted capture
staging is removed on checked close; a host crash or unawaited Drop may leave
staging files for host retention cleanup. Completed artifacts and profile data
are never deleted on close.

Commands are sent once. Dropping an `execute` future stops waiting and removes its
pending response receiver; already-issued browser effects may still occur.
Cancellation is **not rollback**, and timeout/disconnect may mean unknown outcome.
The Runtime must not automatically retry clicks, typing, navigation, evaluation,
or recording commands. Per-CDP calls have a 20-second deadline; an entire action
has a 60-second deadline. Operations serialize on the browser service. Live frame
delivery continues independently while a tool is waiting.

Always await `close()` at Runtime exit. It first rejects/cancels calls, then closes
Chromium, escalating to the enrolled process tree if necessary, and waits for the
child. The process scope and `kill_on_drop` are fallback cleanup, not evidence of
checked shutdown. The library never restarts a crashed browser behind the caller.

## Reproducible verification

```sh
cargo check --locked -p sophon-browser --all-targets
cargo clippy --locked -p sophon-browser --all-targets -- -D warnings
cargo test --locked -p sophon-browser
SOPHON_CHROMIUM=/usr/bin/chromium \
  cargo test --locked -p sophon-browser --test chromium -- --ignored --nocapture
```

The ignored integration test requires actual Chromium plus `ffmpeg` and
`ffprobe`; it fails rather than silently skipping if they are absent. It hosts a
local HTML fixture and checks semantic snapshots, native input, stale refs,
same-origin frames, console/network metadata, screenshots, live frame
backpressure, recording duration, profile locking/persistence, artifacts,
cancellation, and shutdown. Set `SOPHON_BROWSER_EVIDENCE_DIR` to export its fixture
screenshot/video for review. Verified on Linux x86-64 with Chromium 153; this is
not macOS/Windows evidence or full Origin Stage/audio parity. Runtime native-tool
registration and the TS transport must additionally be tested by their owning
integration suite.
