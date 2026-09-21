import assert from 'node:assert/strict'
import { test } from 'node:test'
import { createServer } from 'node:http'
import { Readable } from 'node:stream'
import { randomUUID } from 'node:crypto'
import { mkdtemp, mkdir, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { Agent } from '../dist/index.js'

const executable = process.env.SOPHON_RUNTIME ?? resolve('../../target/debug/sophon-runtime')
const questions = {
  ready: { type: 'noul', instructions: { question: 'Does the supplied evidence establish readiness?' }, criteria: { true: 'Measured success', false: ['Failure or missing evidence'] } },
  action: { type: 'choice', instructions: 'Choose the next evidence-based action.', criteria: { inspect: { action: 'inspect failed input' }, none: null } },
  severity: { type: 'score', instructions: ['Rate the impact'], criteria: ['cosmetic', { blocked: true }, ['unusable']] },
}
const answer = {
  model: 'actual-decision-version', usage: { input_tokens: 137, output_tokens: 29 },
  answers: {
    ready: { type: 'noul', noul: 0.13 },
    action: { type: 'choice', choice: 'inspect', confidence: 0.71, probabilities: { inspect: 0.83, none: 0.17 } },
    severity: { type: 'score', score: 1.55, confidence: 0.42, probabilities: { 0: 0.1, 1: 0.25, 2: 0.65 }, legend: { 0: 'cosmetic', 1: { blocked: true }, 2: ['unusable'] } },
  },
}

test('native decisions validate, correlate, refuse, cancel and time out without paid retries', { timeout: 100000 }, async t => {
  const root = await mkdtemp(join(tmpdir(), 'sophon-decisions-'))
  t.after(() => rm(root, { recursive: true, force: true }))
  const cwd = join(root, 'workspace'); await mkdir(cwd)
  let mode = 'ok'
  let reached
  const counts = new Map()
  const correlations = new Map()
  let hostCalls = 0
  const server = createServer(async (request, response) => {
    let text = ''; for await (const chunk of request) text += chunk
    const body = JSON.parse(text)
    if (request.url === '/v1/systemone') {
      assert.equal(request.headers.authorization, 'Bearer relay-secret-never-output')
      assert.equal(body.model, 'published-decision-route')
      assert.deepEqual(body.questions, questions)
      counts.set(mode, (counts.get(mode) ?? 0) + 1)
      correlations.set(mode, request.headers['x-client-request-id'])
      reached?.()
      if (mode === 'cancel' || mode === 'timeout') return
      if (mode === 'http') {
        response.writeHead(529); response.end('secret-reflected-body-must-not-escape'); return
      }
      if (mode === 'redirect') {
        response.writeHead(307, { location: '/must-not-follow' }); response.end(); return
      }
      const value = structuredClone(answer)
      if (mode === 'malformed') value.answers.action.probabilities.none = 0.01
      response.writeHead(200, { 'content-type': 'application/json' }); response.end(JSON.stringify(value)); return
    }
    assert.equal(request.url, '/v1/chat/completions')
    const tool = body.tools?.find(tool => tool.function?.name === 'evaluate_decisions')
    assert.equal(Boolean(tool), mode !== 'absent', 'only configured native decisions are advertised')
    const finished = mode === 'absent' || body.messages.some(message => message.role === 'tool')
    const args = { state: mode === 'invalid' ? 7 : { mode, evidence: 'Input failed; inspect it.' }, questions }
    const delta = finished ? { role: 'assistant', content: 'Judgment is advisory; no action executed.' }
      : { role: 'assistant', tool_calls: [{ index: 0, id: `decision-${mode}`, type: 'function', function: { name: 'evaluate_decisions', arguments: JSON.stringify(args) } }] }
    const chunk = (delta, finish_reason) => ({ id: 'decision-fixture', object: 'chat.completion.chunk', created: 1, model: 'text-fixture', choices: [{ index: 0, delta, finish_reason }] })
    response.writeHead(200, { 'content-type': 'text/event-stream' })
    response.end(`data: ${JSON.stringify(chunk(delta, null))}\n\ndata: ${JSON.stringify(chunk({}, finished ? 'stop' : 'tool_calls'))}\n\ndata: [DONE]\n\n`)
  })
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve))
  t.after(() => new Promise(resolve => { server.closeAllConnections(); server.close(resolve) }))
  const baseUrl = `http://127.0.0.1:${server.address().port}/v1`
  const config = {
    models: [{ id: 'main', provider: { protocol: 'openai_chat', baseUrl, apiKey: 'text-fixture', model: 'text-fixture', headers: {}, queryParams: {} }, supportedReasoning: [] }],
    defaultModel: 'main', decision: { endpoint: 'system-one', model: 'published-decision-route', baseUrl, bearerToken: 'relay-secret-never-output' },
    subagents: [],
  }
  const env = { GROK_HOME: join(root, 'home'), GROK_AUTH: '', GROK_TELEMETRY_ENABLED: 'false', GROK_TRACE_UPLOAD: 'false', GROK_TURN_SUMMARY: 'false', GROK_FEEDBACK_ENABLED: 'false', GROK_MAX_RETRIES: '0' }
  for (const decision of [{ ...config.decision, endpoint: 'openai-response' }, { ...config.decision, baseUrl: 'https://example.test/wrong' }, { ...config.decision, bearerToken: '' }]) {
    await assert.rejects(Agent.spawn({ executable, config: { ...config, decision }, env }))
  }
  const agent = await Agent.spawn({ executable, config, env, onToolCall: () => { hostCalls++; throw new Error('native decisions must not call host') } })
  t.after(() => agent.finalExit().catch(() => {}))
  config.decision.model = 'MUTATED_HOST_OBJECT_MUST_NOT_REBIND'
  const events = []; agent.subscribe(event => events.push(event))
  for (mode of ['ok', 'invalid', 'malformed', 'http', 'redirect', 'cancel', 'timeout']) {
    const session = await agent.createSession({ workspace: { id: mode, cwd }, model: 'main', requireConfigCandidate: true, mcpServers: [], tools: [] })
    const before = events.length
    const submitted = new Promise(resolve => { reached = resolve })
    const turn = session.prompt({ turnId: `turn-${mode}`, configCandidate: { revision: `mount-${mode}`, model: 'main', instructions: 'Use the advisory decision tool once.', skillDirectories: [], externalMcpServers: [], reasoningEffort: null, subagentBriefs: [] }, blocks: [{ type: 'text', text: 'Evaluate the evidence.' }] })
    if (mode === 'cancel') { await submitted; await session.cancel() }
    const completed = await turn
    assert.equal(completed.stopReason, mode === 'cancel' ? 'cancelled' : 'end_turn')
    assert.equal((await session.effectiveConfig()).mountedRevision, `mount-${mode}`)
    const updates = events.slice(before).filter(event => event.type === 'session' && event.update.type === 'tool_call_update').map(event => event.update.value)
    const output = updates.find(update => update.status === 'completed' && update.rawOutput?.type === 'Dynamic')?.rawOutput.value
    if (mode === 'ok') {
      assert.deepEqual(output.answers, answer.answers)
      assert.deepEqual(output.usage, answer.usage)
      assert.equal(output.model, answer.model)
      assert.equal(output.requestedModel, 'published-decision-route')
      assert.equal(output.requestId, correlations.get(mode))
      assert.equal(output.toolCallId, 'decision-ok')
      assert.ok(Number.isInteger(output.elapsedMs) && output.elapsedMs >= 0)
    } else {
      assert.equal(output, undefined)
      if (mode !== 'cancel') assert.ok(updates.some(update => update.status === 'failed'), mode)
      if (mode === 'timeout') assert.match(JSON.stringify(updates), /outcome=unknown/)
    }
    assert.equal(counts.get(mode) ?? 0, mode === 'invalid' ? 0 : 1, `${mode}: never automatically replay`)
    await session.dispose()
  }
  assert.equal(hostCalls, 0)
  assert.doesNotMatch(JSON.stringify(events), /relay-secret-never-output|secret-reflected-body-must-not-escape/)
  await agent.finalExit()
  mode = 'absent'
  const unavailable = await Agent.spawn({ executable, config: { ...config, decision: undefined }, env })
  t.after(() => unavailable.finalExit().catch(() => {}))
  const idle = await unavailable.createSession({ workspace: { id: 'absent', cwd }, model: 'main', mcpServers: [], tools: [] })
  assert.equal((await idle.prompt({ turnId: 'absent', blocks: [{ type: 'text', text: 'No judgment route is configured.' }] })).stopReason, 'end_turn')
  assert.equal(counts.get('absent') ?? 0, 0)
  await idle.dispose(); await unavailable.finalExit()
})

