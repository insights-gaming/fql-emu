/**
 * Differential runner: replay every `conformance/cases/*.json` suite against
 * a real Fauna endpoint and against the emulator, classify every
 * disagreement, and write the report to `conformance/DIFFERENTIAL.md`.
 *
 *   FAUNA_ENDPOINT=http://localhost:8443 FAUNA_SECRET=secret npm run differential [-- <suite filter>...]
 *
 * Exits 0 with a notice when the env vars are absent, 1 on any unclassified
 * mismatch, 2 when a suite cannot be set up. The suite format is documented
 * in crates/fql-emu-core/tests/conformance.rs.
 *
 * Isolation (mirrors the Rust harness, which runs every case on a fork of
 * the seeded suite): each suite gets an empty child database of the
 * secret's database, and every case runs in a fresh grandchild database
 * seeded with the suite's `schema`, `documents` and `setup`, reached through
 * a scoped secret (`<secret>:<suite db>/<case db>:admin`). *Pure* cases
 * (no write function anywhere in the query, see `WRITE_KEY`) are split into
 * a few chunks that each share one seeded database; every other case gets
 * its own. Seeding waits for index builds to finish. Databases are deleted
 * afterwards unless `FQL_EMU_DIFF_KEEP=1`. `FQL_EMU_DIFF_CONCURRENCY`
 * (default 6) bounds how many case databases are in flight.
 *
 * Fixtures: `documents` are replayed with `Create(Ref(Collection(c), id),
 * {data, ...})`, one transaction per distinct `ts` in ascending order, so
 * relative document timestamps (and ties) survive on the real server even
 * though the absolute values cannot. The emulator side pins its clock to
 * each document's `ts` while creating it, so it reproduces the seeded state
 * exactly.
 *
 * Fidelity: case files, queries and both answers go through `ljson.ts`, so
 * number literals are sent and compared verbatim (`9223372036854775807`
 * stays exact, `1` != `1.0`, as in the Rust harness). Queries are POSTed
 * as raw JSON text with `fetch`, and handed to the wasm emulator as text.
 *
 * Comparison: timestamps (`@ts` values and `ts` fields) and snowflake
 * document ids are normalised away on both sides. Every case also runs on a
 * second emulator whose clock, fixture `ts` values and id seed are all
 * shifted; if its answer differs, the case depends on values a real server
 * cannot reproduce and is skipped (a third emulator, with only the fixture
 * timestamps shifted, tells the two reasons apart). Errors are compared like the conformance
 * test: status plus `code`, `description`, `position`, `cause` and
 * `failures` of the first error. The real answer is also checked against
 * the case's own expectation.
 *
 * Classification of each case:
 *   agree      emulator, real server and the expectation all agree;
 *   todo       the case has a `todo` marker (a known emulator bug) and only
 *              the emulator disagrees with the server;
 *   skipped    expects an emulator-only error code, depends on the clock or
 *              ids, or carries `"skip_differential": "<reason>"`;
 *   mismatch   anything else, including a wrong expectation. Exit code 1.
 * A `todo` case that now agrees everywhere is reported as a stale marker.
 *
 * Other knobs: `FQL_EMU_DIFF_REPORT=<path>` (default: conformance/DIFFERENTIAL.md
 * when no suite filter is given), `FQL_EMU_DIFF_VERBOSE=1` (also print todo
 * and skipped cases), `FQL_EMU_DIFF_DUMP=<path>` (one JSON line per case with
 * both raw answers), `FAUNA_IMAGE` (image whose digest goes in the report),
 * `FQL_EMU_CASES_DIR`. `--probe <suite> '<query>'...` runs raw queries in
 * one database seeded like that suite, on both sides, and prints the answers.
 */
