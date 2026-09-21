import type { ClientFrame } from './generated/ClientFrame.js'
import type { ServerFrame } from './generated/ServerFrame.js'
import type { Request } from './generated/Request.js'
import type { RuntimeConfig } from './generated/RuntimeConfig.js'
import type { RuntimeEvent } from './generated/RuntimeEvent.js'
import type { SessionOptions } from './generated/SessionOptions.js'
import type { SessionDescriptor } from './generated/SessionDescriptor.js'
import type { HistorySnapshot } from './generated/HistorySnapshot.js'
import type { Prompt } from './generated/Prompt.js'
import type { PromptReceipt } from './generated/PromptReceipt.js'
import type { CallbackContext } from './generated/CallbackContext.js'
import type { JsonValue } from './generated/serde_json/JsonValue.js'
import type { SubagentStart } from './generated/SubagentStart.js'
import type { SubagentResult } from './generated/SubagentResult.js'
import type { SubagentSnapshot } from './generated/SubagentSnapshot.js'
import type { SubagentHandle } from './generated/SubagentHandle.js'
import type { SchedulerSnapshot } from './generated/SchedulerSnapshot.js'
import type { ScheduledTask } from './generated/ScheduledTask.js'
import type { ScheduledTaskCreate } from './generated/ScheduledTaskCreate.js'
import type { ScheduledTaskUpdate } from './generated/ScheduledTaskUpdate.js'
import type { SchedulerMutationResult } from './generated/SchedulerMutationResult.js'
import type { Version } from './generated/Version.js'
import type { TerminalRequest } from './generated/TerminalRequest.js'
import type { TerminalEvent } from './generated/TerminalEvent.js'
import type { TerminalOpenResult } from './generated/TerminalOpenResult.js'
import type { TerminalCloseResult } from './generated/TerminalCloseResult.js'
import type { QueueSnapshot } from './generated/QueueSnapshot.js'
import type { SubagentCancelIdResult } from './generated/SubagentCancelIdResult.js'
import type { SubagentCancelOutcome } from './generated/SubagentCancelOutcome.js'

export type { ClientFrame, ServerFrame, Request, RuntimeConfig, RuntimeEvent, SessionOptions, SessionDescriptor, HistorySnapshot, Prompt, PromptReceipt, CallbackContext, JsonValue }
export type { SubagentStart, SubagentResult, SubagentSnapshot, SubagentHandle, SchedulerSnapshot, ScheduledTask, ScheduledTaskCreate, ScheduledTaskUpdate, SchedulerMutationResult, Version }
export type { NativeMediaConfig } from './generated/NativeMediaConfig.js'
export type { MediaRoute } from './generated/MediaRoute.js'
export type { SubagentDefinition } from './generated/SubagentDefinition.js'
export type { SubagentEvent } from './generated/SubagentEvent.js'
export type { CompactionUpdate } from './generated/CompactionUpdate.js'
export type { SchedulerCadence } from './generated/SchedulerCadence.js'
export type { SchedulerDispatch } from './generated/SchedulerDispatch.js'
export type { ScheduledInvocation } from './generated/ScheduledInvocation.js'
export type { NativePromptOrigin } from './generated/NativePromptOrigin.js'
export type { TurnCompletion } from './generated/TurnCompletion.js'
export type { TurnUsage } from './generated/TurnUsage.js'
export type { QueueSnapshot }
export type { SubagentCancelIdResult, SubagentCancelOutcome }
export type { TerminalRequest, TerminalEvent }
export type { TerminalOpenResult, TerminalCloseResult }
export type TerminalResult<T extends TerminalRequest> = T extends { action: 'open' } ? TerminalOpenResult : T extends { action: 'close' } ? TerminalCloseResult : Record<string, never>
export type TerminalStreamEvent = TerminalEvent | { type: 'gap'; dropped: number }
export type { BrowserConfig } from './generated/BrowserConfig.js'
export type { HistoryRecord } from './generated/HistoryRecord.js'
export type { PromptBlock } from './generated/PromptBlock.js'
export type { ProviderRoute } from './generated/ProviderRoute.js'
export type { RuntimeModel } from './generated/RuntimeModel.js'
export type { ToolSpec } from './generated/ToolSpec.js'
export type { ToolCall } from './generated/ToolCall.js'
export type { Update } from './generated/Update.js'
export type { Workspace } from './generated/Workspace.js'

/** Transport implements delivery only, never an agent loop or prompt queue. */
export interface Transport {
  readonly frames: AsyncIterable<ServerFrame>
  readonly closed: Promise<{ code: number | null; signal: string | null }>
  send(frame: ClientFrame): Promise<void>
  close(): Promise<void>
}

