// Opt-in real Agent/Jev/browser comparison. This is a test runner, not an agent loop.
// Only disposable observations go upstream. The relay forwards without retries.
import assert from 'node:assert/strict'
import { createServer } from 'node:http'
import { randomUUID } from 'node:crypto'
import { once } from 'node:events'
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
const { Agent } = await import(process.env.SOPHON_SDK_MODULE ?? '../../../packages/typescript/dist/index.js')

const gateway = process.env.OG_AI_GATEWAY?.replace(/\/+$/, '').replace(/\/v1$/, '')
assert.ok(gateway && process.env.OG_API_KEY && process.env.SOPHON_RUNTIME)
async function get(path) {
  const response = await fetch(`${gateway}${path}`, { redirect: 'error', signal: AbortSignal.timeout(30000), headers: { authorization: `Bearer ${process.env.OG_API_KEY}` } })
  assert.equal(response.status, 200)
  return response.json()
}
const [effective, catalog] = await Promise.all([get('/api/v1/product-config/effective'), get('/v1/models')])
assert.ok(effective.schema_version >= 2 && effective.revision >= 1, 'published decision routing required')
const route = effective.routes[effective.default_dial]
const decision = effective.routes.decision
assert.equal(route.status, 'available')
assert.equal(route.endpoint, 'openai-response')
assert.equal(decision?.status, 'available', 'no guessed decision model or fallback')
assert.equal(decision.endpoint, 'system-one')
const model = catalog.data.find(item => item.id === route.model)
assert.ok(model.supported_reasoning.includes(route.reasoning_effort))
assert.ok(model.supported_endpoint_types.includes(route.endpoint))
const decisionModel = catalog.data.find(item => item.id === decision.model)
assert.ok(decisionModel.supported_endpoint_types.includes('system-one'))

