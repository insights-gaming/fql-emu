/**
 * `@insights-gaming/fql-emu`: run `faunadb` (FQL v4) queries against an in-process
 * emulator instead of a database container.
 *
 * The emulator is the Rust crate `fql-emu-core` compiled to wasm. This
 * module wires it underneath a **real** `faunadb.Client` by replacing the
 * client's HTTP layer, so the driver's own serialisation (`q.wrap`),
 * `parseJSON`, `values.*` classes, observers and error classes are used
 * unchanged.
 *
 * This is not a database and must not be benchmarked; see README.md.
 */
import { createRequire } from 'node:module'
import faunadb from 'faunadb'
import type { Client, ClientConfig, ExprArg, QueryOptions, values } from 'faunadb'

// ---------------------------------------------------------------------------
// wasm binding

/** The wasm-bindgen class from `crates/fql-emu-wasm` (see `wasm/fql_emu.d.ts`). */
interface WasmEmulator {
  free(): void
  fork(): WasmEmulator
  load_schema(bundleJson: string): string
  query(exprJson: string): string
  set_auto_clock_from_micros(micros: number): void
  set_identity(identityJson: string): string
  set_max_documents(n: number): void
  set_next_id(id: bigint): void
  set_now_micros(micros: number): void
  stats(): string
}

interface WasmModule {
  FqlEmulator: { new (): WasmEmulator; supported_forms(): string }
}

/** What `FqlEmulator.query` returns (as JSON). */
interface WasmResponse {
  status: number
  body: unknown
  txn_time_micros: number | null
}

let wasmModule: WasmModule | undefined

function wasm(): WasmModule {
  // The generated loader is CommonJS and reads the .wasm file synchronously.
  // `import.meta.url` is rewritten to the file URL of `__filename` in the
  // CJS build (see scripts/build-cjs.mjs).
  wasmModule ??= createRequire(import.meta.url)('../wasm/fql_emu.js') as WasmModule
  return wasmModule
}

// Driver members that exist at runtime but are missing from its typings.
const wrap = (faunadb.query as unknown as { wrap(v: unknown): unknown }).wrap
const ClientClosed = faunadb.errors.ClientClosed as unknown as new (message: string, description: string) => Error

// ---------------------------------------------------------------------------
// public types

/** A Fauna error object as it appears in `responseContent.errors`. */
export interface FaunaErrorJson {
  code: string
  description: string
  position: Array<string | number>
  cause?: FaunaErrorJson[]
  failures?: Array<{ field: Array<string | number>; code: string; description: string }>
}

export interface EmulatorOptions {
  /** Hard cap on stored documents (default 5 000). Exceeding it fails with `emulator limit exceeded`. */
  maxDocuments?: number
  /** Pin `Now()` and every written `ts`. A number is microseconds since the epoch. */
  now?: Date | number
  /** Seed for `NewId()` and auto-generated document ids. */
  nextId?: number | bigint
}

/** `Emulator.stats()`. Keys mirror the Rust side. */
export interface EmulatorStats {
  collections: number
  indexes: number
  functions: number
  documents: number
  max_documents: number
}

export interface SupportedForm {
  /** The form's wire name, e.g. `create_collection`. */
  name: string
  /** `false` for forms that are recognised but answer with a `not emulated` error. */
  implemented: boolean
}

/**
 * A `faunadb.Client` backed by the emulator. It *is* a `faunadb.Client`
 * (`instanceof` works); the extra members are ones the driver has at runtime
 * but leaves out of its typings.
 */
export type EmulatedClient = Client & {
  getLastTxnTime(): number | null
  syncLastTxnTime(time: number): void
}

/** A wire-format value, as accepted by `loadSchema`. */
export type SchemaBundle = {
  collections?: object[]
  roles?: object[]
  functions?: object[]
  indexes?: object[]
}

/** Thrown by `Emulator.loadSchema` when the bundle is rejected. */
export class SchemaLoadError extends Error {
  readonly errors: FaunaErrorJson[]

  constructor(errors: FaunaErrorJson[]) {
    super(
      'fql-emu: loadSchema failed:\n' +
        errors.map((e) => `  - ${e.code}: ${e.description} (at ${JSON.stringify(e.position ?? [])})`).join('\n'),
    )
    this.name = 'SchemaLoadError'
    this.errors = errors
  }
}

// ---------------------------------------------------------------------------
// HTTP adapter

/** The subset of `HttpClient.execute` options the driver passes. */
interface ExecuteOptions {
  method?: string
  path?: string
  body?: string
  streamConsumer?: unknown
  signal?: { aborted: boolean }
}

interface HttpResponse {
  status: number
  body: string
  headers: Record<string, string>
}

/**
 * Stands in for the driver's `HttpClient` (`faunadb/src/_http/index.js`):
 * `execute`, `getLastTxnTime`, `syncLastTxnTime`, `close`.
 */
