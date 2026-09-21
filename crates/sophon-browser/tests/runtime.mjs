// Actual official TypeScript client -> native Runtime -> Chromium integration.
// Requires built packages/typescript/dist and SOPHON_RUNTIME; never simulates CDP.
import assert from 'node:assert/strict'
import { createServer } from 'node:http'
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { Agent } from '../../../packages/typescript/dist/index.js'

assert.ok(process.env.SOPHON_RUNTIME, 'SOPHON_RUNTIME must point to a built native Runtime')
const root = await mkdtemp(join(tmpdir(), 'sophon-browser-runtime-'))
const workspace = join(root, 'workspace')
await mkdir(workspace)
let tabId
let nativeDefinition = false
let nativeResult = false
let nativeScreenshotRequested = false
let nativeScreenshot
let providerFixtureRequests = 0
let hostToolCallbacks = 0
const toolResults = []
const server = createServer(async (request, response) => {
  if (request.method !== 'POST') {
    response.writeHead(200, { 'content-type': 'text/html' })
    response.end('<!doctype html><title>Native Runtime fixture</title><style>@keyframes m{to{transform:translateX(100px)}}#box{background:red;width:20px;height:20px;animation:m .4s infinite alternate}</style><h1>Native Runtime fixture</h1><input aria-label="Runtime input"><div id="box"></div>')
    return
  }
  let body = ''
  for await (const chunk of request) body += chunk
  const payload = JSON.parse(body)
  toolResults.push(...(payload.messages ?? []).filter(message => message.role === 'tool').map(message => message.content))
  const browser = payload.tools?.find(tool => tool.function?.name === 'browser')
  const result = payload.messages?.find(message => message.role === 'tool' && JSON.stringify(message.content).includes('Native Runtime fixture'))
  if (result) nativeResult = true
  for (const content of toolResults) {
    for (const text of typeof content === 'string' ? [content] : (content ?? []).map(block => block.text)) {
      try {
        const value = JSON.parse(text)
        if (value.artifact_id && value.mime_type === 'image/png') nativeScreenshot = value
      } catch { /* Other tool messages need not contain JSON. */ }
    }
  }
  const action = browser && !nativeDefinition ? 'snapshot'
    : browser && nativeResult && !nativeScreenshotRequested ? 'screenshot' : null
  if (action === 'screenshot') nativeScreenshotRequested = true
  const delta = action
    ? { role: 'assistant', tool_calls: [{ index: 0, id: `native-browser-${action}`, type: 'function', function: { name: 'browser', arguments: JSON.stringify({ action, tab_id: tabId }) } }] }
    : { role: 'assistant', content: 'Native browser verification complete.' }
  if (browser) nativeDefinition = true
  providerFixtureRequests++
  const chunk = (delta, finish_reason) => ({ id: 'local-fixture', object: 'chat.completion.chunk', created: 1234567890, model: 'fixture', choices: [{ index: 0, delta, finish_reason }] })
  response.writeHead(200, { 'content-type': 'text/event-stream' })
  response.end(`data: ${JSON.stringify(chunk(delta, null))}\n\ndata: ${JSON.stringify(chunk({}, delta.tool_calls ? 'tool_calls' : 'stop'))}\n\ndata: ${JSON.stringify({ id: 'local-fixture', object: 'chat.completion.chunk', created: 1234567890, model: 'fixture', choices: [], usage: { prompt_tokens: 12, completion_tokens: 8, total_tokens: 20 } })}\n\ndata: [DONE]\n\n`)
})
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve))
const url = `http://127.0.0.1:${server.address().port}`
await mkdir(join(root, 'home'))
await writeFile(join(root, 'home/config.toml'), '[features]\nsession_recap = false\n')
await writeFile(join(root, 'home/managed_config.toml'), 'plugin_auto_update = false\n')
let agent
let exited = false
try {
  agent = await Agent.spawn({
    executable: process.env.SOPHON_RUNTIME,
    env: { ...process.env, GROK_HOME: join(root, 'home'), GROK_AUTH: '', GROK_TELEMETRY_ENABLED: 'false', GROK_TRACE_UPLOAD: 'false', GROK_FEEDBACK_ENABLED: 'false', GROK_TURN_SUMMARY: 'false' },
    config: {
      models: [{ id: 'fixture', provider: { protocol: 'openai_chat', baseUrl: url, apiKey: 'local-fixture-no-secret', model: 'fixture', headers: {}, queryParams: {} }, contextWindow: 32768, maxCompletionTokens: 1024 }],
      defaultModel: 'fixture', webSearchModel: null, sessionSummaryModel: null, compactionModel: null, imageDescriptionModel: null, media: null, subagents: [],
      browser: { executable: process.env.SOPHON_CHROMIUM ?? '/usr/bin/chromium', dataDir: join(root, 'identity'), artifactDir: join(root, 'evidence'), headless: true, noSandbox: true },
    },
    onCallback: async ({ method }) => {
      if (method.startsWith('tool/')) hostToolCallbacks++
      throw new Error(`Unexpected client callback ${method}`)
    },
  })
  const capabilities = await agent.browser({ action: 'capabilities' })
  assert.equal(capabilities.streaming, true)
  assert.equal(capabilities.audio, false)
  tabId = (await agent.browser({ action: 'new_tab', url })).tab_id
  await agent.browser({ action: 'viewport', tab_id: tabId, width: 960, height: 540 })
  await agent.browser({ action: 'wait', tab_id: tabId, milliseconds: 250 })
  const state = await agent.browser({ action: 'state', tab_id: tabId })
  assert.equal(state.title, 'Native Runtime fixture')
  const viewport = await agent.browser({ action: 'evaluate', tab_id: tabId, expression: '[innerWidth, innerHeight]' })
  assert.deepEqual(viewport.value, [960, 540])
  const snapshot = await agent.browser({ action: 'snapshot', tab_id: tabId })
  const input = snapshot.nodes.find(node => node.role === 'textbox' && node.name === 'Runtime input')
  assert.ok(input?.ref)
  await agent.browser({ action: 'type', tab_id: tabId, ref: input.ref, text: 'Across native Runtime' })
  const probe = await agent.browser({ action: 'evaluate', tab_id: tabId, expression: 'document.querySelector("input").value' })
  assert.equal(probe.value, 'Across native Runtime')
  await agent.browser({ action: 'input', tab_id: tabId, kind: 'text', params: { text: '-host' } })
  const hostProbe = await agent.browser({ action: 'evaluate', tab_id: tabId, expression: 'document.querySelector("input").value' })
  assert.equal(hostProbe.value, 'Across native Runtime-host')
  await assert.rejects(agent.browser({ action: 'evaluate', tab_id: tabId, expression: 'throw new Error("stage-probe-error")' }), /stage-probe-error/)
  const framePromise = new Promise((resolve, reject) => {
    const timer = setTimeout(() => { unsubscribe(); reject(new Error('No native live frame within 5 seconds')) }, 5000)
    const unsubscribe = agent.subscribeBrowserFrames(frame => {
      if (frame.tab_id !== tabId) return
      clearTimeout(timer)
      unsubscribe()
      resolve(frame)
    })
  })
  await agent.browser({ action: 'stream_start', tab_id: tabId })
  const frame = await framePromise
  assert.equal(frame.mime_type, 'image/jpeg')
  assert.deepEqual([...Buffer.from(frame.base64, 'base64').subarray(0, 2)], [255, 216])
  const image = await agent.browser({ action: 'screenshot', tab_id: tabId })
  const artifact = await agent.browser({ action: 'artifact', artifact_id: image.artifact_id })
  assert.equal(Buffer.from(artifact.base64, 'base64').subarray(1, 4).toString(), 'PNG')
  await agent.browser({ action: 'record_start', tab_id: tabId })
  await new Promise(resolve => setTimeout(resolve, 1200))
  const recording = await agent.browser({ action: 'record_stop', tab_id: tabId })
  const video = await agent.browser({ action: 'artifact', artifact_id: recording.artifact_id })
  assert.equal(video.mime_type, 'video/mp4')
  assert.equal(Buffer.from(video.base64, 'base64').subarray(4, 8).toString(), 'ftyp')
  const session = await agent.createSession({ workspace: { id: 'browser-proof', cwd: workspace }, model: 'fixture', mcpServers: [], tools: [] })
  await session.prompt({ turnId: 'native-browser-roundtrip', blocks: [{ type: 'text', text: 'Verify native browser fixture using the browser tool.' }] })
  assert.equal(nativeDefinition, true, 'provider-protocol fixture must receive native browser definition')
  assert.equal(nativeResult, true, `model-facing payload must contain actual Chromium snapshot: ${JSON.stringify(toolResults)}`)
  assert.ok(nativeScreenshot?.artifact, `native screenshot must publish workspace artifact: ${JSON.stringify(nativeScreenshot)}`)
  const published = nativeScreenshot.artifact
  assert.match(nativeScreenshot.artifact_id, /^[0-9a-f-]{36}$/)
  assert.equal(published.path, `.native-browser/${nativeScreenshot.artifact_id}.png`)
  assert.equal(published.mimeType, 'image/png')
  assert.equal(nativeScreenshot.reviewRequired, true)
  assert.ok(published.revision)
  const publishedRead = await session.readArtifact(published.path)
  const publishedBytes = Buffer.from(publishedRead.base64, 'base64')
  assert.equal(publishedBytes.length, published.bytes)
  assert.deepEqual([...publishedBytes.subarray(0, 8)], [137, 80, 78, 71, 13, 10, 26, 10])
  assert.deepEqual(publishedBytes, await readFile(join(workspace, published.path)))
  await assert.rejects(readFile(join(root, 'evidence', published.path)), { code: 'ENOENT' })
  assert.equal(hostToolCallbacks, 0, 'first-party browser must not route to a TS callback')
  await agent.browser({ action: 'stream_stop', tab_id: tabId })
  await agent.finalExit()
  exited = true
  console.log(JSON.stringify({ ok: true, nativeBrowserTool: true, hostToolCallbacks, providerFixtureRequests, realChromium: true, liveFrame: true, viewport: true, hostInput: true, probeExceptions: true, screenshot: true, workspaceArtifact: true, recording: true, checkedRuntimeExit: true }))
} finally {
  if (agent && !exited) await agent.finalExit().catch(() => {})
  await new Promise(resolve => server.close(resolve))
  await rm(root, { recursive: true, force: true })
}
