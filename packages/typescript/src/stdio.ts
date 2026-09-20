import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process'
import { createInterface } from 'node:readline'
import type { ClientFrame, ServerFrame, SpawnOptions, Transport } from './index.js'
import { RuntimeError } from './index.js'

/** Private stdio only. No CLI flags containing credentials and no CLI parsing. */
export class StdioTransport implements Transport {
  readonly frames: AsyncIterable<ServerFrame>
  readonly closed: Promise<{ code: number | null; signal: string | null }>
  private readonly process: ChildProcessWithoutNullStreams

  constructor(options: Pick<SpawnOptions, 'executable' | 'args' | 'env' | 'onStderr'>) {
    this.process = spawn(options.executable, options.args ?? [], {
      env: { ...process.env, ...options.env },
      stdio: ['pipe', 'pipe', 'pipe'],
      windowsHide: true,
    })
    const lines = createInterface({ input: this.process.stdout, crlfDelay: Infinity })
    this.closed = new Promise((resolve, reject) => {
      this.process.once('error', reject)
      this.process.once('close', (code, signal) => resolve({ code, signal }))
    })
    // Retain rejection for checked callers without an unhandled early rejection.
    void this.closed.catch(() => {})
    this.process.stderr.setEncoding('utf8')
    this.process.stderr.on('data', (chunk: string) => options.onStderr?.(chunk))
    this.frames = (async function* () {
      for await (const line of lines) {
        let frame: ServerFrame
        try { frame = JSON.parse(line) as ServerFrame }
        catch { throw new RuntimeError('invalid_frame', 'Runtime stdout contained non-protocol data') }
        if (!frame || typeof frame !== 'object' || typeof frame.type !== 'string') throw new RuntimeError('invalid_frame', 'Malformed Runtime frame')
        yield frame
      }
    })()
  }

  send(frame: ClientFrame): Promise<void> {
    return new Promise((resolve, reject) => { this.process.stdin.write(JSON.stringify(frame) + '\n', error => error ? reject(error) : resolve()) })
  }

  /** EOF requests checked Runtime shutdown. Does not claim it succeeded. */
  async close(): Promise<void> { this.process.stdin.end(); await this.closed }
}
