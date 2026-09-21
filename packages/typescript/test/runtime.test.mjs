import assert from 'node:assert/strict'
import { test } from 'node:test'
import { mkdtemp, mkdir, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { resolve, join } from 'node:path'
import { createServer } from 'node:http'
import { Agent } from '../dist/index.js'
import { StdioTransport } from '../dist/stdio.js'

const executable = process.env.SOPHON_RUNTIME ?? resolve('../../target/debug/sophon-runtime')

const candidate = (revision, model) => ({ revision, model, instructions: `BASELINE_${revision}`, skillDirectories: [], externalMcpServers: [], reasoningEffort: null, subagentBriefs: [] })

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

test('protocol v2 mock handshake exposes only the typed Agent facade', { timeout: 10000 }, async t => {
  const { config, env } = await setup(t)
  const agent = await Agent.spawn({ executable: process.execPath, config, env, args: ['--input-type=module', '-e', `
    import { createInterface } from 'node:readline'
    const send = frame => process.stdout.write(JSON.stringify(frame) + '\\n')
    send({ type: 'ready', protocolVersion: 2 })
    const lines = createInterface({ input: process.stdin })
    for await (const line of lines) {
      const frame = JSON.parse(line)
      if (!['initialize', 'final_exit'].includes(frame.request.method)) process.exit(2)
      send({ type: 'response', id: frame.id, result: {} })
      if (frame.request.method === 'final_exit') { lines.close(); process.stdin.destroy(); break }
    }
  `] })
  assert.equal(agent.request, undefined)
  assert.equal(agent.extension, undefined)
  await agent.finalExit()
})

test('old protocol executable is rejected before initialization', { timeout: 10000 }, async t => {
  const { config, env } = await setup(t)
  await assert.rejects(Agent.spawn({ executable: process.execPath, config, env, args: ['-e', `
    process.stdout.write(JSON.stringify({ type: 'ready', protocolVersion: 1 }) + '\\n')
    process.stdin.resume()
    process.stdin.on('data', () => process.exit(2))
  `] }), error => error.code === 'protocol_version' && /expected 2/.test(error.message))
})

test('supplied old Runtime executable is rejected', { timeout: 10000, skip: !process.env.SOPHON_OLD_RUNTIME }, async t => {
  const { config, env } = await setup(t)
  await assert.rejects(Agent.spawn({ executable: process.env.SOPHON_OLD_RUNTIME, config, env }), error => error.code === 'protocol_version' && /expected 2/.test(error.message))
})

test('configured FFmpeg is absolute and present before Runtime initialization', { timeout: 10000 }, async t => {
  const { root, config, env } = await setup(t)
  for (const ffmpegExecutable of ['ffmpeg', '', join(root, 'missing ffmpeg'), root]) {
    await assert.rejects(Agent.spawn({ executable, config: { ...config, ffmpegExecutable }, env }), /ffmpegExecutable/)
  }
  // Initialization checks custody/path shape, not codec capabilities. Tool
  // preflight and execution must use the selected binary without a fallback.
  const agent = await Agent.spawn({ executable, config: { ...config, ffmpegExecutable: process.execPath }, env })
  await agent.finalExit()
})

test('real stdio Runtime creates, snapshots, schedules, reloads and checks process exit', { timeout: 60000 }, async t => {
  const { cwd, config, env } = await setup(t)
  const agent = await Agent.spawn({ executable, config, env })
  t.after(() => agent.finalExit().catch(() => {}))
  const events = []
  agent.subscribe(event => events.push(event))
  const options = { workspace: { id: 'asymmetric-workspace', cwd }, model: 'runtime-test', mcpServers: [], tools: [] }
  const session = await agent.createSession(options)
  assert.match(session.id, /.+/)
  const snapshot = await session.history()
  assert.equal(snapshot.sessionId, session.id)
  assert.ok(events.some(event => event.type === 'history_boundary' && event.boundaryId === snapshot.boundaryId))
  const queue = await session.queue()
  assert.equal(queue.running, null)
  assert.deepEqual(queue.pending, [])
  await assert.rejects(session.prompt({ turnId: 'unsupported-candidate', blocks: [{ type: 'text', text: 'Must never execute.' }], configCandidate: candidate('unsupported', 'unpublished-model') }), /configCandidate|config_candidate|candidate/i)
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

test('candidate-required create, load and resume keep scheduler inspection read-only', { timeout: 30000 }, async t => {
  const { cwd, config, env } = await setup(t)
  const agent = await Agent.spawn({ executable, config, env })
  t.after(() => agent.finalExit().catch(() => {}))
  const options = { workspace: { id: 'candidate-required', cwd }, requireConfigCandidate: true, model: 'runtime-test', mcpServers: [], tools: [] }
  let session = await agent.createSession(options)
  const id = session.id
  for (const attach of ['create', 'load', 'resume']) {
    if (attach === 'load') session = await agent.loadSession(id, options)
    if (attach === 'resume') session = await agent.resumeSession(id, options)
    const before = await session.scheduler.list()
    assert.deepEqual(before.tasks, [])
    const effective = await session.effectiveConfig()
    assert.equal(effective.sessionId, session.id)
    assert.equal(effective.mountedRevision, null, 'cold attachment is not a successful candidate mount')
    assert.equal(typeof effective.version.generation, 'string')
    await assert.rejects(session.prompt({ turnId: `unmounted-${attach}`, blocks: [{ type: 'text', text: 'Must not infer.' }] }), /requires a configuration candidate before execution/)
    await assert.rejects(session.scheduler.create(`blocked-${attach}`, before.version, {
      cadence: { kind: 'once', at: new Date(Date.now() - 60000).toISOString() },
      prompt: 'Must never run before native publication.', durable: true,
    }), /requires a committed configuration candidate/)
    await assert.rejects(session.scheduler.delete(`blocked-delete-${attach}`, before.version, 'unmounted-task'), /requires a committed configuration candidate/)
    assert.deepEqual(await session.scheduler.list(), before, 'refused writes must not alter scheduler state')
    assert.equal((await session.effectiveConfig()).mountedRevision, null)
    await session.dispose()
  }
  await agent.finalExit()
})

test('official candidate ingress publishes native receipts and keeps busy prompts on the mounted route', { timeout: 60000 }, async t => {
  const { cwd, config, env } = await setup(t)
  const requests = []
  let entered
  const firstEntered = new Promise(resolve => { entered = resolve })
  let release
  const releaseFirst = new Promise(resolve => { release = resolve })
  t.after(() => release())
  const server = createServer(async (request, response) => {
    let body = ''
    for await (const chunk of request) body += chunk
    const payload = JSON.parse(body)
    requests.push(payload)
    const latest = JSON.stringify(payload.messages.filter(message => message.role === 'user').at(-1))
    if (latest.includes('SDK_MOUNT_FIRST_29')) { entered(); await releaseFirst }
    const chunk = (delta, finish_reason) => ({ id: 'mount-fixture', object: 'chat.completion.chunk', created: 1, model: payload.model, choices: [{ index: 0, delta, finish_reason }] })
    response.writeHead(200, { 'content-type': 'text/event-stream' })
    response.end(`data: ${JSON.stringify(chunk({ role: 'assistant', content: 'MOUNT_COMPLETE' }, null))}\n\ndata: ${JSON.stringify(chunk({}, 'stop'))}\n\ndata: [DONE]\n\n`)
  })
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve))
  t.after(() => new Promise(resolve => { server.closeAllConnections(); server.close(resolve) }))
  config.models[0].provider = { protocol: 'openai_chat', baseUrl: `http://127.0.0.1:${server.address().port}`, apiKey: 'local-fixture', model: 'mount-a-wire', headers: {}, queryParams: {} }
  config.models[0].supportedReasoning = ['medium']
  config.models.push({ ...config.models[0], id: 'mount-c', supportedReasoning: ['high'], provider: { ...config.models[0].provider, model: 'mount-c-wire' } })
  const agent = await Agent.spawn({ executable, config, env })
  t.after(() => agent.finalExit().catch(() => {}))
  const options = { workspace: { id: 'official-mount', cwd }, requireConfigCandidate: true, model: 'runtime-test', mcpServers: [], tools: [] }
  const session = await agent.createSession(options)
  const events = []
  session.subscribe(event => events.push(event))
  const prompt = (turnId, text, configCandidate, sendNow = false) => session.prompt({ turnId, blocks: [{ type: 'text', text }], configCandidate, sendNow })
  assert.equal((await session.effectiveConfig()).mountedRevision, null)
  await assert.rejects(prompt('cold-invalid', 'Must not infer.', candidate('invalid', 'missing-route')))
  await assert.rejects(prompt('cold-unsupported-effort', 'Must not infer.', { ...candidate('invalid-effort', 'runtime-test'), reasoningEffort: 'high' }), /reasoning effort is unsupported/)
  assert.equal(requests.length, 0)
  assert.equal((await session.effectiveConfig()).mountedRevision, null)
  const first = prompt('mount-first', 'SDK_MOUNT_FIRST_29', { ...candidate('A', 'runtime-test'), reasoningEffort: 'medium' })
  await Promise.race([firstEntered, first.then(() => { throw new Error('First prompt completed before its provider barrier') })])
  assert.equal((await session.effectiveConfig()).mountedRevision, 'A')
  let queued
  const busyQueued = new Promise(resolve => { queued = resolve })
  const unsubscribe = session.subscribe(event => {
    if (event.type === 'queue' && event.snapshot.pending.some(entry => entry.id === 'mount-busy')) queued()
  })
  const busy = prompt('mount-busy', 'SDK_MOUNT_BUSY_41', candidate('B', 'missing-route'))
  await Promise.race([busyQueued, busy.then(() => { throw new Error('Busy prompt completed before queue admission') })])
  unsubscribe()
  assert.equal((await session.effectiveConfig()).mountedRevision, 'A')
  release()
  assert.equal((await first).stopReason, 'end_turn')
  assert.equal((await busy).stopReason, 'end_turn')
  const count = requests.length
  await assert.rejects(prompt('idle-invalid', 'Must not infer either.', candidate('invalid-idle', 'missing-route')))
  assert.equal(requests.length, count)
  assert.equal((await session.effectiveConfig()).mountedRevision, 'A')
  assert.equal((await prompt('mount-idle', 'SDK_MOUNT_IDLE_73', { ...candidate('C', 'mount-c'), reasoningEffort: 'high' }, true)).stopReason, 'end_turn')
  assert.equal((await session.effectiveConfig()).mountedRevision, 'C')
  assert.deepEqual(requests.map(request => request.model), ['mount-a-wire', 'mount-a-wire', 'mount-c-wire'])
  assert.deepEqual(requests.map(request => request.reasoning_effort), ['medium', 'medium', 'high'])
  assert.match(JSON.stringify(requests[0].messages), /BASELINE_A/)
  assert.match(JSON.stringify(requests[1].messages), /BASELINE_A/)
  assert.match(JSON.stringify(requests[2].messages), /BASELINE_C/)
  for (const promptId of ['mount-first', 'mount-busy', 'mount-idle']) {
    assert.ok(events.some(event => event.type === 'session' && event.promptId === promptId), `native session events must identify ${promptId}`)
  }
  const before = await session.scheduler.list()
  const created = await session.scheduler.create('mounted-schedule', before.version, { cadence: { kind: 'once', at: new Date(Date.now() + 3600000).toISOString() }, prompt: 'Future mounted task.', durable: true })
  assert.equal(created.type, 'committed')
  assert.equal((await session.scheduler.delete('mounted-delete', created.version, created.value.id)).type, 'committed')
  await session.dispose()
  const loaded = await agent.loadSession(session.id, options)
  assert.equal((await loaded.effectiveConfig()).mountedRevision, null, 'a persisted Session must not reuse a prior process-local mount receipt')
  const closed = await loaded.scheduler.list()
  await assert.rejects(loaded.scheduler.delete('cold-delete', closed.version, created.value.id), /requires a committed configuration candidate/)
  await loaded.dispose()
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
  const agent = await Agent.spawn({ executable, config, env, onToolCall: () => { throw new Error('No tools allowed') } })
  t.after(() => agent.finalExit().catch(() => {}))
  const session = await agent.createSession({ workspace: { id: 'refiner-before-parent', cwd }, model: 'runtime-test', mcpServers: [], tools: [{ name: 'product_echo', description: 'Must not inherit into refiner', inputSchema: { type: 'object', properties: {} } }] })
  const result = await session.subagents.start({ id: await session.subagents.newId(), prompt: 'EVIDENCE_29. Refine this trajectory.', description: 'Fixed refiner test', subagentType: 'refiner', cwd: null, model: 'distillation' })
  assert.equal(result.state, 'completed', result.error ?? result.output)
  assert.match(result.output, /REFINEMENT_73/)
  const refinement = requests.find(request => JSON.stringify(request.messages).includes('BASELINE_REFINEMENT_73'))
  assert.ok(refinement, 'actual inference must receive the fixed baseline')
  assert.equal(refinement.model, 'distill-wire', JSON.stringify(requests.map(request => ({ model: request.model, tools: request.tools?.length ?? 0, baseline: JSON.stringify(request.messages).includes('BASELINE_REFINEMENT_73') }))))
  assert.match(JSON.stringify(refinement.messages), /EVIDENCE_29/)
  assert.equal(refinement.tools?.length ?? 0, 0)
  const count = requests.length
  for (const model of ['missing-route', 'distill-wire']) {
    const refused = await session.subagents.start({ id: await session.subagents.newId(), prompt: 'Must not infer.', description: 'unpublished route', subagentType: 'refiner', cwd: null, model })
    assert.equal(refused.state, 'failed')
    assert.match(refused.error ?? refused.output, /registered catalog/)
    assert.equal(requests.length, count, 'unpublished IDs and wire aliases must not fall back to primary')
  }
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
    const isChild = JSON.stringify(payload.messages).includes('Ask the user then finish.')
    const ask = isChild && payload.tools?.some(tool => tool.function?.name === 'ask_user') && !answered
    const delegate = !isChild && JSON.stringify(payload.messages).includes('ROOT_NATIVE_TASK_73') && !payload.messages.some(message => message.role === 'tool' && message.tool_call_id === 'root-task-73')
    const taskTool = payload.tools?.find(tool => ['task', 'Task', 'spawn_subagent'].includes(tool.function?.name))
    if (delegate) assert.ok(taskTool, `root must use advertised native TaskTool: ${payload.tools?.map(tool => tool.function?.name).join(',')}`)
    const delta = delegate
      ? { role: 'assistant', tool_calls: [{ index: 0, id: 'root-task-73', type: 'function', function: { name: taskTool.function.name, arguments: JSON.stringify({ prompt: 'Ask the human.', description: 'Root attribution check', subagent_type: 'task', run_in_background: false }) } }] }
      : ask ? { role: 'assistant', tool_calls: [{ index: 0, id: 'question29', type: 'function', function: { name: 'ask_user', arguments: '{"question":"Choose 29?"}' } }] } : { role: 'assistant', content: 'CHILD_COMPLETE_73' }
    const chunk = (delta, finish_reason) => ({ id: 'child-fixture', object: 'chat.completion.chunk', created: 1, model: 'child-wire', choices: [{ index: 0, delta, finish_reason }] })
    response.writeHead(200, { 'content-type': 'text/event-stream' })
    response.end(`data: ${JSON.stringify(chunk(delta, null))}\n\ndata: ${JSON.stringify(chunk({}, ask || delegate ? 'tool_calls' : 'stop'))}\n\ndata: [DONE]\n\n`)
  })
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve))
  t.after(() => new Promise(resolve => { server.closeAllConnections(); server.close(resolve) }))
  config.models[0].provider = { protocol: 'openai_chat', baseUrl: `http://127.0.0.1:${server.address().port}`, apiKey: 'local-fixture', model: 'child-wire', headers: {}, queryParams: {} }
  for (const name of ['task', 'general-purpose']) {
    config.subagents.push({ name, description: 'Fixed task worker', instructions: 'Ask the user then finish.', model: 'runtime-test', tools: null })
  }
  let receive
  let nextQuestion = new Promise(resolve => { receive = resolve })
  const agent = await Agent.spawn({ executable, config, env, onToolCall: request => new Promise(resolve => {
    assert.equal(request.name, 'ask_user')
    assert.deepEqual(request.args, { question: 'Choose 29?' })
    receive({ request, answer: () => resolve({ answer: 'ANSWER_29' }) })
    request.signal.addEventListener('abort', () => resolve({ cancelled: true }), { once: true })
  }) })
  t.after(() => agent.finalExit().catch(() => {}))
  const toolCompletions = []
  agent.subscribe(event => {
    if (event.type === 'session' && event.update.type === 'tool_call_update' && event.update.value.status === 'completed') toolCompletions.push(event.update.value)
  })
  const session = await agent.createSession({ workspace: { id: 'callback-parent', cwd }, model: 'runtime-test', mcpServers: [], tools: [{ name: 'ask_user', description: 'Ask a human question', inputSchema: { type: 'object', properties: { question: { type: 'string' } }, required: ['question'] } }] })
  const ordinary = session.subagents.start({ id: await session.subagents.newId(), prompt: 'Ask the human.', description: 'ordinary child', subagentType: 'task', cwd: childCwd, model: 'runtime-test' })
  const first = await nextQuestion
  assert.equal(first.request.context.ownerSessionId, session.id)
  assert.notEqual(first.request.context.sessionId, session.id)
  assert.equal(first.request.context.cwd, childCwd)
  assert.equal(first.request.context.scheduledInvocation, null)
  assert.equal(first.request.context.originatingPrompt, null, 'direct child before any parent prompt has no invented root')
  // Creating and inspecting a different idle Session must not acquire the
  // portability/export fence for the active parent or its child callback.
  const peer = await agent.createSession({ workspace: { id: 'busy-peer-history', cwd: childCwd }, model: 'runtime-test', mcpServers: [], tools: [] })
  const peerSchedule = await peer.scheduler.list()
  const peerTask = await peer.scheduler.create('peer-future', peerSchedule.version, { cadence: { kind: 'once', at: new Date(Date.now() + 3600000).toISOString() }, prompt: 'Future peer task.', durable: true })
  assert.equal(peerTask.type, 'committed')
  let peerHistory
  let peerHistoryFailure
  try { peerHistory = await peer.history() } catch (error) { peerHistoryFailure = error }
  assert.equal(first.request.signal.aborted, false, 'peer inspection must not cancel active work')
  await peer.scheduler.delete('peer-remove', peerTask.version, peerTask.value.id)
  await peer.dispose()
  first.answer()
  assert.equal((await ordinary).state, 'completed')
  assert.ok(toolCompletions.some(update => update.rawOutput?.type === 'Dynamic' && update.rawOutput.value?.answer === 'ANSWER_29'), 'successful structured callback must emit a native completed tool event with its exact output')
  nextQuestion = new Promise(resolve => { receive = resolve })
  const rootTurn = session.prompt({ turnId: 'root-prompt-73', blocks: [{ type: 'text', text: 'ROOT_NATIVE_TASK_73' }] })
  const rooted = await nextQuestion
  assert.equal(rooted.request.context.ownerSessionId, session.id)
  assert.notEqual(rooted.request.context.sessionId, session.id)
  assert.notEqual(rooted.request.context.promptId, 'root-prompt-73')
  assert.deepEqual(rooted.request.context.originatingPrompt, { sessionId: session.id, promptId: 'root-prompt-73' })
  assert.equal(rooted.request.context.scheduledInvocation, null)
  rooted.answer()
  assert.equal((await rootTurn).stopReason, 'end_turn')
  for (const cancel of [false, true]) {
    nextQuestion = new Promise(resolve => { receive = resolve })
    let fired
    const child = new Promise(resolve => { fired = resolve })
    const unsubscribe = session.subscribe(event => {
      if (event.type === 'scheduler' && event.occurrence.type === 'fired') fired(event)
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
    assert.equal(context.originatingPrompt, null, 'scheduled invocation must not become a human root')
    const event = await child
    assert.equal(event.taskId, context.scheduledInvocation.taskId)
    assert.equal(event.occurrence.occurrence, context.scheduledInvocation.occurrence)
    const id = event.occurrence.subagentId
    unsubscribe()
    if (cancel) {
      const aborted = new Promise(resolve => question.request.signal.addEventListener('abort', resolve, { once: true }))
      assert.equal((await session.subagents.cancelId(id)).fenced, true)
      await aborted
    } else question.answer()
    assert.equal((await session.subagents.wait(id, 10000)).state, cancel ? 'cancelled' : 'completed')
  }
  const fires = []
  let twoFires
  let recurringId
  let skipped
  const skipUpdate = new Promise(resolve => { skipped = resolve })
  const firedTwice = new Promise(resolve => { twoFires = resolve })
  const unsubscribe = session.subscribe(event => {
    if (event.type === 'scheduler' && event.occurrence.type === 'fired') {
      fires.push(event)
      if (fires.length === 2) twoFires()
    }
    if (event.type === 'scheduler' && event.taskId === recurringId && event.occurrence.type === 'upserted') skipped()
  })
  nextQuestion = new Promise(resolve => { receive = resolve })
  const before = await session.scheduler.list()
  const recurring = await session.scheduler.create('overlap', before.version, { cadence: { kind: 'interval', everySecs: 2, anchor: new Date(Date.now() - 2000).toISOString() }, prompt: 'Ask the human before finishing.', durable: true })
  assert.equal(recurring.type, 'committed')
  const earlier = await nextQuestion
  recurringId = recurring.value.id
  await skipUpdate
  const advanced = await session.scheduler.list()
  const skippedTask = advanced.tasks.find(task => task.id === recurringId)
  assert.equal(skippedTask.lastDispatch.status, 'skipped')
  assert.notEqual(skippedTask.lastDispatch.occurrence, earlier.request.context.scheduledInvocation.occurrence)
  assert.equal(skippedTask.lastDispatch.subagentId, null)
  assert.equal(fires.length, 1, 'skipped occurrence must not emit Fired or launch a second child')
  assert.equal(earlier.request.signal.aborted, false)
  nextQuestion = new Promise(resolve => { receive = resolve })
  const independent = await session.scheduler.create('independent-task', advanced.version, { cadence: { kind: 'once', at: new Date(Date.now() - 1000).toISOString() }, prompt: 'Ask the human before finishing.', durable: true })
  assert.equal(independent.type, 'committed')
  const later = await nextQuestion
  assert.notEqual(later.request.context.scheduledInvocation.taskId, recurringId)
  await firedTwice
  unsubscribe()
  const current = await session.scheduler.list()
  assert.equal((await session.scheduler.delete('stop-future', current.version, recurring.value.id)).type, 'committed')
  const childFor = question => {
    const source = question.request.context.scheduledInvocation
    const event = fires.find(event => event.sessionId === source.sessionId && event.taskId === source.taskId && event.occurrence.occurrence === source.occurrence)
    assert.ok(event, 'exact native fired association must exist')
    return event.occurrence.subagentId
  }
  const earlierId = childFor(earlier)
  const laterId = childFor(later)
  assert.notEqual(earlierId, laterId)
  const aborted = new Promise(resolve => earlier.request.signal.addEventListener('abort', resolve, { once: true }))
  assert.equal((await session.subagents.cancelId(earlierId)).fenced, true)
  await aborted
  assert.equal((await session.subagents.wait(earlierId, 10000)).state, 'cancelled')
  assert.equal(later.request.signal.aborted, false, 'stopping earlier occurrence must not cancel later question')
  later.answer()
  assert.equal((await session.subagents.wait(laterId, 10000)).state, 'completed')
  const unrelated = await agent.createSession({ workspace: { id: 'unrelated-close-owner', cwd: childCwd }, model: 'runtime-test', mcpServers: [], tools: [] })
  nextQuestion = new Promise(resolve => { receive = resolve })
  const beforeClose = await session.scheduler.list()
  const closeTask = await session.scheduler.create('pending-at-dispose', beforeClose.version, { cadence: { kind: 'once', at: new Date(Date.now() - 1000).toISOString() }, prompt: 'Ask the human before finishing.', durable: true })
  assert.equal(closeTask.type, 'committed')
  const pendingAtClose = await nextQuestion
  assert.equal(pendingAtClose.request.context.scheduledInvocation.taskId, closeTask.value.id)
  assert.equal(pendingAtClose.request.signal.aborted, false)
  await session.dispose()
  assert.equal(pendingAtClose.request.signal.aborted, true, 'checked dispose must abort owned callback before acknowledging close')
  assert.equal((await unrelated.prompt({ turnId: 'unrelated-after-close', blocks: [{ type: 'text', text: 'Complete independently.' }] })).stopReason, 'end_turn')
  await unrelated.dispose()
  await agent.finalExit()
  assert.equal(peerHistoryFailure, undefined, 'peer history must not use the agent-global portable-export fence')
  assert.equal(peerHistory.sessionId, peer.id)
  assert.ok(peerHistory.boundaryId)
})