export interface CallbackRequest {
  method: string
  params: JsonValue
  context: CallbackContext | null
  signal: AbortSignal
}

export interface ClientOptions {
  onCallback?: (request: CallbackRequest) => Promise<JsonValue>
  /** Observer failures are isolated from native execution. */
  onObserverError?: (error: unknown) => void
}

export interface SpawnOptions extends ClientOptions {
  executable: string
  args?: string[]
  env?: Record<string, string | undefined>
  config: RuntimeConfig
  /** Runtime diagnostics only. Never receives private stdout frames. */
  onStderr?: (chunk: string) => void
}

export class RuntimeError extends Error {
  constructor(readonly code: string, message: string) { super(message); this.name = 'RuntimeError' }
}

type Pending = { resolve: (result: JsonValue) => void; reject: (error: unknown) => void }
type EventListener = (event: RuntimeEvent, sequence: number) => void

/** Exactly one Runtime Agent. Session handles do not own subprocesses. */
export class Agent {
  private nextId = 0
  private sequence = 0
  private pending = new Map<string, Pending>()
  private callbacks = new Map<string, AbortController>()
  private listeners = new Set<EventListener>()
  private browserListeners = new Set<(frame: JsonValue) => void>()
  private terminalListeners = new Set<(event: TerminalStreamEvent) => void>()
  private stopped: Error | undefined
  private exiting = false
  private readyResolve!: () => void
  private readyReject!: (error: unknown) => void
  private readonly ready = new Promise<void>((resolve, reject) => { this.readyResolve = resolve; this.readyReject = reject })
  private readonly reader: Promise<void>

  private constructor(private readonly transport: Transport, private readonly options: ClientOptions) {
    this.reader = this.read()
  }

  static async spawn(options: SpawnOptions): Promise<Agent> {
    const { StdioTransport } = await import('./stdio.js')
    const transport = new StdioTransport(options)
    try { return await Agent.connect(transport, options.config, options) }
    catch (error) { await transport.close(); throw error }
  }

  static async connect(transport: Transport, config: RuntimeConfig, options: ClientOptions = {}): Promise<Agent> {
    const agent = new Agent(transport, options)
    await agent.ready
    await agent.request({ method: 'initialize', config })
    return agent
  }

  subscribe(listener: EventListener): () => void {
    this.listeners.add(listener)
    return () => { this.listeners.delete(listener) }
  }

  subscribeBrowserFrames(listener: (frame: JsonValue) => void): () => void {
    this.browserListeners.add(listener)
    return () => { this.browserListeners.delete(listener) }
  }

  subscribeTerminalEvents(listener: (event: TerminalStreamEvent) => void): () => void {
    this.terminalListeners.add(listener)
    return () => { this.terminalListeners.delete(listener) }
  }

  private emit(event: RuntimeEvent, sequence: number): void {
    for (const listener of this.listeners) {
      try { listener(event, sequence) } catch (error) { this.options.onObserverError?.(error) }
    }
  }

  private fail(error: Error): void {
    if (this.stopped) return
    this.stopped = error
    this.readyReject(error)
    for (const pending of this.pending.values()) pending.reject(error)
    this.pending.clear()
    for (const controller of this.callbacks.values()) controller.abort(error)
    this.callbacks.clear()
  }

  private async read(): Promise<void> {
    try {
      for await (const frame of this.transport.frames) {
        switch (frame.type) {
          case 'ready':
            if (frame.protocolVersion !== 1) throw new RuntimeError('protocol_version', `Unsupported Runtime protocol ${frame.protocolVersion}`)
            this.readyResolve()
            break
          case 'response': case 'error': {
            const pending = this.pending.get(frame.id)
            if (!pending) throw new RuntimeError('unknown_response', 'Runtime returned an unknown request ID')
            this.pending.delete(frame.id)
            if (frame.type === 'response') pending.resolve(frame.result)
            else pending.reject(new RuntimeError(frame.error.code, frame.error.message))
            break
          }
          case 'event':
            if (frame.sequence !== this.sequence + 1) this.emit({ type: 'gap', dropped: Math.max(1, frame.sequence - this.sequence - 1) }, frame.sequence)
            this.sequence = frame.sequence
            this.emit(frame.event, frame.sequence)
            break
          case 'callback': void this.callback(frame); break
          case 'browser_frame':
            for (const listener of this.browserListeners) {
              try { listener(frame.frame) } catch (error) { this.options.onObserverError?.(error) }
            }
            break
          case 'terminal': case 'terminal_gap':
            for (const listener of this.terminalListeners) {
              try { listener(frame.type === 'terminal' ? frame.event : { type: 'gap', dropped: frame.dropped }) } catch (error) { this.options.onObserverError?.(error) }
            }
            break
          case 'callback_cancelled': this.callbacks.get(frame.id)?.abort(new RuntimeError('cancelled', 'Native tool call was cancelled')); break
          default: throw new RuntimeError('invalid_frame', 'Unrecognized Runtime frame')
        }
      }
      this.fail(new RuntimeError('runtime_closed', 'Runtime transport closed'))
    } catch (error) {
      this.fail(error instanceof Error ? error : new Error(String(error)))
    }
  }