import { execFileSync } from 'node:child_process'
import { readdirSync, readFileSync, writeFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { basename, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { equal, get, isNum, isObj, LNum, mapTree, obj, parse, someKey, stringify, type LJ, type LObj } from './ljson.js'

// ---------------------------------------------------------------------------
// suites

interface Case {
  name: string
  query: LJ
  expect?: LJ
  error?: LObj
  todo?: string
  skip?: string
}

interface Doc {
  collection: string
  id: string
  /** Explicit fixture timestamp (µs), if any. */
  ts?: number
  /** Every other key: document fields in wire format. */
  fields: Array<[string, LJ]>
}

interface Suite {
  label: string
  description: string
  nowMicros?: number
  schema?: LObj
  documents: Doc[]
  setup: LJ[]
  cases: Case[]
}

const str = (v: LJ | undefined): string | undefined => (typeof v === 'string' ? v : undefined)

function loadSuite(file: string): Suite {
  const root = parse(readFileSync(file, 'utf8'))
  const label = basename(file, '.json')
  const num = (v: LJ | undefined): number | undefined => (isNum(v) ? Number(v.raw) : undefined)
  const arr = (v: LJ | undefined): LJ[] => (Array.isArray(v) ? v : [])
  const schema = get(root, 'schema')
  return {
    label,
    description: str(get(root, 'description')) ?? '',
    nowMicros: num(get(root, 'now_micros')),
    schema: isObj(schema) ? schema : undefined,
    documents: arr(get(root, 'documents')).map((d) => {
      const id = get(d, 'id')
      return {
        collection: str(get(d, 'collection')) ?? '',
        id: isNum(id) ? id.raw : (str(id) ?? ''),
        ts: num(get(d, 'ts')),
        fields: isObj(d) ? [...d].filter(([k]) => !['collection', 'id', 'ts'].includes(k)) : [],
      }
    }),
    setup: arr(get(root, 'setup')),
    cases: arr(get(root, 'cases')).map((c) => {
      const error = get(c, 'error')
      return {
        name: str(get(c, 'name')) ?? '<unnamed>',
        query: get(c, 'query') ?? null,
        expect: get(c, 'expect'),
        error: isObj(error) ? error : undefined,
        todo: str(get(c, 'todo')),
        skip: str(get(c, 'skip_differential')),
      }
    }),
  }
}

/** Functions that write (or consume ids); a query containing one gets its own database. */
const WRITE_KEY = /^(create|create_\w+|update|replace|delete|insert|remove|new_id|move_database|login|logout|call)$/
const isPure = (c: Case) => !someKey(c.query, (k) => WRITE_KEY.test(k))

/**
 * Turn a wire-format *value* into an expression that evaluates to it: plain
 * objects need the `{"object": ...}` wrapper, `@` escapes are literals.
 */
function valueToExpr(v: LJ): LJ {
  if (Array.isArray(v)) return v.map(valueToExpr)
  if (!isObj(v)) return v
  const keys = [...v.keys()]
  if (keys.length === 1 && keys[0].startsWith('@')) {
    return keys[0] === '@obj' ? obj([['object', valueToExpr(v.get('@obj') ?? null)]]) : v
  }
  return obj([['object', new Map([...v].map(([k, x]) => [k, valueToExpr(x)]))]])
}

const SCHEMA_SECTIONS: Array<[string, string]> = [
  ['collections', 'create_collection'],
  ['roles', 'create_role'],
  ['functions', 'create_function'],
  ['indexes', 'create_index'],
]

function createDoc(d: Doc): LJ {
  const ref = obj([
    ['ref', obj([['collection', d.collection]])],
    ['id', d.id],
  ])
  return obj([
    ['create', ref],
    ['params', obj([['object', new Map(d.fields.map(([k, x]) => [k, valueToExpr(x)]))]])],
  ])
}

/** Documents grouped by effective `ts`, ascending; one transaction each on the real server. */
function documentGroups(suite: Suite): Array<{ ts: number | undefined; docs: Doc[] }> {
  const groups = new Map<number | undefined, Doc[]>()
  for (const d of suite.documents) {
    const ts = d.ts ?? suite.nowMicros
    groups.set(ts, [...(groups.get(ts) ?? []), d])
  }
  return [...groups]
    .sort(([a], [b]) => (a ?? 0) - (b ?? 0))
    .map(([ts, docs]) => ({ ts, docs }))
}

// ---------------------------------------------------------------------------
// answers

interface Answer {
  status: number
  body: LJ
}

const okStatus = (s: number) => (s >= 200 && s < 300 ? 200 : s)

/** Replace values that cannot be pinned on a real server. */
function normalise(v: LJ): LJ {
  return mapTree(v, (x, key) => {
    if (key === 'ts' && isNum(x)) return '<ts>'
    if (key === 'id' && typeof x === 'string' && /^\d{15,}$/.test(x)) return '<id>'
    if (isObj(x) && x.size === 1 && typeof x.get('@ts') === 'string') return obj([['@ts', '<ts>']])
    return x
  })
}

const show = (v: LJ | undefined) => (v === undefined ? 'undefined' : stringify(v))

function firstError(a: Answer): LJ {
  const errs = get(a.body, 'errors')
  return Array.isArray(errs) ? (errs[0] ?? null) : null
}

const ERROR_KEYS = ['code', 'description', 'position', 'cause', 'failures']

/** Differences between two answers, or `[]` if they agree. */
function compare(emu: Answer, real: Answer, emuName = 'emulator', realName = 'real'): string[] {
  if (emu.status !== real.status) {
    return [`status: ${emuName} ${emu.status} ${show(emu.body)}, ${realName} ${real.status} ${show(real.body)}`]
  }
  if (emu.status === 200) {
    const er = normalise(get(emu.body, 'resource') ?? null)
    const rr = normalise(get(real.body, 'resource') ?? null)
    return equal(er, rr) ? [] : [`resource: ${emuName} ${show(er)}, ${realName} ${show(rr)}`]
  }
  const ee = normalise(firstError(emu))
  const re = normalise(firstError(real))
  return ERROR_KEYS.filter((k) => !equal(get(ee, k), get(re, k))).map(
    (k) => `${k}: ${emuName} ${show(get(ee, k))}, ${realName} ${show(get(re, k))}`,
  )
}

/** Does the real answer satisfy the case's own expectation (as the Rust harness checks it)? */
function checkExpectation(c: Case, real: Answer): string[] {
  if (c.expect !== undefined) {
    if (real.status !== 200) return [`case expects ${show(c.expect)}, real errored ${show(real.body)}`]
    const got = normalise(get(real.body, 'resource') ?? null)
    return equal(normalise(c.expect), got) ? [] : [`case expects ${show(c.expect)}, real ${show(got)}`]
  }
  if (c.error) {
    if (real.status === 200) return [`case expects error ${show(c.error)}, real succeeded ${show(real.body)}`]
    const got = normalise(firstError(real))
    const out: string[] = []
    for (const [k, want] of c.error) {
      if (k === 'status') {
        if (!(isNum(want) && Number(want.raw) === real.status)) out.push(`case expects status ${show(want)}, real ${real.status}`)
      } else if (!equal(normalise(want), get(got, k))) {
        out.push(`case expects ${k} ${show(want)}, real ${show(get(got, k))}`)
      }
    }
    return out
  }
  return ['case has neither `expect` nor `error`']
}

// ---------------------------------------------------------------------------
// emulator side (the wasm class directly, so queries and answers stay text)

interface WasmEmulator {
  fork(): WasmEmulator
  free(): void
  load_schema(bundleJson: string): string
  query(exprJson: string): string
  set_auto_clock_from_micros(micros: number): void
  set_next_id(id: bigint): void
  set_now_micros(micros: number): void
}

const wasm = createRequire(import.meta.url)('../wasm/fql_emu.js') as { FqlEmulator: new () => WasmEmulator }

function emuQuery(emu: WasmEmulator, query: LJ): Answer {
  const r = parse(emu.query(stringify(query)))
  const status = get(r, 'status')
  return { status: okStatus(isNum(status) ? Number(status.raw) : -1), body: get(r, 'body') ?? null }
}

/** 1000 days and 1 µs: moves every clock-derived value without overflowing an f64. */
const SHIFT = 86_400_000_000 * 1000 + 1
const AUTO_CLOCK_START = 1_600_000_000_000_000

/**
 * A seeded emulator. `shift` moves the clock and the id seed;
 * `fixtureShift` moves the explicit `documents[].ts` values.
 */
function seedEmulator(suite: Suite, shift: number, fixtureShift = shift): WasmEmulator {
  const emu = new wasm.FqlEmulator()
  const restoreClock = () => {
    if (suite.nowMicros !== undefined) emu.set_now_micros(suite.nowMicros + shift)
    else if (shift) emu.set_auto_clock_from_micros(AUTO_CLOCK_START + shift)
  }
  restoreClock()
  if (shift) emu.set_next_id(400_000_000_000_000_000n + BigInt(shift))
  if (suite.schema) {
    const errs = emu.load_schema(stringify(suite.schema))
    if (errs !== 'null') throw new Error(`emulator: schema failed: ${errs}`)
  }
  for (const d of suite.documents) {
    if (d.ts !== undefined) emu.set_now_micros(d.ts + fixtureShift)
    const a = emuQuery(emu, createDoc(d))
    if (a.status !== 200) throw new Error(`emulator: document ${d.collection}/${d.id} failed: ${show(a.body)}`)
    restoreClock()
  }
  for (const [i, s] of suite.setup.entries()) {
    const a = emuQuery(emu, s)
    if (a.status !== 200) throw new Error(`emulator: setup[${i}] failed: ${show(a.body)}`)
  }
  return emu
}

// ---------------------------------------------------------------------------
// real side

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))