test('published decision route executes all three judgments through a real main Agent and relay', { skip: process.env.SOPHON_LIVE_DECISIONS !== '1', timeout: 180000 }, async t => {
  assert.ok(process.env.OG_AI_GATEWAY && process.env.OG_API_KEY)
  const origin = process.env.OG_AI_GATEWAY.replace(/\/$/, '')
  const headers = { authorization: `Bearer ${process.env.OG_API_KEY}` }
  const effectiveResponse = await fetch(`${origin}/api/v1/product-config/effective`, { headers, signal: AbortSignal.timeout(15000) })
  assert.equal(effectiveResponse.status, 200)
  const effective = await effectiveResponse.json()
  assert.equal(effective.schema_version, 2, 'requires explicitly published schema2; no legacy fallback')
  const decision = effective.routes.decision
  assert.equal(decision.status, 'available', 'decision must be published, never guessed from catalog')
  assert.equal(decision.endpoint, 'system-one')
  assert.equal(decision.reasoning_effort, null)
  const main = effective.routes.deep
  assert.equal(main.status, 'available'); assert.equal(main.endpoint, 'openai-response')
  const catalogResponse = await fetch(`${origin}/v1/models`, { headers, signal: AbortSignal.timeout(15000) })
  assert.equal(catalogResponse.status, 200)
  const catalog = await catalogResponse.json()
  const declared = catalog.data.filter(model => model.id === main.model)
  assert.equal(declared.length, 1)
  assert.ok(declared[0].supported_reasoning.includes(main.reasoning_effort))
  assert.equal(catalog.data.filter(model => model.id === decision.model).length, 1)
  const relayToken = randomUUID()
  const state = { nonce: randomUUID(), build: 'failed', observation: 'Required input controls are unimplemented. No successful playable run has been observed.' }
  const wire = []
  let posted
  const relay = createServer(async (request, response) => {
    if (request.headers.authorization !== `Bearer ${relayToken}` || request.method !== 'POST' || !['/v1/responses', '/v1/systemone'].includes(request.url)) {
      response.writeHead(401); response.end(); return
    }
    const abort = new AbortController(); response.on('close', () => abort.abort())
    try {
      let raw = ''; for await (const chunk of request) raw += chunk
      const body = JSON.parse(raw)
      if (request.url === '/v1/systemone') { posted = body; assert.equal(body.model, decision.model) }
      const entry = { path: request.url, model: body.model, status: null }; wire.push(entry)
      const upstream = await fetch(`${origin}${request.url}`, { method: 'POST', headers: { ...headers, 'content-type': 'application/json' }, body: raw, signal: abort.signal })
      entry.status = upstream.status
      response.writeHead(upstream.status, { 'content-type': upstream.headers.get('content-type') ?? 'application/json' })
      Readable.fromWeb(upstream.body).on('error', () => response.destroy()).pipe(response)
    } catch { if (!response.destroyed) { response.writeHead(502); response.end('{}') } }
  })
  await new Promise(resolve => relay.listen(0, '127.0.0.1', resolve))
  const root = await mkdtemp(join(tmpdir(), 'sophon-live-decisions-'))
  const cwd = join(root, 'workspace'); await mkdir(cwd)
  let agent
  t.after(async () => {
    try { if (agent) await agent.finalExit() } finally {
      await new Promise(resolve => { relay.closeAllConnections(); relay.close(resolve) })
      await rm(root, { recursive: true, force: true })
      t.diagnostic(JSON.stringify({ schema: effective.schema_version, revision: effective.revision, wire }))
    }
  })
  const baseUrl = `http://127.0.0.1:${relay.address().port}/v1`
  let callbacks = 0
  agent = await Agent.spawn({ executable, config: {
    models: [{ id: 'deep', provider: { protocol: 'openai_responses', baseUrl, apiKey: relayToken, model: main.model, headers: {}, queryParams: {} }, supportedReasoning: declared[0].supported_reasoning, contextWindow: declared[0].limit.context, maxCompletionTokens: 4096 }],
    defaultModel: 'deep', decision: { endpoint: 'system-one', model: decision.model, baseUrl, bearerToken: relayToken }, subagents: [],
  }, env: { GROK_HOME: join(root, 'native-home'), GROK_AUTH: '', GROK_MAX_RETRIES: '0', GROK_TELEMETRY_ENABLED: 'false', GROK_TRACE_UPLOAD: 'false', GROK_TURN_SUMMARY: 'false', GROK_FEEDBACK_ENABLED: 'false' }, onToolCall: () => { callbacks++; throw new Error('No host decision callback') } })
  const session = await agent.createSession({ workspace: { id: 'live-decisions', cwd }, model: 'deep', requireConfigCandidate: true, mcpServers: [], tools: [] })
  const events = []; session.subscribe(event => events.push(event))
  const receipt = await session.prompt({ turnId: 'decision-live', configCandidate: { revision: 'decision-A', model: 'deep', reasoningEffort: main.reasoning_effort, instructions: 'Use evaluate_decisions once for the supplied advisory batch. Do not execute actions or call other tools. Never retry a failed or uncertain decision request.', skillDirectories: [], externalMcpServers: [], subagentBriefs: [] }, blocks: [{ type: 'text', text: `Call evaluate_decisions exactly once with these exact arguments, then report the judgments as advisory, not verified success: ${JSON.stringify({ state, questions })}` }] })
  assert.equal(receipt.stopReason, 'end_turn')
  assert.equal((await session.effectiveConfig()).mountedRevision, 'decision-A')
  assert.equal(callbacks, 0)
  assert.deepEqual(posted?.state, state); assert.deepEqual(posted?.questions, questions)
  const results = events.filter(event => event.type === 'session' && event.update.type === 'tool_call_update' && event.update.value.status === 'completed' && event.update.value.rawOutput?.type === 'Dynamic').map(event => event.update.value.rawOutput.value).filter(value => value.requestedModel === decision.model)
  assert.equal(results.length, 1, 'exactly one validated native decision completion required')
  const result = results[0]
  assert.deepEqual(Object.keys(result.answers).sort(), ['action', 'ready', 'severity'])
  assert.ok(result.answers.ready.noul < 0.5, 'failed controls do not establish readiness')
  assert.equal(result.answers.action.choice, 'inspect')
  assert.equal(result.answers.severity.type, 'score')
  assert.ok(result.usage.input_tokens > 0 && result.usage.output_tokens > 0)
  assert.equal(wire.filter(entry => entry.path === '/v1/systemone').length, 1)
  assert.ok(wire.every(entry => entry.status === 200))
  assert.doesNotMatch(JSON.stringify(events), new RegExp(relayToken))
  t.diagnostic(JSON.stringify({ model: result.model, requestedModel: result.requestedModel, usage: result.usage, elapsedMs: result.elapsedMs, requestId: result.requestId, answers: result.answers }))
  await session.dispose(); await agent.finalExit(); agent = undefined
})