  private async callback(frame: Extract<ServerFrame, { type: 'callback' }>): Promise<void> {
    const controller = new AbortController()
    this.callbacks.set(frame.id, controller)
    try {
      if (!this.options.onCallback) throw new RuntimeError('unsupported_callback', `No handler for ${frame.method}`)
      const result = await this.options.onCallback({ method: frame.method, params: frame.params, context: frame.context, signal: controller.signal })
      if (!controller.signal.aborted) await this.transport.send({ type: 'callback_result', id: frame.id, result, error: null })
    } catch (error) {
      if (!controller.signal.aborted) {
        try { await this.transport.send({ type: 'callback_result', id: frame.id, result: null, error: { code: error instanceof RuntimeError ? error.code : 'callback_failed', message: error instanceof Error ? error.message : 'Product callback failed' } }) }
        catch (error) { this.fail(error instanceof Error ? error : new Error(String(error))) }
      }
    } finally { this.callbacks.delete(frame.id) }
  }

  /** Correlation is transport-only; Grok Build owns the native FIFO. */
  request(request: Request): Promise<JsonValue> {
    if (this.stopped) return Promise.reject(this.stopped)
    if (this.exiting && request.method !== 'final_exit') return Promise.reject(new RuntimeError('admission_closed', 'Runtime is exiting'))
    const id = String(++this.nextId)
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject })
      void this.transport.send({ type: 'request', id, request }).catch(error => { this.pending.delete(id); reject(error) })
    })
  }

  async createSession(options: SessionOptions): Promise<Session> {
    return new Session(this, await this.request({ method: 'create_session', options }) as unknown as SessionDescriptor)
  }
  async loadSession(id: string, options: SessionOptions): Promise<Session> {
    return new Session(this, await this.request({ method: 'load_session', id, options }) as unknown as SessionDescriptor)
  }
  async resumeSession(id: string, options: SessionOptions): Promise<Session> {
    return new Session(this, await this.request({ method: 'resume_session', id, options }) as unknown as SessionDescriptor)
  }
  listSessions(cwd: string | null = null, cursor: string | null = null): Promise<JsonValue> { return this.request({ method: 'list_sessions', cwd, cursor }) }
  extension(name: string, params: JsonValue): Promise<JsonValue> { return this.request({ method: 'extension', sessionId: null, name, params }) }
  browser(args: JsonValue): Promise<JsonValue> { return this.request({ method: 'browser', args }) }
  async terminal<T extends TerminalRequest>(request: T): Promise<TerminalResult<T>> { return await this.request({ method: 'terminal', request }) as unknown as TerminalResult<T> }
  skills(cwd: string): Promise<JsonValue> { return this.extension('x.ai/skills/list', { cwd }) }
  quiesce(timeoutMs = 30_000): Promise<JsonValue> { return this.request({ method: 'quiesce', timeoutMs }) }

  /** Native checked persistence receipt AND successful process/transport exit. */
  async finalExit(timeoutMs = 30_000): Promise<void> {
    if (this.exiting) throw new RuntimeError('admission_closed', 'finalExit already requested')
    this.exiting = true
    let timer: ReturnType<typeof setTimeout> | undefined
    const deadline = new Promise<never>((_, reject) => { timer = setTimeout(() => reject(new RuntimeError('exit_timeout', 'Runtime did not finish checked exit')), timeoutMs + 5_000) })
    try {
      await Promise.race([(async () => {
        await this.request({ method: 'final_exit', timeoutMs })
        const exit = await this.transport.closed
        await this.reader
        if (exit.code !== 0 || exit.signal !== null) throw new RuntimeError('exit_failed', `Runtime exit code ${exit.code}, signal ${exit.signal}`)
      })(), deadline])
    } finally { if (timer) clearTimeout(timer) }
  }
}