class Fauna {
  build = 'unknown'

  constructor(
    readonly endpoint: string,
    readonly secret: string,
  ) {}

  async query(secret: string, query: LJ): Promise<Answer> {
    for (let attempt = 0; ; attempt++) {
      try {
        const res = await fetch(this.endpoint, {
          method: 'POST',
          headers: {
            Authorization: `Bearer ${secret}`,
            'X-FaunaDB-API-Version': '4',
            'Content-Type': 'application/json',
          },
          body: stringify(query),
        })
        this.build = res.headers.get('x-faunadb-build') ?? this.build
        const text = await res.text()
        return { status: okStatus(res.status), body: parse(text) }
      } catch (e) {
        if (attempt >= 3) return { status: -1, body: obj([['transport_error', String(e)]]) }
        await sleep(200 * (attempt + 1))
      }
    }
  }

  /** For setup steps: retry contention / unavailability, throw on any other error. */
  async must(secret: string, query: LJ, what: string): Promise<LJ> {
    for (let attempt = 0; ; attempt++) {
      const a = await this.query(secret, query)
      if (a.status === 200) return get(a.body, 'resource') ?? null
      if (attempt < 8 && (a.status === 409 || a.status >= 500 || a.status === -1 || a.status === 401)) {
        await sleep(100 * 2 ** Math.min(attempt, 4))
        continue
      }
      throw new Error(`${what} failed: ${a.status} ${show(a.body)}`)
    }
  }
}

