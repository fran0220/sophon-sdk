import assert from 'node:assert/strict'
import { test } from 'node:test'
import { mkdtemp, mkdir, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { resolve, join } from 'node:path'
import { createServer } from 'node:http'
import { Agent } from '../dist/index.js'

const executable = process.env.SOPHON_RUNTIME ?? resolve('../../target/debug/sophon-runtime')

async function setup(t) {
  const root = await mkdtemp(join(tmpdir(), 'sophon-runtime-test-'))
  const cwd = join(root, 'workspace')
  await mkdir(cwd)
  t.after(() => rm(root, { recursive: true, force: true }))
  const config = {
    models: [{ id: 'runtime-test', provider: {
      protocol: process.env.SOPHON_TEST_PROTOCOL ?? 'openai_responses', baseUrl: `${(process.env.OG_HOST ?? 'https://origingame.dev').replace(/\/$/, '')}/gw/v1`,
      apiKey: process.env.OG_API_KEY ?? 'unused-no-inference-route',
      model: process.env.SOPHON_TEST_MODEL ?? 'gpt-5.4', headers: {}, queryParams: {},
    }, contextWindow: 200000, maxCompletionTokens: 4096 }],
    defaultModel: 'runtime-test', webSearchModel: null, sessionSummaryModel: null,
    imageDescriptionModel: null, browser: null, media: null,
    subagents: [{ name: 'refiner', description: 'Fixed no-tools refinement', instructions: 'Return only refinement text.', model: 'runtime-test', tools: [] }],
  }
  return { root, cwd, config, env: { GROK_HOME: join(root, 'native-home'), GROK_TELEMETRY_ENABLED: 'false', GROK_TRACE_UPLOAD: 'false', GROK_TURN_SUMMARY: 'false', GROK_FEEDBACK_ENABLED: 'false' } }
}

test('real stdio Runtime creates, snapshots, schedules, reloads and checks process exit', { timeout: 60000 }, async t => {
  const { cwd, config, env } = await setup(t)
  const agent = await Agent.spawn({ executable, config, env })
  t.after(() => agent.finalExit().catch(() => {}))
  const events = []
  agent.subscribe(event => events.push(event))
  const options = { workspace: { id: 'asymmetric-workspace', cwd }, model: 'runtime-test', metadata: {}, mcpServers: [], tools: [] }
  const session = await agent.createSession(options)
  assert.match(session.id, /.+/)
  const snapshot = await session.history()
  assert.equal(snapshot.sessionId, session.id)
  assert.ok(events.some(event => event.type === 'history_boundary' && event.boundaryId === snapshot.boundaryId))
  const queue = await session.queue()
  assert.equal(queue.running, null)
  assert.deepEqual(queue.pending, [])
  await assert.rejects(session.prompt({ turnId: 'unsupported-candidate', blocks: [{ type: 'text', text: 'Must never execute.' }], metadata: { 'x.sophon/configCandidate': { instructions: 'Changed instructions', model: 'unpublished-model' } } }), /configCandidate|config_candidate|candidate/i)
  assert.equal((await session.history()).records.some(record => record.promptId === 'unsupported-candidate'), false)
  const childId = await session.subagents.newId()
  assert.match(childId, /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/)
  assert.deepEqual(await session.subagents.cancelId(childId), { subagentId: childId, fenced: true, outcome: { kind: 'not_found' } })
  const cancelled = await session.subagents.start({ id: childId, prompt: 'Never infer.', description: 'cancelled before registration', subagentType: 'refiner', cwd: null, model: 'runtime-test' })
  assert.equal(cancelled.state, 'cancelled')
  assert.equal(cancelled.attemptId, null)
  const before = await session.scheduler.list()
  const cadence = { kind: 'once', at: new Date(Date.now() + 3600000).toISOString() }
  const created = await session.scheduler.create('schedule-a', before.version, { cadence, prompt: 'future explicit scheduled task', durable: true })
  assert.equal(created.type, 'committed')
  const conflict = await session.scheduler.create('schedule-stale', before.version, { cadence, prompt: 'must not commit', durable: true })
  assert.equal(conflict.type, 'conflict')
  const removed = await session.scheduler.delete('schedule-remove', created.version, created.value.id)
  assert.equal(removed.type, 'committed')
  assert.equal(removed.value, true)
  await session.cancel('never-admitted')
  const id = session.id
  await session.dispose()
  const loaded = await agent.loadSession(id, options)
  assert.equal(loaded.id, id)
  assert.equal((await loaded.history()).sessionId, id)
  await loaded.dispose()
  const report = await agent.quiesce()
  assert.equal(report.drained, true)
  await agent.finalExit()
})

test('native terminal survives stdio transport through resize, input, output and reaped exit', { timeout: 30000 }, async t => {
  const { cwd, config, env } = await setup(t)
  const agent = await Agent.spawn({ executable, config, env })
  t.after(() => agent.finalExit().catch(() => {}))
  const events = []
  agent.subscribeTerminalEvents(event => events.push(event))
  const exited = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('Terminal exit not observed')), 5000)
    agent.subscribeTerminalEvents(event => { if (event.type === 'exit') { clearTimeout(timer); resolve(event) } })
  })
  const { terminalId } = await agent.terminal({ action: 'open', program: '/bin/sh', args: ['-c', 'read line; stty size; printf "RECEIPT_%s\\n" "$line"; exit 7'], cwd, env: {}, cols: 80, rows: 24 })
  await agent.terminal({ action: 'resize', terminalId, cols: 103, rows: 37 })
  await agent.terminal({ action: 'write', terminalId, data: Buffer.from('INPUT_29\n').toString('base64') })
  await exited
  const receipt = await agent.terminal({ action: 'close', terminalId })
  assert.equal(receipt.exitCode, 7)
  await agent.finalExit()
  assert.equal(events.some(event => event.type === 'gap' || event.type === 'error'), false)
  const output = events.filter(event => event.type === 'output' && event.terminalId === terminalId)
  let offset = 0
  const bytes = output.map(event => {
    const bytes = Buffer.from(event.data, 'base64')
    offset += bytes.length
    assert.equal(event.offset, offset)
    return bytes
  })
  assert.match(Buffer.concat(bytes).toString(), /37 103/)
  assert.match(Buffer.concat(bytes).toString(), /RECEIPT_INPUT_29/)
  assert.ok(events.some(event => event.type === 'exit' && event.terminalId === terminalId && event.exitCode === 7))
})