class EmulatorHttp {
  #resolve: () => Emulator
  #lastSeen: number | null = null
  #closed = false

  constructor(resolve: () => Emulator) {
    this.#resolve = resolve
  }

  execute(options: ExecuteOptions = {}): Promise<HttpResponse> {
    if (this.#closed) {
      return Promise.reject(
        new ClientClosed(
          'The Client has already been closed',
          'No subsequent requests can be issued after the .close method is called. ' +
            'Consider creating a new Client instance',
        ),
      )
    }
    if (options.streamConsumer != null) {
      return Promise.reject(new Error('fql-emu: streaming is not emulated'))
    }
    const method = options.method ?? 'GET'
    const path = (options.path ?? '').replace(/^\//, '')
    if (method === 'GET' && path === 'ping') {
      return Promise.resolve(jsonResponse(200, { resource: 'Scope write is OK' }, null))
    }
    if (method !== 'POST' || path !== '') {
      return Promise.reject(new Error(`fql-emu: only query requests are emulated (got ${method} /${path})`))
    }
    const r = this.#resolve().runRaw(options.body ?? 'null')
    return Promise.resolve(jsonResponse(r.status, r.body, r.txn_time_micros))
  }

  getLastTxnTime(): number | null {
    return this.#lastSeen
  }

  syncLastTxnTime(time: number): void {
    if (this.#lastSeen == null || this.#lastSeen < time) {
      this.#lastSeen = time
    }
  }

  close(_opts?: { force?: boolean }): Promise<void> {
    this.#closed = true
    return Promise.resolve()
  }
}

function jsonResponse(status: number, body: unknown, txnTime: number | null): HttpResponse {
  const headers: Record<string, string> = { 'content-type': 'application/json;charset=utf-8' }
  if (txnTime != null) headers['x-txn-time'] = String(txnTime)
  return { status, body: JSON.stringify(body), headers }
}

function makeClient(resolve: () => Emulator, opts: Partial<ClientConfig> = {}): EmulatedClient {
  // The secret is never checked (every query runs as admin) but the driver
  // wants one; the original HttpClient it builds opens no connections.
  const client = new faunadb.Client({ secret: 'fql-emu', ...opts }) as EmulatedClient
  ;(client as unknown as { _http: EmulatorHttp })._http = new EmulatorHttp(resolve)
  return client
}

// ---------------------------------------------------------------------------
// Emulator

/** Default start of the auto clock (matches `store::Database::new`). */
const DEFAULT_CLOCK_MICROS = 1_600_000_000_000_000

function toMicros(d: Date | number): number {
  if (d instanceof Date) {
    const ms = d.getTime()
    if (Number.isNaN(ms)) throw new RangeError('fql-emu: invalid Date')
    return ms * 1000
  }
  if (!Number.isSafeInteger(d)) throw new RangeError(`fql-emu: expected integer microseconds, got ${d}`)
  return d
}

/**
 * One emulated database. Cheap to fork; forks share nothing.
 *
 * Every query is one transaction: it either commits all its writes or (on
 * any error) none of them.
 */
export class Emulator {
  #wasm: WasmEmulator
  /** Latest transaction time seen, so `setNow(null)` can resume monotonically. */
  #clockMicros: number
  #default: EmulatedClient | undefined

  private constructor(inner: WasmEmulator, clockMicros: number) {
    this.#wasm = inner
    this.#clockMicros = clockMicros
  }

  /** Prefer `createEmulator()`. */
  static create(opts: EmulatorOptions = {}): Emulator {
    const emu = new Emulator(new (wasm().FqlEmulator)(), DEFAULT_CLOCK_MICROS)
    if (opts.maxDocuments !== undefined) emu.setMaxDocuments(opts.maxDocuments)
    if (opts.now !== undefined) emu.setNow(opts.now)
    if (opts.nextId !== undefined) emu.setNextId(opts.nextId)
    return emu
  }

  /** Every form the emulator recognises, and whether it is implemented. */
  static supportedForms(): SupportedForm[] {
    const pairs = JSON.parse(wasm().FqlEmulator.supported_forms()) as Array<[string, boolean]>
    return pairs.map(([name, implemented]) => ({ name, implemented }))
  }

  /**
   * A real `faunadb.Client` whose HTTP layer is this emulator. Options are
   * passed to the `Client` constructor (`observer`, `queryTimeout`, ...);
   * connection options are ignored. Each call returns a new client with its
   * own last-seen transaction time.
   */
  client(opts?: Partial<ClientConfig>): EmulatedClient {
    return makeClient(() => this, opts)
  }

  /** Run a query on an internal client. Same semantics as `client().query`. */
  query<T = unknown>(expr: ExprArg, options?: QueryOptions): Promise<T> {
    this.#default ??= this.client()
    return this.#default.query<T>(expr, options)
  }