let dbCounter = 0

/** Per-case database statistics, for the timing summary. */
const dbStats = { databases: 0, createMs: 0, seedMs: 0 }

/** The database at `path` (slash-separated, relative to the secret's database; '' is the secret's own). */
class Database {
  constructor(
    readonly fauna: Fauna,
    readonly path: string,
  ) {}

  get secret(): string {
    return `${this.fauna.secret}:${this.path}:admin`
  }

  /** Create a child database and wait until its scoped secret answers. */
  async child(prefix: string): Promise<Database> {
    const name = `${prefix}_${process.pid.toString(36)}_${Date.now().toString(36)}_${(dbCounter++).toString(36)}`
    const secret = this.path ? this.secret : this.fauna.secret
    await this.fauna.must(secret, obj([['create_database', obj([['object', obj([['name', name]])]])]]), `create database ${name}`)
    const db = new Database(this.fauna, this.path ? `${this.path}/${name}` : name)
    await this.fauna.must(db.secret, new LNum('1'), `open database ${db.path}`)
    return db
  }

  async drop(): Promise<void> {
    if (process.env.FQL_EMU_DIFF_KEEP === '1') return
    const parts = this.path.split('/')
    const name = parts.pop() as string
    const parentSecret = parts.length ? `${this.fauna.secret}:${parts.join('/')}:admin` : this.fauna.secret
    const ref = obj([['database', name]])
    await this.fauna.query(parentSecret, obj([['delete', ref]]))
  }