test('native exit errors flush a structured receipt before process closure', { timeout: 30000 }, async t => {
  const { cwd, config, env } = await setup(t)
  const transport = new StdioTransport({ executable, env })
  const agent = await Agent.connect(transport, config)
  await agent.createSession({ workspace: { id: 'exit-error', cwd }, model: 'runtime-test', mcpServers: [], tools: [] })
  await assert.rejects(agent.finalExit(0), error => error.code === 'operation_failed' && /timed out/i.test(error.message))
  assert.deepEqual(await transport.closed, { code: 1, signal: null })
})

test('idle newly created Session exits with a checked native receipt', { timeout: 40000 }, async t => {
  const { cwd, config, env } = await setup(t)
  let diagnostics = ''
  const transport = new StdioTransport({ executable, env, onStderr: chunk => { diagnostics += chunk } })
  const agent = await Agent.connect(transport, config)
  await agent.createSession({ workspace: { id: 'unbound-product-identity', cwd }, model: 'runtime-test', mcpServers: [], tools: [] })
  // Product identity persistence can fail after native creation. Admit no
  // prompt, do not dispose first, and do not retry an uncertain final exit.
  try { await agent.finalExit() }
  catch (cause) {
    const closed = await transport.closed
    const reason = diagnostics.split('\n').filter(line => line.startsWith('Sophon Runtime stopped:') || /panicked at|fatal runtime error|stack overflow|double free|corrupt|malloc|Thread Local|AccessError|already borrowed/.test(line)).join('\n')
    throw new Error(`Idle exit failed: ${JSON.stringify(closed)} ${reason}`, { cause })
  }
})