  /** An independent copy of the current state (documents, schema, clock, id seed, limits). */
  fork(): Emulator {
    return new Emulator(this.#wasm.fork(), this.#clockMicros)
  }

  /**
   * Load a schema bundle (see docs/schema-bundle.md): wire-format
   * `CreateCollection` / `CreateRole` / `CreateFunction` / `CreateIndex`
   * params. Throws `SchemaLoadError` listing the Fauna errors on failure.
   */
  loadSchema(bundle: SchemaBundle | string): void {
    const json = typeof bundle === 'string' ? bundle : JSON.stringify(bundle)
    const out = this.#wasm.load_schema(json)
    if (out !== 'null') throw new SchemaLoadError(JSON.parse(out) as FaunaErrorJson[])
  }

  /**
   * Pin `Now()`, `Time("now")` and every written `ts` (a number is
   * microseconds since the epoch). `null` returns to the auto clock, which
   * advances 1 µs per transaction from the latest time seen.
   */
  setNow(d: Date | number | null): void {
    if (d === null) {
      this.#wasm.set_auto_clock_from_micros(this.#clockMicros)
      return
    }
    const micros = toMicros(d)
    this.#wasm.set_now_micros(micros)
    this.#clockMicros = Math.max(this.#clockMicros, micros)
  }

  /** Seed for `NewId()` and generated document ids. Ids above 2^53 must be passed as a bigint. */
  setNextId(n: number | bigint): void {
    if (typeof n === 'number' && !Number.isSafeInteger(n)) {
      throw new RangeError(`fql-emu: setNextId(${n}) is not a safe integer; pass a bigint`)
    }
    const id = BigInt(n)
    if (id < 0n || id >= 1n << 64n) throw new RangeError(`fql-emu: setNextId(${n}) out of range`)
    this.#wasm.set_next_id(id)
  }

  /**
   * What `CurrentIdentity()` returns (`null` clears it). Roles/ABAC are not
   * enforced: queries always run as admin.
   */
  setIdentity(ref: values.Ref | null): void {
    const out = this.#wasm.set_identity(JSON.stringify(wrap(ref)))
    if (out !== 'null') throw new Error(`fql-emu: setIdentity failed: ${out}`)
  }

  /** Change the document cap (default 5 000). */
  setMaxDocuments(n: number): void {
    if (!Number.isSafeInteger(n) || n < 0) throw new RangeError(`fql-emu: invalid maxDocuments ${n}`)
    this.#wasm.set_max_documents(n)
  }

  stats(): EmulatorStats {
    return JSON.parse(this.#wasm.stats()) as EmulatorStats
  }

  /**
   * Release the wasm memory now instead of at garbage collection. The
   * emulator (and its clients) must not be used afterwards.
   */
  free(): void {
    this.#wasm.free()
  }

  /** @internal Run one serialised query; used by the HTTP adapter. */
  runRaw(body: string): WasmResponse {
    const r = JSON.parse(this.#wasm.query(body)) as WasmResponse
    if (r.txn_time_micros != null) this.#clockMicros = Math.max(this.#clockMicros, r.txn_time_micros)
    return r
  }
}

/** A fresh, empty emulated database. */
export function createEmulator(opts?: EmulatorOptions): Emulator {
  return Emulator.create(opts)
}

// ---------------------------------------------------------------------------
// vitest helper

/** The shape of the app's `faunaClient` module. */
export interface MockFaunaClientModule {
  faunaClient: EmulatedClient
  query: <T = unknown>(expr: ExprArg, options?: QueryOptions) => Promise<T>
}

/**
 * Build a replacement for a module that exports `{ faunaClient }` (and a
 * `query` shortcut). Pass an emulator, or a getter so each test can swap in
 * a fresh fork without re-mocking:
 *
 * ```ts
 * import { beforeEach, vi } from 'vitest'
 * import type { Emulator } from '@insights-gaming/fql-emu'
 *
 * const state = vi.hoisted(() => ({ emu: undefined as unknown as Emulator }))
 * vi.mock('./faunaClient', async () => {
 *   const { mockFaunaClient } = await import('@insights-gaming/fql-emu')
 *   return mockFaunaClient(() => state.emu)
 * })
 *
 * const { createEmulator } = await import('@insights-gaming/fql-emu')
 * const base = createEmulator()
 * base.loadSchema(bundle)
 * beforeEach(() => { state.emu = base.fork() })
 * ```
 *
 * With a single emulator: `vi.mock('./faunaClient', () => mockFaunaClient(emu))`
 * where `emu` comes from `vi.hoisted`.
 */
export function mockFaunaClient(emu: Emulator | (() => Emulator)): MockFaunaClientModule {
  const resolve = typeof emu === 'function' ? emu : () => emu
  const faunaClient = makeClient(resolve)
  return {
    faunaClient,
    query: <T = unknown>(expr: ExprArg, options?: QueryOptions) => faunaClient.query<T>(expr, options),
  }
}