  /**
   * Indexes over several collections (or `_`) are built by a background
   * task even when empty, and answer "is not active" until it finishes.
   */
  async awaitIndexes(): Promise<void> {
    const inactive = parse(
      '{"filter":{"lambda":"i","expr":{"not":{"select":["active"],"from":{"get":{"var":"i"}}}}},' +
        '"collection":{"select":["data"],"from":{"paginate":{"indexes":null},"size":100000}}}',
    )
    const deadline = Date.now() + 120_000
    for (;;) {
      const left = await this.fauna.must(this.secret, inactive, 'index status')
      if (!Array.isArray(left) || left.length === 0) return
      if (Date.now() > deadline) throw new Error(`indexes still inactive after 120s: ${show(left)}`)
      await sleep(100)
    }
  }

  async seed(suite: Suite): Promise<void> {
    for (const [section, form] of SCHEMA_SECTIONS) {
      const items = get(suite.schema ?? null, section)
      if (!Array.isArray(items) || items.length === 0) continue
      await this.fauna.must(this.secret, obj([['do', items.map((p) => obj([[form, valueToExpr(p)]]))]]), `schema ${section}`)
    }
    const indexes = get(suite.schema ?? null, 'indexes')
    if (Array.isArray(indexes) && indexes.length > 0) await this.awaitIndexes()
    for (const g of documentGroups(suite)) {
      await this.fauna.must(this.secret, obj([['do', g.docs.map(createDoc)]]), `documents ts=${g.ts}`)
    }
    for (const [i, s] of suite.setup.entries()) await this.fauna.must(this.secret, s, `setup[${i}]`)
  }
}

/** Run `tasks` with at most `limit` in flight. */
async function pool<T>(tasks: Array<() => Promise<T>>, limit: number): Promise<T[]> {
  const out: T[] = new Array(tasks.length)
  let next = 0
  const worker = async () => {
    while (next < tasks.length) {
      const i = next++
      out[i] = await tasks[i]()
    }
  }
  await Promise.all(Array.from({ length: Math.min(limit, tasks.length) }, worker))
  return out
}

// ---------------------------------------------------------------------------
// running

type Verdict = 'agree' | 'todo' | 'skip' | 'mismatch'

interface Result {
  suite: string
  name: string
  verdict: Verdict
  /** todo text, skip reason, or problem list. */
  detail: string[]
  query: LJ
  real?: Answer
  emu?: Answer
}

const EMULATOR_ONLY_CODES = new Set(['not emulated', 'emulator limit exceeded'])

interface Emulators {
  base: WasmEmulator
  /** Clock, id seed and fixture timestamps all shifted. */
  shifted: WasmEmulator
  /** Only fixture timestamps shifted; absent when the suite has none. */
  fixtures?: WasmEmulator
}

function runOnFork(emu: WasmEmulator, query: LJ): Answer {
  const fork = emu.fork()
  try {
    return emuQuery(fork, query)
  } finally {
    fork.free()
  }
}

function judge(suite: Suite, c: Case, emus: Emulators, real: Answer): Result {
  const emuAnswer = runOnFork(emus.base, c.query)
  const r = { suite: suite.label, name: c.name, query: c.query, real, emu: emuAnswer }
  if (compare(emuAnswer, runOnFork(emus.shifted, c.query)).length > 0) {
    const fixtures = emus.fixtures && compare(emuAnswer, runOnFork(emus.fixtures, c.query)).length > 0
    const reason = fixtures
      ? 'answer depends on fixture document timestamps (`documents[].ts`), which a real server cannot reproduce'
      : 'answer depends on the transaction clock or generated ids (`Now()`, written `ts` values, `NewId()`)'
    return { ...r, verdict: 'skip', detail: [reason] }
  }
  const emuVsReal = compare(emuAnswer, real)
  const expectVsReal = checkExpectation(c, real)
  if (emuVsReal.length === 0 && expectVsReal.length === 0) {
    return { ...r, verdict: 'agree', detail: c.todo ? [`stale todo marker (the emulator now agrees): ${c.todo}`] : [] }
  }
  // A `todo` covers an emulator bug, never a wrong expectation.
  if (c.todo && expectVsReal.length === 0) return { ...r, verdict: 'todo', detail: [c.todo, ...emuVsReal] }
  return { ...r, verdict: 'mismatch', detail: [...emuVsReal, ...expectVsReal] }
}

