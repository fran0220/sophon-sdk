# Sophon SDK

`sophon-sdk` is a deliberately thin compatibility crate for the public
[`xai-org/grok-build`](https://github.com/xai-org/grok-build) Rust source. It
pins one audited upstream snapshot and re-exports that snapshot's public agent,
ACP, and transport APIs. It is not an official xAI SDK.

The current source pin is:

- public Grok Build commit: `9684fa3cdbf2995e30ea8b9b637f1db008f144fc`
- source metadata: 1.0.10
- embedded monorepo revision: `70ec060ec3d28e77b9c4593be43c2ab0128bcd21`

## Public surface

```rust
use sophon_sdk::{AuthManager, Config, MvpAgent, acp, grok_build, transport};
```

- `grok_build` re-exports `xai-grok-shell` without changing its behavior.
- `acp` re-exports `agent-client-protocol`.
- `transport` re-exports `xai-acp-lib`.
- `Config`, `AuthManager`, and `MvpAgent` are aliases to the corresponding
  upstream types.
- `source_provenance()` reports the exact public and embedded source commits.

There is intentionally no Sophon-owned runtime, Session model, protocol mirror,
durable store, scheduler, harness, kernel, workflow driver, provider registry,
or orchestration state machine. Applications use Grok Build's native ACP and
public Rust APIs directly.

## Upgrade policy

Upstream-owned directories must remain byte-for-byte equal to the commit in
`UPSTREAM_GROK_BUILD_COMMIT`. An upgrade updates that pin and imports the whole
upstream snapshot; SDK work should normally be limited to dependency or
re-export adjustments caused by upstream API changes.

Run:

```sh
crates/sophon-sdk/scripts/check-upstream-sync.sh
cargo check -p sophon-sdk --all-targets
```

The crate is consumed from this repository because Grok Build's workspace
crates are not independently published to crates.io.