export class Session {
  readonly id: string
  readonly subagents: Subagents
  readonly scheduler: Scheduler
  constructor(private readonly agent: Agent, readonly descriptor: SessionDescriptor) {
    this.id = descriptor.id
    this.subagents = new Subagents(agent, this.id)
    this.scheduler = new Scheduler(agent, this.id)
  }
  subscribe(listener: EventListener): () => void {
    return this.agent.subscribe((event, sequence) => {
      if (event.type === 'gap' || event.type === 'extension' || (event.type === 'history_record' ? event.record.sessionId === this.id : event.type === 'subagent' ? event.event.parentSessionId === this.id : event.type === 'queue' ? event.snapshot.sessionId === this.id : event.sessionId === this.id)) listener(event, sequence)
    })
  }
  async prompt(prompt: Prompt): Promise<PromptReceipt> { return await this.agent.request({ method: 'prompt', sessionId: this.id, prompt }) as unknown as PromptReceipt }
  async history(): Promise<HistorySnapshot> { return await this.agent.request({ method: 'history', sessionId: this.id }) as unknown as HistorySnapshot }
  async cancel(turnId: string | null = null): Promise<void> { await this.agent.request({ method: 'cancel', sessionId: this.id, turnId }) }
  async dispose(): Promise<void> { await this.agent.request({ method: 'dispose', sessionId: this.id }) }
  async queue(): Promise<QueueSnapshot> { return await this.agent.request({ method: 'queue', sessionId: this.id }) as unknown as QueueSnapshot }
  readArtifact(path: string): Promise<JsonValue> { return this.agent.request({ method: 'read_artifact', sessionId: this.id, path }) }
  async setModel(model: string, metadata: Record<string, JsonValue> = {}): Promise<void> { await this.agent.request({ method: 'set_model', sessionId: this.id, model, metadata }) }
  extension(name: string, params: JsonValue): Promise<JsonValue> { return this.agent.request({ method: 'extension', sessionId: this.id, name, params }) }
}

/** Native child coordinator, never an independent host Session loop. */
export class Subagents {
  constructor(private readonly agent: Agent, private readonly sessionId: string) {}
  /** Allocate a native UUIDv7 logical ID before admission, usable for cancelId. */
  async newId(): Promise<string> { return await this.agent.request({ method: 'subagent_new_id', sessionId: this.sessionId }) as string }
  async start(request: SubagentStart): Promise<SubagentResult> { return await this.agent.request({ method: 'subagent_start', sessionId: this.sessionId, request }) as unknown as SubagentResult }
  async query(id: string): Promise<SubagentSnapshot | null> { return await this.agent.request({ method: 'subagent_query', sessionId: this.sessionId, id }) as unknown as SubagentSnapshot | null }
  /** Waits for native state, without adding a host queue or retrying execution. */
  async wait(id: string, timeoutMs = 300_000): Promise<SubagentSnapshot> { return await this.agent.request({ method: 'subagent_wait', sessionId: this.sessionId, id, timeoutMs }) as unknown as SubagentSnapshot }
  async cancel(target: SubagentHandle): Promise<SubagentCancelOutcome> { return await this.agent.request({ method: 'subagent_cancel', sessionId: this.sessionId, target }) as unknown as SubagentCancelOutcome }
  /** Fences even an unregistered ID. Active-child exit must still be awaited separately. */
  async cancelId(id: string): Promise<SubagentCancelIdResult> { return await this.agent.request({ method: 'subagent_cancel_id', sessionId: this.sessionId, id }) as unknown as SubagentCancelIdResult }
}

/** Mutations carry native revision and caller-chosen idempotency key. */
export class Scheduler {
  constructor(private readonly agent: Agent, private readonly sessionId: string) {}
  async list(): Promise<SchedulerSnapshot> { return await this.agent.request({ method: 'scheduler_list', sessionId: this.sessionId }) as unknown as SchedulerSnapshot }
  async create(operationId: string, expected: Version, task: ScheduledTaskCreate): Promise<SchedulerMutationResult<ScheduledTask>> { return await this.agent.request({ method: 'scheduler_create', sessionId: this.sessionId, operationId, expected, task }) as unknown as SchedulerMutationResult<ScheduledTask> }
  async update(operationId: string, expected: Version, task: ScheduledTaskUpdate): Promise<SchedulerMutationResult<ScheduledTask>> { return await this.agent.request({ method: 'scheduler_update', sessionId: this.sessionId, operationId, expected, task }) as unknown as SchedulerMutationResult<ScheduledTask> }
  async delete(operationId: string, expected: Version, id: string): Promise<SchedulerMutationResult<boolean>> { return await this.agent.request({ method: 'scheduler_delete', sessionId: this.sessionId, operationId, expected, id }) as unknown as SchedulerMutationResult<boolean> }
}
