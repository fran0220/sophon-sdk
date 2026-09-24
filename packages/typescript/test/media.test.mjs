import assert from 'node:assert/strict'
import { test } from 'node:test'
import { createServer } from 'node:http'
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdtemp, mkdir, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { Agent } from '../dist/index.js'

test('native sound, music and model tools publish decoded artifacts without host callbacks', { timeout: 60000 }, async t => {
  const root = await mkdtemp(join(tmpdir(), 'sophon-audio-tools-'))
  t.after(() => rm(root, { recursive: true, force: true }))
  const cwd = join(root, 'workspace'); await mkdir(cwd)
  const audio = execFileSync('ffmpeg', ['-nostdin', '-v', 'error', '-f', 'lavfi', '-i', 'sine=frequency=739:duration=0.2', '-f', 'mp3', 'pipe:1'])
  const revision = createHash('sha256').update(audio).digest('hex')
  const document = { asset: { version: '2.0' }, buffers: [{ byteLength: 36 }], bufferViews: [{ buffer: 0, byteLength: 36 }], accessors: [{ bufferView: 0, componentType: 5126, count: 3, type: 'VEC3', min: [0, 0, 0], max: [2, 3, 0] }], meshes: [{ primitives: [{ attributes: { POSITION: 0 } }] }], nodes: [{ mesh: 0 }], scenes: [{ nodes: [0] }], scene: 0 }
  let json = JSON.stringify(document); json = json.padEnd(Math.ceil(json.length / 4) * 4, ' ')
  const glb = Buffer.alloc(28 + json.length + 36)
  glb.write('glTF'); glb.writeUInt32LE(2, 4); glb.writeUInt32LE(glb.length, 8)
  glb.writeUInt32LE(json.length, 12); glb.write('JSON', 16); glb.write(json, 20)
  glb.writeUInt32LE(36, 20 + json.length); glb.write('BIN\0', 24 + json.length)
  ;[0, 0, 0, 2, 0, 0, 0, 3, 0].forEach((v, i) => glb.writeFloatLE(v, 28 + json.length + i * 4))
  const calls = []
  const modelCalls = []
  let enabled = true, callbacks = 0
  const server = createServer(async (request, response) => {
    let raw = ''; for await (const chunk of request) raw += chunk
    const body = raw ? JSON.parse(raw) : null
    if (request.url.startsWith('/meshy/')) {
      modelCalls.push({ path: request.url, method: request.method, body })
      assert.equal(request.headers.authorization, 'Bearer fixture-private')
      if (request.url === '/meshy/openapi/v2/text-to-3d') { response.writeHead(202); response.end(JSON.stringify({ result: 'fixture-model' })); return }
      if (request.url === '/meshy/tasks/fixture-model') { response.end(JSON.stringify({ task_id: 'fixture-model', platform: 'meshy', action: 'text-to-3d', status: 'SUCCESS' })); return }
      assert.equal(request.url, '/meshy/tasks/fixture-model/content'); response.end(glb); return
    }
    if (request.url !== '/v1/chat/completions') {
      assert.ok(['/v1/sound-generation', '/v1/music'].includes(request.url))
      assert.equal(request.headers.authorization, 'Bearer fixture-private')
      calls.push({ path: request.url, body })
      response.writeHead(200, { 'content-type': 'audio/mpeg' }); response.end(audio); return
    }
    const names = body.tools.map(tool => tool.function?.name)
    assert.equal(names.includes('generate_sound_effect'), enabled)
    assert.equal(names.includes('generate_music'), enabled)
    assert.equal(names.includes('generate_model3d'), enabled)
    const count = body.messages.filter(message => message.role === 'tool').length
    const finished = !enabled || count >= 3
    const name = ['generate_sound_effect', 'generate_music', 'generate_model3d'][count]
    const args = count === 0 ? { prompt: 'coin effect', output_path: 'coin.mp3', duration_seconds: 1.5, loop: false }
      : count === 1 ? { prompt: 'game music', output_path: 'music.mp3', duration_seconds: 3.125, force_instrumental: true }
        : { prompt: 'game model', output_path: 'model.glb' }
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
    music: { endpoint: 'audio-music', model: 'declared-music', baseUrl, bearerToken: 'fixture-private', headers: {} },
    model3d: { endpoint: 'model-3d-text', model: 'meshy-text-to-3d', baseUrl: baseUrl.replace(/\/v1$/, ''), bearerToken: 'fixture-private', headers: {} } }
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
    assert.equal(outputs.length, enabled ? 3 : 0)
    for (const result of outputs) {
      const model = result.kind === 'model3d'
      assert.equal(result.artifact.revision, model ? createHash('sha256').update(glb).digest('hex') : revision)
      assert.equal(result.artifact.mimeType, model ? 'model/gltf-binary' : 'audio/mpeg')
      assert.deepEqual(await readFile(join(cwd, result.artifact.path)), model ? glb : audio)
      const receipt = JSON.parse(await readFile(join(cwd, result.receipt), 'utf8'))
      assert.equal(receipt.status, 'completed'); assert.equal(receipt.request_id, result.request_id)
      assert.deepEqual(receipt.artifact, result.artifact)
    }
    assert.doesNotMatch(JSON.stringify(events), /fixture-private/)
    await session.dispose(); await agent.finalExit()
  }
  assert.equal(callbacks, 0); assert.equal(calls.length, 2)
  assert.deepEqual(modelCalls, [
    { path: '/meshy/openapi/v2/text-to-3d', method: 'POST', body: { mode: 'preview', prompt: 'game model', ai_model: 'latest', should_remesh: false, target_formats: ['glb'] } },
    { path: '/meshy/tasks/fixture-model', method: 'GET', body: null },
    { path: '/meshy/tasks/fixture-model/content', method: 'GET', body: null },
  ])
  assert.deepEqual(calls, [
    { path: '/v1/sound-generation', body: { model: 'declared-sfx', model_id: 'declared-sfx', text: 'coin effect', duration_seconds: 1.5, loop: false } },
    { path: '/v1/music', body: { model: 'declared-music', model_id: 'declared-music', prompt: 'game music', music_length_ms: 3125, force_instrumental: true } },
  ])
})