test('fixed no-tools refiner runs before any parent prompt against its explicit model and baseline', { timeout: 60000 }, async t => {
  const { cwd, config, env } = await setup(t)
  const requests = []
  const server = createServer(async (request, response) => {
    let body = ''
    for await (const chunk of request) body += chunk
    requests.push(JSON.parse(body))
    const chunk = (delta, finish_reason) => ({ id: 'refiner-fixture', object: 'chat.completion.chunk', created: 1, model: 'distill-wire', choices: [{ index: 0, delta, finish_reason }] })
    response.writeHead(200, { 'content-type': 'text/event-stream' })
    response.end(`data: ${JSON.stringify(chunk({ role: 'assistant', content: 'REFINEMENT_73' }, null))}\n\ndata: ${JSON.stringify(chunk({}, 'stop'))}\n\ndata: [DONE]\n\n`)
  })
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve))
  t.after(() => new Promise(resolve => { server.closeAllConnections(); server.close(resolve) }))
  config.models[0].provider = { protocol: 'openai_chat', baseUrl: `http://127.0.0.1:${server.address().port}`, apiKey: 'local-fixture', model: 'main-wire', headers: {}, queryParams: {} }
  config.models.push({ ...config.models[0], id: 'distillation', provider: { ...config.models[0].provider, model: 'distill-wire' } })
  config.subagents[0] = { name: 'refiner', description: 'Product-owned fixed refiner', instructions: 'BASELINE_REFINEMENT_73. Return the requested refinement.', model: 'distillation', tools: [] }
  const agent = await Agent.spawn({ executable, config, env, onCallback: () => { throw new Error('No tools allowed') } })
  t.after(() => agent.finalExit().catch(() => {}))
  const session = await agent.createSession({ workspace: { id: 'refiner-before-parent', cwd }, model: 'runtime-test', metadata: {}, mcpServers: [], tools: [{ name: 'product_echo', description: 'Must not inherit into refiner', inputSchema: { type: 'object', properties: {} } }] })
  const result = await session.subagents.start({ id: await session.subagents.newId(), prompt: 'EVIDENCE_29. Refine this trajectory.', description: 'Fixed refiner test', subagentType: 'refiner', cwd: null, model: 'distillation' })
  assert.equal(result.state, 'completed', result.error ?? result.output)
  assert.match(result.output, /REFINEMENT_73/)
  const refinement = requests.find(request => JSON.stringify(request.messages).includes('BASELINE_REFINEMENT_73'))
  assert.ok(refinement, 'actual inference must receive the fixed baseline')
  assert.equal(refinement.model, 'distill-wire', JSON.stringify(requests.map(request => ({ model: request.model, tools: request.tools?.length ?? 0, baseline: JSON.stringify(request.messages).includes('BASELINE_REFINEMENT_73') }))))
  assert.match(JSON.stringify(refinement.messages), /EVIDENCE_29/)
  assert.equal(refinement.tools?.length ?? 0, 0)
  await agent.finalExit()
})

