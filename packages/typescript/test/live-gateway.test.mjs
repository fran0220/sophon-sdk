import assert from 'node:assert/strict'
import { test } from 'node:test'
import { mkdtemp, mkdir, readFile, writeFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { createServer } from 'node:http'
import { Readable } from 'node:stream'
import { randomUUID } from 'node:crypto'
import { Agent } from '../dist/index.js'

// Explicit opt-in: account credentials alone must not spend gateway quota.
test('configured real gateway executes native MCP, OS tools, auxiliary child and compaction', {
  skip: process.env.SOPHON_LIVE_GATEWAY !== '1', timeout: 300000,
}, async t => {
  assert.ok(process.env.OG_API_KEY, 'OG_API_KEY is required')
  assert.ok(process.env.OG_AI_GATEWAY, 'OG_AI_GATEWAY is required')
  const origin = process.env.OG_AI_GATEWAY.replace(/\/$/, '')
  const auth = { authorization: `Bearer ${process.env.OG_API_KEY}` }
  const effectiveResponse = await fetch(`${origin}/api/v1/product-config/effective`, { headers: auth, signal: AbortSignal.timeout(15000) })
  assert.equal(effectiveResponse.status, 200)
  const effective = await effectiveResponse.json()
  const catalogResponse = await fetch(`${origin}/v1/models`, { headers: auth, signal: AbortSignal.timeout(15000) })
  assert.equal(catalogResponse.status, 200)
  const catalog = await catalogResponse.json()
  for (const name of ['deep', 'distillation', 'summary']) {
    assert.equal(effective.routes[name].status, 'available', `${name} route unavailable`)
    assert.equal(effective.routes[name].endpoint, 'openai-response', `${name} protocol not supported by this fixture`)
  }
  t.diagnostic(JSON.stringify({ revision: effective.revision, routes: Object.fromEntries(['deep', 'distillation', 'summary'].map(name => [name, effective.routes[name].model])) }))
  const root = await mkdtemp(join(tmpdir(), 'sophon-live-gateway-'))
  const cwd = join(root, 'workspace')
  await mkdir(cwd)
  const nonce = `MCP_RECEIPT_${randomUUID()}`
  const callsPath = join(root, 'mcp-calls.jsonl')
  const mcpPath = join(root, 'server.py')
  await writeFile(mcpPath, `import sys,json,os
for line in sys.stdin:
    request=json.loads(line)
    if 'id' not in request: continue
    method=request.get('method')
    if method=='initialize': result={'protocolVersion':request['params']['protocolVersion'],'capabilities':{'tools':{}},'serverInfo':{'name':'acceptance','version':'1'}}
    elif method=='tools/list': result={'tools':[{'name':'receipt','description':'Return the acceptance receipt for code 29.','inputSchema':{'type':'object','properties':{'code':{'type':'integer'}},'required':['code']}}]}
    elif method=='tools/call':
        with open(os.environ['CALLS_PATH'],'a') as output: output.write(json.dumps(request['params'])+'\\n')
        result={'content':[{'type':'text','text':os.environ['RECEIPT_NONCE']}]}
    else: result={}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}),flush=True)
`)
  // Transparent observation only: every inference byte comes from the real
  // configured gateway; record model/status, never credentials or bodies.
  const wire = []
  const relay = createServer(async (request, response) => {
    const cancel = new AbortController()
    response.on('close', () => cancel.abort())
    try {
      let raw = ''
      for await (const chunk of request) raw += chunk
      const body = JSON.parse(raw)
      const entry = { path: request.url, model: body.model, status: null }
      wire.push(entry)
      const upstream = await fetch(`${origin}${request.url}`, { method: request.method, headers: { ...auth, 'content-type': 'application/json' }, body: raw, signal: cancel.signal })
      entry.status = upstream.status
      response.writeHead(upstream.status, { 'content-type': upstream.headers.get('content-type') ?? 'application/json' })
      Readable.fromWeb(upstream.body).on('error', () => response.destroy()).pipe(response)
    } catch (error) {
      if (!response.destroyed) { response.writeHead(502); response.end(JSON.stringify({ error: { type: error.name } })) }
    }
  })
  await new Promise(resolve => relay.listen(0, '127.0.0.1', resolve))
  let agent
  t.after(async () => {
    try { if (agent) await agent.finalExit() }
    finally {
      await new Promise(resolve => { relay.closeAllConnections(); relay.close(resolve) })
      await rm(root, { recursive: true, force: true })
      t.diagnostic(`observed wire=${JSON.stringify(wire)}`)
    }
  })
  const models = ['deep', 'distillation', 'summary'].map(id => {
    const declared = catalog.data.find(model => model.id === effective.routes[id].model)
    assert.ok(declared)
    assert.ok(Array.isArray(declared.supported_reasoning))
    if (effective.routes[id].reasoning_effort !== null) assert.ok(declared.supported_reasoning.includes(effective.routes[id].reasoning_effort))
    return { id, supportedReasoning: declared.supported_reasoning, contextWindow: declared.limit.context, maxCompletionTokens: 2048, provider: { protocol: 'openai_responses', baseUrl: `http://127.0.0.1:${relay.address().port}/v1`, apiKey: process.env.OG_API_KEY, model: effective.routes[id].model, headers: {}, queryParams: {} } }
  })
  agent = await Agent.spawn({ executable: process.env.SOPHON_RUNTIME ?? resolve('../../target/debug/sophon-runtime'), config: {
    models, defaultModel: 'deep', compactionModel: 'summary', sessionSummaryModel: null, webSearchModel: null, imageDescriptionModel: null, browser: null, media: null,
    subagents: [{ name: 'refiner', description: 'Acceptance no-tools auxiliary child', instructions: 'Return exactly AUXILIARY_NATIVE_73. Do not use tools.', model: 'distillation', tools: [] }],
  }, env: { GROK_HOME: join(root, 'native-home'), GROK_TELEMETRY_ENABLED: 'false', GROK_TRACE_UPLOAD: 'false', GROK_TURN_SUMMARY: 'false', GROK_FEEDBACK_ENABLED: 'false' } })
  const options = { workspace: { id: 'live-acceptance', cwd }, model: 'deep', requireConfigCandidate: true, mcpServers: [], tools: [] }
  const session = await agent.createSession(options)
  const events = []
  session.subscribe(event => events.push(event))
  const receipt = await session.prompt({ turnId: 'live-native-mcp', blocks: [{ type: 'text', text: 'Call the acceptance MCP receipt tool with code 29. Then use your native shell or file tool to write its exact returned receipt text to receipt.txt in the workspace. Do not invent the receipt. Reply DONE after both operations.' }], configCandidate: {
    revision: 'live-A', model: 'deep', reasoningEffort: effective.routes.deep.reasoning_effort, instructions: 'Complete the requested acceptance task using the supplied native tools. Keep all file operations inside the workspace.', skillDirectories: [], subagentBriefs: [],
    externalMcpServers: [{ transport: 'stdio', name: 'acceptance', command: 'python3', args: [mcpPath], env: { CALLS_PATH: callsPath, RECEIPT_NONCE: nonce } }],
  } })
  assert.equal(receipt.stopReason, 'end_turn')
  assert.equal((await readFile(join(cwd, 'receipt.txt'), 'utf8')).trim(), nonce)
  assert.ok((await readFile(callsPath, 'utf8')).trim().split('\n').map(JSON.parse).some(call => call.name === 'receipt' && call.arguments.code === 29))
  assert.equal((await session.effectiveConfig()).mountedRevision, 'live-A')
  t.diagnostic('PASS native MCP tool execution + generic OS write + candidate receipt')
  const history = await session.history()
  assert.ok(history.records.some(record => record.promptId === 'live-native-mcp'))
  t.diagnostic('PASS native history snapshot')
  const auxiliary = await session.subagents.start({ id: await session.subagents.newId(), prompt: 'Return the configured auxiliary acceptance marker.', description: 'Real auxiliary route verification', subagentType: 'refiner', cwd: null, model: 'distillation' })
  assert.equal(auxiliary.state, 'completed', auxiliary.error ?? auxiliary.output)
  assert.match(auxiliary.output, /AUXILIARY_NATIVE_73/)
  assert.ok(wire.some(entry => entry.model === effective.routes.distillation.model && entry.status === 200))
  t.diagnostic('PASS explicit no-tools auxiliary child and real auxiliary gateway model')
  const beforeCompact = wire.length
  const compact = await session.prompt({ turnId: 'live-compact', blocks: [{ type: 'text', text: '/compact Preserve the acceptance receipt and completed file operation.' }] })
  assert.equal(compact.stopReason, 'end_turn')
  const compactions = events.filter(event => event.type === 'session' && event.update.type === 'compaction').map(event => event.update.value)
  t.diagnostic(`compaction phases=${JSON.stringify(compactions.map(value => value.phase))}`)
  assert.ok(wire.slice(beforeCompact).some(entry => entry.model === effective.routes.summary.model && entry.status === 200), 'compaction must call the configured real summary route')
  assert.ok(compactions.some(update => update.phase === 'completed'), 'native compaction completion required')
  t.diagnostic('PASS native compaction and configured summary gateway model')
  await session.dispose()
  const loaded = await agent.loadSession(session.id, options)
  assert.equal((await loaded.effectiveConfig()).mountedRevision, null)
  assert.ok((await loaded.history()).records.length > 0)
  await loaded.dispose()
  const exiting = agent
  agent = undefined
  await exiting.finalExit()
  t.diagnostic(`PASS checked close + cold history recovery; wire=${JSON.stringify(wire)}`)
})