function preSkip(c: Case): string | undefined {
  if (c.skip) return c.skip
  const code = get(c.error ?? null, 'code')
  if (typeof code === 'string' && EMULATOR_ONLY_CODES.has(code)) return `expects the emulator-only error code \`${code}\``
  return undefined
}

async function runSuite(file: string, root: Database, concurrency: number): Promise<Result[]> {
  const suite = loadSuite(file)
  const emus: Emulators = {
    base: seedEmulator(suite, 0),
    shifted: seedEmulator(suite, SHIFT),
    fixtures: suite.documents.some((d) => d.ts !== undefined) ? seedEmulator(suite, 0, SHIFT) : undefined,
  }

  const results: Result[] = new Array(suite.cases.length)
  const pure: number[] = []
  const impure: number[] = []
  suite.cases.forEach((c, i) => {
    const reason = preSkip(c)
    if (reason) results[i] = { suite: suite.label, name: c.name, verdict: 'skip', detail: [reason], query: c.query }
    else (isPure(c) ? pure : impure).push(i)
  })

  // Pure cases share a seeded database per chunk; each impure case gets its own.
  const chunks = Math.max(1, Math.min(pure.length, Math.max(concurrency, Math.ceil(pure.length / 40))))
  const groups: number[][] = Array.from({ length: chunks }, (_, k) =>
    pure.slice(Math.floor((k * pure.length) / chunks), Math.floor(((k + 1) * pure.length) / chunks)),
  ).filter((g) => g.length > 0)
  for (const i of impure) groups.push([i])

  const suiteDb = await root.child(`fql_emu_diff_${suite.label}`)
  try {
    await pool(
      groups.map((g) => async () => {
        const t0 = Date.now()
        const db = await suiteDb.child('c')
        const t1 = Date.now()
        try {
          await db.seed(suite)
          dbStats.databases++
          dbStats.createMs += t1 - t0
          dbStats.seedMs += Date.now() - t1
          for (const i of g) {
            const c = suite.cases[i]
            const real = await root.fauna.query(db.secret, c.query)
            results[i] = judge(suite, c, emus, real)
          }
        } finally {
          await db.drop()
        }
      }),
      concurrency,
    )
  } finally {
    await suiteDb.drop()
    for (const e of Object.values(emus)) e?.free()
  }
  return results
}

// ---------------------------------------------------------------------------
// reporting

function tally(results: Result[]): Record<Verdict, number> {
  const t = { agree: 0, mismatch: 0, todo: 0, skip: 0 }
  for (const r of results) t[r.verdict]++
  return t
}

const fmtAnswer = (a: Answer | undefined) => (a ? `${a.status} ${show(a.status === 200 ? (get(a.body, 'resource') ?? null) : firstError(a))}` : '-')

function printSuite(label: string, results: Result[], ms: number): void {
  const t = tally(results)
  console.log(
    `\n${label}: ${t.agree} agree, ${t.mismatch} mismatched, ${t.todo} todo, ${t.skip} skipped ` +
      `(${results.length} cases, ${(ms / 1000).toFixed(1)}s)`,
  )
  for (const r of results) {
    if (r.verdict === 'mismatch') console.log(`  FAIL  ${r.name}\n          ${r.detail.join('\n          ')}`)
    else if (r.verdict === 'agree' && r.detail.length > 0) console.log(`  stale ${r.name}: ${r.detail[0]}`)
    else if (r.verdict === 'todo' && process.env.FQL_EMU_DIFF_VERBOSE) console.log(`  todo  ${r.name}: ${r.detail[0]}`)
    else if (r.verdict === 'skip' && process.env.FQL_EMU_DIFF_VERBOSE) console.log(`  skip  ${r.name}: ${r.detail[0]}`)
  }
}