test('ordinary and scheduled children retain callback owner, workspace, source and cancellation', { timeout: 60000 }, async t => {
  const { root, cwd, config, env } = await setup(t)
  const childCwd = join(root, 'child-workspace')
  await mkdir(childCwd)
  const server = createServer(async (request, response) => {
    let body = ''
    for await (const chunk of request) body += chunk
    const payload = JSON.parse(body)
    const answered = payload.messages?.some(message => message.role === 'tool' && JSON.stringify(message.content).includes('ANSWER_29'))
    const ask = payload.tools?.some(tool => tool.function?.name === 'ask_user') && !answered
    const delta = ask ? { role: 'assistant', tool_calls: [{ index: 0, id: 'question29', type: 'function', function: { name: 'ask_user', arguments: '{"question":"Choose 29?"}' } }] } : { role: 'assistant', content: 'CHILD_COMPLETE_73' }
    const chunk = (delta, finish_reason) => ({ id: 'child-fixture', object: 'chat.completion.chunk', created: 1, model: 'child-wire', choices: [{ index: 0, delta, finish_reason }] })
    response.writeHead(200, { 'content-type': 'text/event-stream' })
    response.end(`data: ${JSON.stringify(chunk(delta, null))}\n\ndata: ${JSON.stringify(chunk({}, ask ? 'tool_calls' : 'stop'))}\n\ndata: [DONE]\n\n`)
  })
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve))
  t.after(() => new Promise(resolve => { server.closeAllConnections(); server.close(resolve) }))
  config.models[0].provider = { protocol: 'openai_chat', baseUrl: `http://127.0.0.1:${server.address().port}`, apiKey: 'local-fixture', model: 'child-wire', headers: {}, queryParams: {} }
  config.subagents.push({ name: 'task', description: 'Fixed task worker', instructions: 'Ask the user then finish.', model: 'runtime-test', tools: null })
  let receive
  let nextQuestion = new Promise(resolve => { receive = resolve })
  const agent = await Agent.spawn({ executable, config, env, onCallback: request => new Promise(resolve => {
    assert.equal(request.method, 'tool/ask_user')
    receive({ request, answer: () => resolve({ answer: 'ANSWER_29' }) })
    request.signal.addEventListener('abort', () => resolve({ cancelled: true }), { once: true })
  }) })
  t.after(() => agent.finalExit().catch(() => {}))
  const session = await agent.createSession({ workspace: { id: 'callback-parent', cwd }, model: 'runtime-test', metadata: {}, mcpServers: [], tools: [{ name: 'ask_user', description: 'Ask a human question', inputSchema: { type: 'object', properties: { question: { type: 'string' } }, required: ['question'] } }] })
  const ordinary = session.subagents.start({ id: await session.subagents.newId(), prompt: 'Ask the human.', description: 'ordinary child', subagentType: 'task', cwd: childCwd, model: 'runtime-test' })
  const first = await nextQuestion
  assert.equal(first.request.context.ownerSessionId, session.id)
  assert.notEqual(first.request.context.sessionId, session.id)
  assert.equal(first.request.context.cwd, childCwd)
  assert.equal(first.request.context.scheduledInvocation, null)
  first.answer()
  assert.equal((await ordinary).state, 'completed')
  for (const cancel of [false, true]) {
    nextQuestion = new Promise(resolve => { receive = resolve })
    let fired
    const child = new Promise(resolve => { fired = resolve })
    const unsubscribe = session.subscribe(event => {
      if (event.type === 'scheduler' && event.occurrence.type === 'fired') fired(event.occurrence.subagentId)
    })
    const before = await session.scheduler.list()
    const at = new Date(Date.now() - 1000).toISOString()
    const created = await session.scheduler.create(`question-${cancel}`, before.version, { cadence: { kind: 'once', at }, prompt: 'Ask the human before finishing.', durable: true })
    assert.equal(created.type, 'committed')
    const question = await nextQuestion
    const context = question.request.context
    assert.equal(context.ownerSessionId, session.id)
    assert.notEqual(context.sessionId, session.id)
    assert.equal(context.cwd, cwd)
    assert.equal(context.scheduledInvocation.sessionId, session.id)
    assert.equal(context.scheduledInvocation.taskId, created.value.id)
    assert.equal(Date.parse(context.scheduledInvocation.occurrence), Date.parse(at))
    const id = await child
    unsubscribe()
    if (cancel) {
      const aborted = new Promise(resolve => question.request.signal.addEventListener('abort', resolve, { once: true }))
      assert.equal((await session.subagents.cancelId(id)).fenced, true)
      await aborted
    } else question.answer()
    assert.equal((await session.subagents.wait(id, 10000)).state, cancel ? 'cancelled' : 'completed')
  }
  await agent.finalExit()
})

