import assert from 'node:assert/strict'
import { test } from 'node:test'
import { mkdtemp, mkdir, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { resolve, join } from 'node:path'
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
  const before = await session.scheduler.list()
  const created = await session.scheduler.create('schedule-a', before.version, { intervalSecs: 3600, prompt: 'future explicit scheduled task', recurring: false, durable: true, fireImmediately: false })
  assert.equal(created.type, 'committed')
  const conflict = await session.scheduler.create('schedule-stale', before.version, { intervalSecs: 7200, prompt: 'must not commit', recurring: false, durable: true, fireImmediately: false })
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
  assert.equal(calls[0].context.promptId, 'turn-native-37')
  assert.equal(calls[0].params.code, 29)
  assert.ok(events.some(event => event.type === 'history_record' && event.record.promptId === 'turn-native-37'))
  assert.equal(JSON.stringify(events).includes(process.env.OG_API_KEY), false)
  const history = await session.history()
  assert.ok(history.records.some(record => record.promptId === 'turn-native-37'))
  await agent.finalExit()
})
