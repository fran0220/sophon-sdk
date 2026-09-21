// Opt-in real account-effective gateway inference -> native browser acceptance.
// Never logs credentials, provider payloads, or unsanitized Runtime diagnostics.
import assert from 'node:assert/strict'
import { createServer } from 'node:http'
import { mkdtemp, mkdir, readdir, readFile, rm, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { tmpdir } from 'node:os'
import { Agent } from '../../../packages/typescript/dist/index.js'

assert.ok(process.env.SOPHON_RUNTIME && process.env.OG_AI_GATEWAY && process.env.OG_API_KEY)
const gateway = process.env.OG_AI_GATEWAY.replace(/\/+$/, '').replace(/\/v1$/, '')
async function discover(path) {
  const response = await fetch(`${gateway}${path}`, { redirect: 'error', signal: AbortSignal.timeout(30000), headers: { authorization: `Bearer ${process.env.OG_API_KEY}` } })
  assert.equal(response.status, 200, `Discovery ${path} status`)
  return response.json()
}
const [effective, catalog] = await Promise.all([discover('/api/v1/product-config/effective'), discover('/v1/models')])
assert.ok(effective.revision >= 1)
const dial = effective.default_dial
assert.ok(['fast', 'deep'].includes(dial))
const route = effective.routes[dial]
assert.equal(route.status, 'available')
assert.equal(route.endpoint, 'openai-response')
const matches = catalog.data.filter(model => model.id === route.model)
assert.equal(matches.length, 1)
const model = matches[0]
assert.ok(model.supported_endpoint_types.includes(route.endpoint))
const contextWindow = model.limit?.context
const maxCompletionTokens = model.limit?.output
assert.ok(Number.isSafeInteger(contextWindow) && contextWindow > 0)
assert.ok(Number.isSafeInteger(maxCompletionTokens) && maxCompletionTokens > 0)
const supportedReasoning = model.supported_reasoning
assert.ok(Array.isArray(supportedReasoning))
assert.ok(supportedReasoning.every(effort => ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'].includes(effort)))
if (route.reasoning_effort !== null) assert.ok(supportedReasoning.includes(route.reasoning_effort))
console.log(JSON.stringify({ phase: 'discovery', revision: effective.revision, dial, model: route.model, endpoint: route.endpoint, reasoning: route.reasoning_effort }))

const root = await mkdtemp(join(tmpdir(), 'sophon-browser-gateway-'))
const workspace = join(root, 'workspace')
await mkdir(workspace)
await mkdir(join(root, 'home'))
await writeFile(join(root, 'home/config.toml'), '[features]\nsession_recap = false\n')
await writeFile(join(root, 'home/managed_config.toml'), 'plugin_auto_update = false\n')
let pageLoads = 0
const submitted = []
const server = createServer((request, response) => {
  const url = new URL(request.url, 'http://fixture.invalid')
  response.setHeader('content-type', 'text/html')
  if (url.pathname === '/receipt') {
    submitted.push(url.searchParams.get('value'))
    response.end('<!doctype html><title>Browser receipt</title><h1>Receipt accepted</h1>')
  } else {
    if (url.pathname === '/form') pageLoads++
    response.end('<!doctype html><title>Gateway browser acceptance</title><h1>Disposable browser form</h1><form action="/receipt"><label>Acceptance code<input name="value"></label><button>Submit once</button></form>')
  }
})
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve))
const url = `http://127.0.0.1:${server.address().port}/form`
let agent
let session
let timer
let callbacks = 0
const calls = new Map()
try {
  agent = await Agent.spawn({ executable: process.env.SOPHON_RUNTIME,
    env: { ...process.env, GROK_HOME: join(root, 'home'), GROK_AUTH: '', GROK_TELEMETRY_ENABLED: 'false', GROK_TRACE_UPLOAD: 'false', GROK_FEEDBACK_ENABLED: 'false', GROK_TURN_SUMMARY: 'false' },
    config: { models: [{ id: dial, provider: { protocol: 'openai_responses', baseUrl: `${gateway}/v1`, apiKey: process.env.OG_API_KEY, model: route.model, headers: {}, queryParams: {} }, supportedReasoning, contextWindow, maxCompletionTokens }], defaultModel: dial, webSearchModel: null, sessionSummaryModel: null, compactionModel: null, imageDescriptionModel: null, media: null, subagents: [], browser: { executable: process.env.SOPHON_CHROMIUM ?? '/usr/bin/chromium', dataDir: join(root, 'identity'), artifactDir: join(root, 'evidence'), headless: true, noSandbox: true } },
    onCallback: async () => { callbacks++; throw new Error('Unexpected client callback') },
  })
  agent.subscribe(event => {
    if (event.type === 'history_record' && ['tool_call', 'tool_call_update'].includes(event.record.update.type)) {
      const call = event.record.update.value
      calls.set(call.id, { ...calls.get(call.id), ...call, rawInput: call.rawInput ?? calls.get(call.id)?.rawInput })
    }
  })
  session = await agent.createSession({ workspace: { id: 'gateway-browser', cwd: workspace }, requireConfigCandidate: true, model: dial, mcpServers: [], tools: [] })
  timer = setTimeout(() => { session.cancel('gateway-browser').catch(() => {}) }, 180000)
  const receipt = await session.prompt({ turnId: 'gateway-browser', configCandidate: { revision: `browser-${effective.revision}`, instructions: 'Use only the native browser tool for this acceptance task. Do not use shell, file, network, or delegation tools. Never retry submission. The page is a disposable local fixture. Do not access other websites.', skillDirectories: [], externalMcpServers: [], model: dial, reasoningEffort: route.reasoning_effort, subagentBriefs: [] }, blocks: [{ type: 'text', text: `Use browser new_tab to create a blank tab, then navigate it to ${url}. Take a semantic snapshot. Type exactly BROWSER_GATEWAY_73 into Acceptance code using its snapshot ref. Take a fresh snapshot, then click Submit once using its ref exactly once. Wait briefly and snapshot the receipt. Finally take a screenshot of the receipt with the browser tool, then report the artifact path. Do not claim to visually inspect the screenshot. Do not use other tools.` }] })
  clearTimeout(timer)
  assert.equal(receipt.stopReason, 'end_turn')
  assert.ok(pageLoads >= 1)
  assert.deepEqual(submitted, ['BROWSER_GATEWAY_73'])
  assert.equal(callbacks, 0)
  const actions = [...calls.values()].map(call => call.rawInput?.action).filter(Boolean)
  for (const action of ['new_tab', 'navigate', 'snapshot', 'type', 'click', 'screenshot']) assert.ok(actions.includes(action), `Missing native action ${action}`)
  const files = await readdir(join(workspace, '.native-browser'))
  const screenshots = files.filter(file => file.endsWith('.png'))
  assert.equal(screenshots.length, 1)
  const path = `.native-browser/${screenshots[0]}`
  const artifact = await session.readArtifact(path)
  const bytes = Buffer.from(artifact.base64, 'base64')
  assert.deepEqual([...bytes.subarray(0, 8)], [137, 80, 78, 71, 13, 10, 26, 10])
  assert.deepEqual(bytes, await readFile(join(workspace, path)))
  if (process.env.SOPHON_BROWSER_EVIDENCE_DIR) await writeFile(join(process.env.SOPHON_BROWSER_EVIDENCE_DIR, 'gateway-browser.png'), bytes)
  await agent.finalExit()
  agent = null
  console.log(JSON.stringify({ ok: true, realGateway: true, model: route.model, effectiveRevision: effective.revision, actions, submissions: submitted.length, hostToolCallbacks: callbacks, workspaceArtifact: true, pngBytes: bytes.length, checkedRuntimeExit: true }))
} catch (error) {
  const diagnostic = String(error.message).replaceAll(process.env.OG_API_KEY, '[credential]').replaceAll(gateway, '[gateway]').slice(0, 800)
  console.error(JSON.stringify({ ok: false, errorType: error.name, diagnostic, actions: [...calls.values()].map(call => ({ action: call.rawInput?.action, status: call.status })), submissions: submitted.length, callbacks }))
  process.exitCode = 1
} finally {
  clearTimeout(timer)
  if (agent) await agent.finalExit().catch(() => {})
  await new Promise(resolve => server.close(resolve))
  await rm(root, { recursive: true, force: true })
}
