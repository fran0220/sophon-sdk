# Interactive MCP (protocol v2)

Protocol v2 adds three additive MCP capabilities. Existing tool callbacks and the protocol version are unchanged.

## Interactive sessions and server authentication

Set `SessionOptions.interactive` to `true` on each create/load/resume to permit host interaction. It defaults to `false`. The Runtime sends the corresponding `startupHints.nonInteractive` with that session attachment, not ACP initialization. HTTP MCP server configuration retains `headers` and may additionally contain `oauth` and `bearerTokenEnvVar`; both are nullable. OAuth fields (`clientId`, `clientSecretEnvVar`, `scopes`, and `callbackPort`) are independently optional and nullable. `clientSecretEnvVar` and `bearerTokenEnvVar` name environment variables on the runtime machine rather than carrying secret values. Missing variables fail closed. A bearer variable cannot be combined with an explicit Authorization header.

After attaching (creating, loading, or resuming) a session, explicitly complete authentication with:

```ts
await session.authenticateMcp('github')
```

This request resolves only after the native OAuth exchange and MCP handshake finish:

```json
{"type":"request","id":"8","request":{"method":"mcp_authenticate","sessionId":"s1","serverName":"github"}}
{"type":"response","id":"8","result":null}
```

## Elicitation callbacks

Native sends an `mcp_callback` containing the session/request context and either a form or URL request:

```json
{"type":"mcp_callback","id":"c1","request":{"kind":"elicitation","request":{"mode":"form","serverName":"crm","message":"Choose an account","requestedSchema":{"type":"object"}}},"context":{"sessionId":"s1","requestId":"r1"}}
{"type":"callback_result","id":"c1","result":{"action":"accept","content":{"account":"work"}},"error":null}
```

The host answers through the existing callback result frame with `accept` (optionally including `content`), `decline`, or `cancel`. URL elicitation additionally supplies `url` and `elicitationId`. With no `onElicitation` handler, the SDK replies `{"action":"cancel"}`. MCP callbacks are never routed to `onToolCall`.

## OAuth authorization callbacks

OAuth asks the host to open a URL:

```json
{"type":"mcp_callback","id":"c2","request":{"kind":"oauth","request":{"serverName":"github","url":"https://trusted.example/authorize"}},"context":{"sessionId":"s1","requestId":"r2"}}
{"type":"callback_result","id":"c2","result":{"opened":true},"error":null}
```

`opened: true` acknowledges only that the browser was opened; it does not mean authentication completed. With no `onMcpOAuth` handler the SDK replies `{ "opened": false }`.

The SDK/native runtime never opens a browser on behalf of an SDK host. The host must validate and explicitly open only trusted authorization or elicitation URLs. OAuth loopback callbacks listen on the **runtime machine**, which may differ from the host UI machine.

Both `Agent.connect` and `Agent.spawn` accept the host callbacks:

```ts
const agent = await Agent.spawn({
  executable, config,
  onElicitation: (request, context) => host.showMcpDialog(request, context.signal),
  onMcpOAuth: async (request, context) => {
    await host.confirmAndOpenTrustedUrl(request.url, context.signal)
    return { opened: true }
  },
})
const session = await agent.createSession({
  workspace, model: null, tools: [], interactive: true,
  mcpServers: [{ transport: 'http', name: 'crm', url: mcpUrl,
    headers: { 'X-Workspace': workspace.id }, oauth: { scopes: ['read'] } }],
})
await session.authenticateMcp('crm')
```

Initial server discovery remains disk-only. Authentication is explicit, and failures
reject `authenticateMcp` instead of returning a success receipt. Ship the generated
0.9 client together with the matching Runtime; an older Runtime rejects new fields.

## Cancellation

Native cancels either callback with the existing `{"type":"callback_cancelled","id":"c1"}` frame. The SDK aborts the handler's context `signal`, immediately releases its pending entry, and never sends a late result—even if host code ignores the signal. Transport closure likewise aborts all callbacks. Handlers should still stop expensive work promptly when signaled.