const criteria = {
  exchange: 'Click Exchange once only when the customer explicitly requests a size exchange and it has not already been completed.',
  none: 'Do nothing when the exchange has already been completed or no further action is requested.',
  insufficient_evidence: 'Do nothing and request clarification when it is unclear what remedy the customer wants.',
}
// Written before measurement; the deterministic baseline uses the same visible facts.
const cases = [
  { id: 'explicit-exchange', text: 'The shoes arrived in size 8 instead of 10. Please exchange them for size 10.', expected: 'exchange' },
  { id: 'already-completed', text: 'The shoes arrived in size 8 instead of 10. The size exchange has already been completed; no further action is requested.', expected: 'none' },
  { id: 'missing-remedy', text: 'There is a problem with my shoes. Please fix it.', expected: 'insufficient_evidence' },
]
function rule(text) {
  if (/already been completed|no further action/.test(text)) return 'none'
  if (/size 8 instead of 10/.test(text) && /Please exchange/.test(text)) return 'exchange'
  return 'insufficient_evidence'
}
const root = await mkdtemp(join(tmpdir(), 'sophon-browser-decisions-'))
const fixtureCases = new Map()
const receipts = new Map()
const relayCalls = []
let heldDecision = null
const bearer = randomUUID()
const relay = createServer(async (request, response) => {
  try {
    assert.equal(request.headers.authorization, `Bearer ${bearer}`)
    assert.ok(['/v1/responses', '/v1/systemone'].includes(request.url))
    const chunks = []
    for await (const chunk of request) chunks.push(chunk)
    const body = Buffer.concat(chunks)
    const payload = JSON.parse(body)
    const started = performance.now()
    const entry = { endpoint: request.url, model: payload.model, status: null, elapsedMs: null }
    relayCalls.push(entry)
    const controller = new AbortController()
    response.on('close', () => { if (!response.writableFinished) controller.abort() })
    const upstream = await fetch(`${gateway}${request.url}`, { method: 'POST', redirect: 'error', signal: controller.signal, headers: { authorization: `Bearer ${process.env.OG_API_KEY}`, 'content-type': 'application/json' }, body })
    entry.status = upstream.status
    if (request.url === '/v1/systemone' && heldDecision) await heldDecision()
    if (response.destroyed) { await upstream.body.cancel(); return }
    response.writeHead(upstream.status, { 'content-type': upstream.headers.get('content-type') ?? 'application/json' })
    for await (const chunk of upstream.body) {
      if (!response.write(chunk)) await once(response, 'drain', { signal: controller.signal })
    }
    response.end()
    entry.elapsedMs = Math.round(performance.now() - started)
  } catch {
    if (!response.headersSent) response.writeHead(502)
    response.end('{"error":"acceptance relay failed"}')
  }
})
const page = createServer((request, response) => {
  const url = new URL(request.url, 'http://fixture.invalid')
  const id = url.searchParams.get('id')
  if (url.pathname === '/effect') {
    receipts.set(id, (receipts.get(id) ?? 0) + 1)
    response.end('Exchange recorded')
  } else {
    response.setHeader('content-type', 'text/html')
    response.end(`<!doctype html><h1>Disposable support case</h1><p id="state">${fixtureCases.get(id)}</p><button onclick="window.effect=fetch('/effect?id=${id}').then(()=>document.querySelector('#state').textContent='Exchange completed')">Exchange</button>`)
  }
})
await Promise.all([new Promise(resolve => relay.listen(0, '127.0.0.1', resolve)), new Promise(resolve => page.listen(0, '127.0.0.1', resolve))])
const relayBase = `http://127.0.0.1:${relay.address().port}/v1`
const pageBase = `http://127.0.0.1:${page.address().port}`
const results = []
let agent
let active
let callbackCount = 0
async function run(mode, example, repetition, special = null) {
  const id = randomUUID()
  fixtureCases.set(id, example.text)
  const workspace = join(root, id)
  await mkdir(workspace)
  const tab = (await agent.browser({ action: 'new_tab', url: `${pageBase}/?id=${id}` })).tab_id
  await agent.browser({ action: 'wait', tab_id: tab, milliseconds: 100 })
  const started = performance.now()
  const relayStart = relayCalls.length
  const completions = []
  const failures = []
  let assistantText = ''
  active = { completions, failures, onText: text => { assistantText += text } }
  let receipt
  let selected
  let session
  let timer
  try {
    if (mode === 'rules') {
      const snapshot = await agent.browser({ action: 'snapshot', tab_id: tab })
      const visible = snapshot.nodes.filter(node => node.role === 'StaticText').map(node => node.name).join(' ')
      selected = rule(visible)
      if (selected === 'exchange') await agent.browser({ action: 'click', tab_id: tab, ref: snapshot.nodes.find(node => node.role === 'button' && node.name === 'Exchange').ref })
    } else {
      session = await agent.createSession({ workspace: { id, cwd: workspace }, model: 'acceptance-main', mcpServers: [], tools: [] })
      let release
      let decisionEntered
      const entered = new Promise(resolve => { decisionEntered = resolve })
      const gate = new Promise(resolve => { release = resolve })
      if (special) heldDecision = async () => {
        if (special === 'stale') await agent.browser({ action: 'evaluate', tab_id: tab, expression: 'document.querySelector("#state").textContent="Exchange has already been completed; no further action is requested"' })
        decisionEntered()
        if (special === 'cancel') await gate
      }
      timer = setTimeout(() => session.cancel(id).catch(() => {}), 180000)
      const prompt = session.prompt({ turnId: id, configCandidate: { revision: `decision-${effective.revision}`, model: 'acceptance-main', reasoningEffort: route.reasoning_effort, instructions: 'Only browser and evaluate_decisions tools are permitted. Use exactly one initial snapshot. Never retry an interaction. On stale_ref stop without executing another action. Do not access other pages or hidden state.', skillDirectories: [], externalMcpServers: [], subagentBriefs: [] }, blocks: [{ type: 'text', text: `Observe tab ${tab} with browser snapshot. Decide one next action from ${JSON.stringify(criteria)}. ${mode === 'jev' ? 'Call evaluate_decisions exactly once with the observed snapshot and its snapshot_id as state; ask one choice question named next_action using these exact criteria. Use its selected choice without a second decision call.' : 'Choose using your own reasoning; do not call evaluate_decisions.'} If exchange, click its button once using the initial snapshot ref. If none or insufficient_evidence, do not click. Return only the selected option ID. A stale_ref error means stop and report stale_ref, not reobserve or retry.` }] })
      if (special === 'cancel') {
        await Promise.race([entered, prompt.then(() => { throw new Error('turn ended before decision barrier') })])
        await session.cancel(id)
        release()
      }
      receipt = await prompt
      clearTimeout(timer)
      const decisionOutput = completions.find(call => call.rawOutput?.value?.answers?.next_action)?.rawOutput.value
      selected = mode === 'jev' ? decisionOutput?.answers.next_action.choice : assistantText.trim()
      if (!special) {
        assert.equal(receipt.stopReason, 'end_turn')
        assert.equal(selected, example.expected)
        if (mode === 'jev') {
          assert.equal(completions.filter(call => call.rawOutput?.value?.answers).length, 1)
          assert.equal(decisionOutput.requestedModel, decision.model)
        }
      } else if (special === 'cancel') assert.equal(receipt.stopReason, 'cancelled')
      else assert.ok(failures.some(call => JSON.stringify(call.rawOutput).includes('stale_ref') || JSON.stringify(call.rawOutput).includes('stale')))
    }
    // This host read checks effects; it does not direct the Agent or poll a scene.
    if (!special) assert.equal(selected, example.expected)
    await agent.browser({ action: 'evaluate', tab_id: tab, expression: 'Promise.resolve(window.effect).then(()=>document.querySelector("#state").textContent)' })
    assert.equal(receipts.get(id) ?? 0, !special && example.expected === 'exchange' ? 1 : 0)
    assert.equal(callbackCount, 0)
    const decisions = completions.filter(call => call.rawOutput?.value?.answers).map(call => call.rawOutput.value)
    const record = { mode, case: example.id, repetition, special, ok: true, selected, elapsedMs: Math.round(performance.now() - started), observedEffects: receipts.get(id) ?? 0, nativeCompletedCalls: completions.length, mainUsage: receipt?.usage ?? null, decisions: decisions.map(value => ({ elapsedMs: value.elapsedMs, model: value.model, usage: value.usage, answers: value.answers })), relayCalls: relayCalls.slice(relayStart) }
    results.push(record)
    console.log(JSON.stringify(record))
  } catch (error) {
    const record = { mode, case: example.id, repetition, special, ok: false, elapsedMs: Math.round(performance.now() - started), observedEffects: receipts.get(id) ?? 0, mainUsage: receipt?.usage ?? null, decisions: completions.filter(call => call.rawOutput?.value?.answers).map(call => call.rawOutput.value), nativeCompletedCalls: completions.length, relayCalls: relayCalls.slice(relayStart), diagnostic: String(error.message).replaceAll(process.env.OG_API_KEY, '[credential]').replaceAll(gateway, '[gateway]').slice(0, 1000) }
    results.push(record)
    console.log(JSON.stringify(record))
  } finally {
    clearTimeout(timer)
    heldDecision = null
    active = null
    if (session) await session.dispose()
    await agent.browser({ action: 'close_tab', tab_id: tab })
  }
}

