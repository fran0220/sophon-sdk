import assert from 'node:assert/strict'
import { test } from 'node:test'
import { createServer } from 'node:http'
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdtemp, mkdir, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { Agent } from '../dist/index.js'

test('native sound and music publish decoded artifacts and completed receipts without host callbacks', { timeout: 60000 }, async t => {
  const root = await mkdtemp(join(tmpdir(), 'sophon-audio-tools-'))
  t.after(() => rm(root, { recursive: true, force: true }))
  const cwd = join(root, 'workspace'); await mkdir(cwd)
  const audio = execFileSync('ffmpeg', ['-nostdin', '-v', 'error', '-f', 'lavfi', '-i', 'sine=frequency=739:duration=0.2', '-f', 'mp3', 'pipe:1'])
  const revision = createHash('sha256').update(audio).digest('hex')
  const calls = []
  let enabled = true, callbacks = 0
  const server = createServer(async (request, response) => {
    let raw = ''; for await (const chunk of request) raw += chunk
    const body = JSON.parse(raw)
    if (request.url !== '/v1/chat/completions') {
      assert.ok(['/v1/sound-generation', '/v1/music'].includes(request.url))
      assert.equal(request.headers.authorization, 'Bearer fixture-private')
      calls.push({ path: request.url, body })
      response.writeHead(200, { 'content-type': 'audio/mpeg' }); response.end(audio); return
    }
    const names = body.tools.map(tool => tool.function?.name)
    assert.equal(names.includes('generate_sound_effect'), enabled)
    assert.equal(names.includes('generate_music'), enabled)
    const count = body.messages.filter(message => message.role === 'tool').length
    const finished = !enabled || count >= 2
    const name = count === 0 ? 'generate_sound_effect' : 'generate_music'
    const args = count === 0 ? { prompt: 'coin effect', output_path: 'coin.mp3', duration_seconds: 1.5, loop: false }
      : { prompt: 'game music', output_path: 'music.mp3', duration_seconds: 3.125, force_instrumental: true }
    const delta = finished ? { role: 'assistant', content: 'Artifacts require review.' }
      : { role: 'assistant', tool_calls: [{ index: 0, id: `audio-${count}`, type: 'function', function: { name, arguments: JSON.stringify(args) } }] }
    const chunk = (delta, finish_reason) => ({ id: 'audio-fixture', object: 'chat.completion.chunk', created: 1, model: 'fixture', choices: [{ index: 0, delta, finish_reason }] })
    response.writeHead(200, { 'content-type': 'text/event-stream' })
    response.end(`data: ${JSON.stringify(chunk(delta, null))}\n\ndata: ${JSON.stringify(chunk({}, finished ? 'stop' : 'tool_calls'))}\n\ndata: [DONE]\n\n`)
  })
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve))
  t.after(() => new Promise(resolve => { server.closeAllConnections(); server.close(resolve) }))
  const baseUrl = `http://127.0.0.1:${server.address().port}/v1`
  const media = { image: null, speech: null, video: null,
    sfx: { endpoint: 'audio-sfx', model: 'declared-sfx', baseUrl, bearerToken: 'fixture-private', headers: {} },
    music: { endpoint: 'audio-music', model: 'declared-music', baseUrl, bearerToken: 'fixture-private', headers: {} } }
  const config = { models: [{ id: 'main', provider: { protocol: 'openai_chat', baseUrl, apiKey: 'fixture', model: 'fixture', headers: {}, queryParams: {} } }], defaultModel: 'main', subagents: [], media }
  for (enabled of [true, false]) {
    const agent = await Agent.spawn({ executable: process.env.SOPHON_RUNTIME ?? resolve('../../target/debug/sophon-runtime'),
      config: { ...config, media: enabled ? media : { image: null, speech: null, video: null } },
      env: { GROK_HOME: join(root, `home-${enabled}`), GROK_AUTH: '', GROK_TELEMETRY_ENABLED: 'false', GROK_TRACE_UPLOAD: 'false', GROK_FEEDBACK_ENABLED: 'false' },
      onToolCall: () => { callbacks++; throw new Error('must be native') } })
    t.after(() => agent.finalExit().catch(() => {}))
    const events = []; agent.subscribe(event => events.push(event))
    const session = await agent.createSession({ workspace: { id: `audio-${enabled}`, cwd }, model: 'main', mcpServers: [], tools: [] })
    assert.equal((await session.prompt({ turnId: `audio-${enabled}`, blocks: [{ type: 'text', text: 'Create sound and music using native tools if available.' }] })).stopReason, 'end_turn')
    const outputs = events.filter(event => event.type === 'session' && event.update.type === 'tool_call_update' && event.update.value.status === 'completed').map(event => event.update.value.rawOutput).filter(output => output?.type === 'Dynamic').map(output => output.value)
    assert.equal(outputs.length, enabled ? 2 : 0)
    for (const result of outputs) {
      assert.equal(result.artifact.revision, revision)
      assert.equal(result.artifact.mimeType, 'audio/mpeg')
      assert.deepEqual(await readFile(join(cwd, result.artifact.path)), audio)
      const receipt = JSON.parse(await readFile(join(cwd, result.receipt), 'utf8'))
      assert.equal(receipt.status, 'completed'); assert.equal(receipt.request_id, result.request_id)
      assert.deepEqual(receipt.artifact, result.artifact)
    }
    assert.doesNotMatch(JSON.stringify(events), /fixture-private/)
    await session.dispose(); await agent.finalExit()
  }
  assert.equal(callbacks, 0); assert.equal(calls.length, 2)
  assert.deepEqual(calls, [
    { path: '/v1/sound-generation', body: { model: 'declared-sfx', model_id: 'declared-sfx', text: 'coin effect', duration_seconds: 1.5, loop: false } },
    { path: '/v1/music', body: { model: 'declared-music', model_id: 'declared-music', prompt: 'game music', music_length_ms: 3125, force_instrumental: true } },
  ])
})