function timing(seconds: number, cases: number): string {
  const n = Math.max(1, dbStats.databases)
  return (
    `${cases} cases in ${seconds.toFixed(1)}s (${((seconds * 1000) / Math.max(1, cases)).toFixed(1)} ms per case wall clock); ` +
    `${dbStats.databases} seeded case databases, mean ${(dbStats.createMs / n).toFixed(0)} ms to create and ` +
    `${(dbStats.seedMs / n).toFixed(0)} ms to seed, concurrency ${concurrency()}`
  )
}

const concurrency = () => Number(process.env.FQL_EMU_DIFF_CONCURRENCY ?? 6)

function imageDigest(): string {
  const image = process.env.FAUNA_IMAGE ?? 'fauna/faunadb:latest'
  try {
    const out = execFileSync('docker', ['image', 'inspect', image, '--format', '{{index .RepoDigests 0}}'], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
    })
    return out.trim()
  } catch {
    return `${image} (digest unavailable: docker image inspect failed)`
  }
}

const md = (s: string) => s.replace(/\|/g, '\\|').replace(/\n/g, ' ')
const code = (s: string) => '`' + s.replace(/`/g, "'") + '`'

function writeReport(path: string, bySuite: Array<[string, Result[]]>, build: string, seconds: number): void {
  const all = bySuite.flatMap(([, r]) => r)
  const t = tally(all)
  const lines: string[] = [
    '# Differential report',
    '',
    'Generated by `npm run differential` (js/src/differential.ts); do not edit by hand.',
    '',
    `- Date: ${new Date().toISOString().slice(0, 10)}`,
    `- Server image: ${code(imageDigest())}, build ${code(build)}`,
    `- Cases: ${all.length}; ${t.agree} agree, ${t.mismatch} mismatched, ${t.todo} todo (emulator bugs), ${t.skip} skipped`,
    `- Timing: ${timing(seconds, all.length)}`,
    '',
    '## Per suite',
    '',
    '| Suite | Cases | Agree | Mismatched | Todo | Skipped |',
    '|---|---:|---:|---:|---:|---:|',
    ...bySuite.map(([s, r]) => {
      const x = tally(r)
      return `| ${s} | ${r.length} | ${x.agree} | ${x.mismatch} | ${x.todo} | ${x.skip} |`
    }),
    '',
    '## Emulator bugs (`todo` cases)',
    '',
    'The case expectation matches the real server; the emulator does not.',
    '',
  ]
  // One table per distinct `todo` text, i.e. per bug.
  const bugs = new Map<string, Result[]>()
  for (const r of all.filter((x) => x.verdict === 'todo')) bugs.set(r.detail[0], [...(bugs.get(r.detail[0]) ?? []), r])
  if (bugs.size === 0) lines.push('None.', '')
  for (const [todo, rs] of bugs) {
    lines.push(`### ${md(todo)}`, '', `${rs.length} case${rs.length === 1 ? '' : 's'}.`, '')
    lines.push('| Case | Query | Server | Emulator |', '|---|---|---|---|')
    for (const r of rs) {
      const cells = [show(r.query), fmtAnswer(r.real), fmtAnswer(r.emu)].map((x) => md(code(x)))
      lines.push(`| ${r.suite} :: ${md(r.name)} | ${cells.join(' | ')} |`)
    }
    lines.push('')
  }
  const stale = all.filter((r) => r.verdict === 'agree' && r.detail.length > 0)
  if (stale.length > 0) {
    lines.push('## Stale `todo` markers', '', 'These cases now agree everywhere; remove their `todo`.', '')
    for (const r of stale) lines.push(`- ${r.suite} :: ${md(r.name)}`)
    lines.push('')
  }
  const fails = all.filter((r) => r.verdict === 'mismatch')
  if (fails.length > 0) {
    lines.push('## Unclassified mismatches', '')
    for (const r of fails) lines.push(`- **${r.suite} :: ${r.name}**: ${md(r.detail.join('; '))}`)
    lines.push('')
  }
  lines.push('## Skipped cases', '', '| Suite | Case | Reason |', '|---|---|---|')
  for (const r of all.filter((x) => x.verdict === 'skip')) lines.push(`| ${r.suite} | ${md(r.name)} | ${md(r.detail[0])} |`)
  lines.push('')
  writeFileSync(path, lines.join('\n'))
}

/**
 * `--probe <suite> <query>...`: run raw wire-format queries in one fresh
 * database seeded like the suite's cases, on both sides, and print both
 * answers verbatim. For classifying mismatches by hand.
 */
async function probe(root: Database, file: string, queries: string[]): Promise<number> {
  const suite = loadSuite(file)
  const emu = seedEmulator(suite, 0)
  const db = await root.child('fql_emu_probe')
  try {
    await db.seed(suite)
    for (const text of queries) {
      const query = parse(text)
      const real = await root.fauna.query(db.secret, query)
      const mine = emuQuery(emu, query)
      console.log(`query    ${stringify(query)}\nserver   ${real.status} ${show(real.body)}\nemulator ${mine.status} ${show(mine.body)}\n`)
    }
  } finally {
    await db.drop()
    emu.free()
  }
  return 0
}

async function main(): Promise<number> {
  const endpoint = process.env.FAUNA_ENDPOINT
  const secret = process.env.FAUNA_SECRET
  if (!endpoint || !secret) {
    console.log('differential: FAUNA_ENDPOINT and FAUNA_SECRET are not set; nothing to compare against. Skipping.')
    return 0
  }
  const repo = fileURLToPath(new URL('../../', import.meta.url))
  const dir = process.env.FQL_EMU_CASES_DIR ?? join(repo, 'conformance/cases')
  const fauna = new Fauna(endpoint.replace(/\/$/, ''), secret)
  const root = new Database(fauna, '')
  if (process.argv[2] === '--probe') return probe(root, join(dir, `${process.argv[3]}.json`), process.argv.slice(4))
  const filters = process.argv.slice(2)
  const files = readdirSync(dir)
    .filter((f) => f.endsWith('.json'))
    .filter((f) => filters.length === 0 || filters.some((p) => f.includes(p)))
    .sort()
    .map((f) => join(dir, f))
  if (files.length === 0) {
    console.error(`differential: no suites in ${dir}`)
    return 2
  }

  const started = Date.now()
  const bySuite: Array<[string, Result[]]> = []
  for (const f of files) {
    const t0 = Date.now()
    try {
      const results = await runSuite(f, root, concurrency())
      bySuite.push([basename(f, '.json'), results])
      printSuite(basename(f), results, Date.now() - t0)
    } catch (e) {
      console.error(`differential: ${basename(f)}: ${e instanceof Error ? e.message : String(e)}`)
      return 2
    }
  }
  const seconds = (Date.now() - started) / 1000
  const all = bySuite.flatMap(([, r]) => r)
  const t = tally(all)
  console.log(`\ndifferential: ${t.agree} agree, ${t.mismatch} mismatched, ${t.todo} todo, ${t.skip} skipped`)
  console.log(`differential: ${timing(seconds, all.length)}`)
  const report = process.env.FQL_EMU_DIFF_REPORT ?? (filters.length === 0 ? join(repo, 'conformance/DIFFERENTIAL.md') : '')
  if (report) {
    writeReport(report, bySuite, fauna.build, seconds)
    console.log(`differential: report written to ${report}`)
  }
  if (process.env.FQL_EMU_DIFF_DUMP) {
    // One JSON line per case with both raw answers, for scripted triage.
    const line = (r: Result) =>
      `{"suite":${JSON.stringify(r.suite)},"name":${JSON.stringify(r.name)},"verdict":"${r.verdict}",` +
      `"real":${r.real ? stringify(obj([['status', new LNum(String(r.real.status))], ['body', r.real.body]])) : 'null'},` +
      `"emu":${r.emu ? stringify(obj([['status', new LNum(String(r.emu.status))], ['body', r.emu.body]])) : 'null'}}`
    writeFileSync(process.env.FQL_EMU_DIFF_DUMP, all.map(line).join('\n') + '\n')
  }
  return t.mismatch > 0 ? 1 : 0
}

main().then(
  (code) => process.exit(code),
  (e: unknown) => {
    console.error(e)
    process.exit(2)
  },
)