test('live provider dispatches native OS and explicit product tools with native prompt identity', { timeout: 180000, skip: process.env.SOPHON_LIVE_GATEWAY !== '1' }, async t => {
  const { cwd, config, env } = await setup(t)
  const origin = process.env.OG_AI_GATEWAY.replace(/\/$/, '')
  const headers = { authorization: `Bearer ${process.env.OG_API_KEY}` }
  const effectiveResponse = await fetch(`${origin}/api/v1/product-config/effective`, { headers, signal: AbortSignal.timeout(15000) })
  assert.equal(effectiveResponse.status, 200)
  const route = (await effectiveResponse.json()).routes.deep
  assert.equal(route.status, 'available')
  assert.equal(route.endpoint, 'openai-response')
  const catalogResponse = await fetch(`${origin}/v1/models`, { headers, signal: AbortSignal.timeout(15000) })
  assert.equal(catalogResponse.status, 200)
  const model = (await catalogResponse.json()).data.find(model => model.id === route.model)
  assert.ok(model.supported_reasoning.includes(route.reasoning_effort))
  Object.assign(config.models[0], { contextWindow: model.limit.context, supportedReasoning: model.supported_reasoning })
  Object.assign(config.models[0].provider, { protocol: 'openai_responses', baseUrl: `${origin}/v1`, model: route.model })
  const calls = []
  const agent = await Agent.spawn({ executable, config, env, onToolCall: async request => {
    assert.equal(request.name, 'product_echo')
    calls.push(request)
    return { content: [{ type: 'text', text: 'PRODUCT_VERIFIED_29' }] }
  } })
  t.after(() => agent.finalExit().catch(() => {}))
  const events = []
  agent.subscribe(event => events.push(event))
  const session = await agent.createSession({ workspace: { id: 'native-tools', cwd }, model: 'runtime-test', mcpServers: [], tools: [
    { name: 'product_echo', description: 'Return a product-service test receipt. Call with code 29.', inputSchema: { type: 'object', properties: { code: { type: 'integer' } }, required: ['code'], additionalProperties: false } },
  ] })
  const receipt = await session.prompt({ turnId: 'turn-native-37', blocks: [{ type: 'text', text: 'Use a native file or shell tool to write exactly NATIVE_FILE_37 to file probe.txt in the current workspace. Then invoke product_echo with code 29. Do both tool calls. Reply with the product receipt after success.' }], configCandidate: { ...candidate('live-tools', 'runtime-test'), reasoningEffort: route.reasoning_effort } })
  assert.equal(receipt.stopReason, 'end_turn')
  assert.equal(receipt.promptId, 'turn-native-37')
  assert.equal((await readFile(join(cwd, 'probe.txt'), 'utf8')).trim(), 'NATIVE_FILE_37')
  assert.equal(calls.length, 1)
  assert.equal(calls[0].context.sessionId, session.id)
  assert.equal(calls[0].context.ownerSessionId, session.id)
  assert.equal(calls[0].context.promptId, 'turn-native-37')
  assert.deepEqual(calls[0].context.originatingPrompt, { sessionId: session.id, promptId: 'turn-native-37' })
  assert.equal(calls[0].args.code, 29)
  assert.ok(events.some(event => event.type === 'history_record' && event.record.promptId === 'turn-native-37'))
  assert.equal(JSON.stringify(events).includes(process.env.OG_API_KEY), false)
  const history = await session.history()
  assert.ok(history.records.some(record => record.promptId === 'turn-native-37'))
  await agent.finalExit()
})