test('live provider dispatches native OS and explicit product tools with native prompt identity', { timeout: 180000, skip: !process.env.OG_API_KEY }, async t => {
  const { cwd, config, env } = await setup(t)
  const calls = []
  const agent = await Agent.spawn({ executable, config, env, onCallback: async request => {
    assert.equal(request.method, 'tool/product_echo')
    calls.push(request)
    return { content: [{ type: 'text', text: 'PRODUCT_VERIFIED_29' }] }
  } })
  t.after(() => agent.finalExit().catch(() => {}))
  const events = []
  agent.subscribe(event => events.push(event))
  const session = await agent.createSession({ workspace: { id: 'native-tools', cwd }, model: 'runtime-test', metadata: {}, mcpServers: [], tools: [
    { name: 'product_echo', description: 'Return a product-service test receipt. Call with code 29.', inputSchema: { type: 'object', properties: { code: { type: 'integer' } }, required: ['code'], additionalProperties: false } },
  ] })
  const receipt = await session.prompt({ turnId: 'turn-native-37', blocks: [{ type: 'text', text: 'Use a native file or shell tool to write exactly NATIVE_FILE_37 to file probe.txt in the current workspace. Then invoke product_echo with code 29. Do both tool calls. Reply with the product receipt after success.' }], metadata: {} })
  assert.equal(receipt.stopReason, 'end_turn')
  assert.equal(receipt.promptId, 'turn-native-37')
  assert.equal((await readFile(join(cwd, 'probe.txt'), 'utf8')).trim(), 'NATIVE_FILE_37')
  assert.equal(calls.length, 1)
  assert.equal(calls[0].context.sessionId, session.id)
  assert.equal(calls[0].context.ownerSessionId, session.id)
  assert.equal(calls[0].context.promptId, 'turn-native-37')
  assert.equal(calls[0].params.code, 29)
  assert.ok(events.some(event => event.type === 'history_record' && event.record.promptId === 'turn-native-37'))
  assert.equal(JSON.stringify(events).includes(process.env.OG_API_KEY), false)
  const history = await session.history()
  assert.ok(history.records.some(record => record.promptId === 'turn-native-37'))
  await agent.finalExit()
})