try {
  await mkdir(join(root, 'home'))
  await writeFile(join(root, 'home/config.toml'), '[features]\nsession_recap = false\n')
  await writeFile(join(root, 'home/managed_config.toml'), 'plugin_auto_update = false\n')
  agent = await Agent.spawn({ executable: process.env.SOPHON_RUNTIME,
    env: { ...process.env, GROK_HOME: join(root, 'home'), GROK_AUTH: '', GROK_TELEMETRY_ENABLED: 'false', GROK_TRACE_UPLOAD: 'false', GROK_FEEDBACK_ENABLED: 'false', GROK_TURN_SUMMARY: 'false' },
    config: { models: [{ id: 'acceptance-main', provider: { protocol: 'openai_responses', baseUrl: relayBase, apiKey: bearer, model: route.model, headers: {}, queryParams: {} }, supportedReasoning: model.supported_reasoning, contextWindow: model.limit.context, maxCompletionTokens: model.limit.output }], defaultModel: 'acceptance-main', decision: { endpoint: 'system-one', model: decision.model, baseUrl: relayBase, bearerToken: bearer }, webSearchModel: null, sessionSummaryModel: null, compactionModel: null, imageDescriptionModel: null, media: null, subagents: [], browser: { executable: process.env.SOPHON_CHROMIUM ?? '/usr/bin/chromium', dataDir: join(root, 'identity'), artifactDir: join(root, 'evidence'), headless: true, noSandbox: true } },
    onCallback: async () => { callbackCount++; throw new Error('unexpected callback') },
  })
  agent.subscribe(event => {
    if (!active || event.type !== 'session') return
    if (event.update.type === 'assistant_text') active.onText(event.update.value)
    if (event.update.type === 'tool_call_update') {
      if (event.update.value.status === 'completed') active.completions.push(event.update.value)
      if (event.update.value.status === 'failed') active.failures.push(event.update.value)
    }
  })
  for (let repetition = 0; repetition < 2; repetition++) {
    for (const example of cases) {
      for (const mode of repetition === 0 ? ['rules', 'baseline', 'jev'] : ['jev', 'baseline', 'rules']) await run(mode, example, repetition)
    }
  }
  await run('jev', cases[0], 0, 'stale')
  await run('jev', cases[0], 0, 'cancel')
  await agent.finalExit()
  agent = null
  assert.ok(results.every(result => result.ok), 'one or more comparison/freshness/cancellation cases failed; retained in evidence')
  console.log(JSON.stringify({ ok: true, completedCases: results.length, conclusions: 'Small repeated correctness comparison only; no single-run acceleration claim.' }))
} catch (error) {
  console.error(JSON.stringify({ ok: false, completedCases: results.length, diagnostic: String(error.message).replaceAll(process.env.OG_API_KEY, '[credential]').replaceAll(gateway, '[gateway]').slice(0, 1000) }))
  process.exitCode = 1
} finally {
  if (process.env.SOPHON_BROWSER_EVIDENCE_DIR) await writeFile(join(process.env.SOPHON_BROWSER_EVIDENCE_DIR, 'browser-decisions.json'), JSON.stringify({ effectiveRevision: effective.revision, mainModel: route.model, decisionModel: decision.model, results }, null, 2))
  if (agent) await agent.finalExit().catch(() => {})
  relay.closeAllConnections()
  await Promise.all([new Promise(resolve => relay.close(resolve)), new Promise(resolve => page.close(resolve))])
  await rm(root, { recursive: true, force: true })
}
