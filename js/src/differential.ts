/**
 * Differential runner: replay every `conformance/cases/*.json` suite against
 * a real Fauna endpoint and against the emulator, and report where they
 * disagree.
 *
 *   FAUNA_ENDPOINT=http://localhost:8443 FAUNA_SECRET=secret npm run differential [-- <suite filter>...]
 *
 * Exits 0 with a notice when the env vars are absent, 1 on any mismatch, 2
 * when a suite cannot be set up. The suite format is documented in
 * crates/fql-emu-core/tests/conformance.rs.
 *
 * Isolation: each suite runs in a fresh child database (`CreateDatabase` +
 * `CreateKey`), deleted afterwards unless `FQL_EMU_DIFF_KEEP=1`. If the secret
 * cannot create databases, the secret's own database is used instead; it
 * must be empty, and it is wiped after each suite. Within a suite the cases
 * run in file order against the same real database (the emulator runs each
 * on a fresh fork), so a case that writes can affect later cases there.
 *
 * Comparison: timestamps (`@ts` values and `ts` fields) and snowflake
 * document ids are normalised away on both sides, since neither the clock
 * nor `NewId()` can be pinned on a real server. Cases whose emulator answer
 * still changes when the clock and id seed move (e.g. `ToMicros(Now())`) are
 * reported as skipped. Errors are compared like the conformance test:
 * status plus `code`, `description`, `position`, `cause` and `failures` of
 * the first error. Cases expecting an emulator-only error code
 * (`not emulated`, `emulator limit exceeded`) are skipped.
 */
import { readdirSync, readFileSync } from 'node:fs'
import { basename, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { isDeepStrictEqual } from 'node:util'
import faunadb from 'faunadb'
import type { Client } from 'faunadb'
import { createEmulator, type Emulator, type SchemaBundle } from './index.js'

const q = faunadb.query

type Json = null | boolean | number | string | Json[] | { [k: string]: Json }

interface Case {
  name?: string
  query: Json
  expect?: Json
  error?: { [k: string]: Json }
  todo?: string
}

interface Suite {
  description?: string
  now_micros?: number
  schema?: SchemaBundle & { [k: string]: Json[] }
  setup?: Json[]
  cases?: Case[]
}

interface Answer {
  status: number
  body: Json
}

const EMULATOR_ONLY_CODES = new Set(['not emulated', 'emulator limit exceeded'])
const SCHEMA_SECTIONS: Array<[string, string]> = [
  ['collections', 'create_collection'],
  ['roles', 'create_role'],
  ['functions', 'create_function'],
  ['indexes', 'create_index'],
]

// ---------------------------------------------------------------------------
// wire helpers

/** Send a raw wire-format query through a driver client and capture the raw answer. */
async function run(client: Client, query: Json): Promise<Answer> {
  let answer: Answer | undefined
  try {
    await client.query(new faunadb.Expr(query as object), {
      observer: (r) => {
        // The server answers 201 for writes; the emulator always 200. Treat 2xx alike.
        const status = r.statusCode >= 200 && r.statusCode < 300 ? 200 : r.statusCode
        answer = { status, body: JSON.parse(r.responseRaw) as Json }
      },
    })
  } catch (e) {
    if (answer) return answer
    return { status: -1, body: { transport_error: String(e) } }
  }
  return answer ?? { status: -1, body: { transport_error: 'no response' } }
}

/**
 * Turn a wire-format *value* (schema params) into an expression that
 * evaluates to it: plain objects need the `{"object": ...}` wrapper, `@`
 * escapes are literals already.
 */
function valueToExpr(v: Json): Json {
  if (Array.isArray(v)) return v.map(valueToExpr)
  if (v === null || typeof v !== 'object') return v
  const keys = Object.keys(v)
  if (keys.length === 1 && keys[0].startsWith('@')) {
    return keys[0] === '@obj' ? { object: valueToExpr(v['@obj']) } : v
  }
  return { object: Object.fromEntries(keys.map((k) => [k, valueToExpr(v[k])])) }
}

function schemaQueries(schema: Suite['schema']): Json[] {
  const out: Json[] = []
  for (const [section, form] of SCHEMA_SECTIONS) {
    for (const params of schema?.[section] ?? []) out.push({ [form]: valueToExpr(params as Json) })
  }
  return out
}

/** Replace values that cannot be pinned on a real server. */
function normalise(v: Json, key?: string): Json {
  if (Array.isArray(v)) return v.map((x) => normalise(x))
  if (typeof v === 'number' && key === 'ts') return '<ts>'
  if (v === null || typeof v !== 'object') return v
  if (typeof v['@ts'] === 'string' && Object.keys(v).length === 1) return { '@ts': '<ts>' }
  const out: { [k: string]: Json } = {}
  for (const [k, x] of Object.entries(v)) {
    out[k] = k === 'id' && typeof x === 'string' && /^\d{15,}$/.test(x) ? '<id>' : normalise(x, k)
  }
  return out
}

function firstError(a: Answer): { [k: string]: Json } {
  const errs = (a.body as { errors?: Json[] } | null)?.errors
  return (Array.isArray(errs) && (errs[0] as { [k: string]: Json })) || {}
}

/** Differences between two answers, or `[]` if they agree. */
function compare(emu: Answer, real: Answer): string[] {
  const e = normalise(emu.body)
  const r = normalise(real.body)
  if (emu.status !== real.status) return [`status: emulator ${emu.status}, real ${real.status}`]
  if (emu.status === 200) {
    const er = (e as { resource?: Json }).resource
    const rr = (r as { resource?: Json }).resource
    return isDeepStrictEqual(er, rr) ? [] : [`resource: emulator ${JSON.stringify(er)}, real ${JSON.stringify(rr)}`]
  }
  const ee = firstError({ status: emu.status, body: e })
  const re = firstError({ status: real.status, body: r })
  const out: string[] = []
  for (const k of ['code', 'description', 'position', 'cause', 'failures']) {
    if (!isDeepStrictEqual(ee[k], re[k])) {
      out.push(`${k}: emulator ${JSON.stringify(ee[k])}, real ${JSON.stringify(re[k])}`)
    }
  }
  return out
}

/** Does the real answer satisfy the case's own expectation? */
function checkExpectation(c: Case, real: Answer): string[] {
  if (c.expect !== undefined) {
    if (real.status !== 200) return [`case expects ${JSON.stringify(c.expect)}, real errored ${JSON.stringify(real.body)}`]
    const got = normalise((real.body as { resource: Json }).resource)
    return isDeepStrictEqual(normalise(c.expect), got)
      ? []
      : [`case expects ${JSON.stringify(c.expect)}, real ${JSON.stringify(got)}`]
  }
  if (c.error) {
    if (real.status === 200) return [`case expects error ${JSON.stringify(c.error)}, real succeeded`]
    const got = firstError({ status: real.status, body: normalise(real.body) })
    const out: string[] = []
    for (const [k, want] of Object.entries(c.error)) {
      const have = k === 'status' ? real.status : got[k]
      if (!isDeepStrictEqual(k === 'status' ? want : normalise(want), have)) {
        out.push(`case expects ${k} ${JSON.stringify(want)}, real ${JSON.stringify(have)}`)
      }
    }
    return out
  }
  return []
}

// ---------------------------------------------------------------------------
// emulator side

function seedEmulator(suite: Suite, shift: number): Emulator {
  const emu = createEmulator()
  if (suite.now_micros !== undefined) emu.setNow(suite.now_micros + shift)
  else if (shift) emu.setNow(1_700_000_000_000_000 + shift)
  if (shift) emu.setNextId(BigInt(400_000_000_000_000_000 + shift))
  if (suite.schema) emu.loadSchema(suite.schema)
  if (shift && suite.now_micros === undefined) emu.setNow(null)
  return emu
}

async function runSetup(client: Client, setup: Json[], label: string): Promise<void> {
  for (const [i, s] of setup.entries()) {
    const a = await run(client, s)
    if (a.status !== 200) throw new Error(`${label}: setup[${i}] failed: ${JSON.stringify(a.body)}`)
  }
}

// ---------------------------------------------------------------------------
// real side

interface RealDb {
  client: Client
  dispose(): Promise<void>
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))

async function openRealDb(endpoint: string, secret: string, suiteName: string): Promise<RealDb> {
  const root = new faunadb.Client({ endpoint, secret, checkNewVersion: false })
  const name = `fql_emu_diff_${suiteName.replace(/[^a-z0-9_]/gi, '_')}_${Date.now().toString(36)}`
  let created = false
  try {
    await root.query(q.CreateDatabase({ name }))
    created = true
    const key = await root.query<{ secret: string }>(q.CreateKey({ database: q.Database(name), role: 'admin' }))
    const client = new faunadb.Client({ endpoint, secret: key.secret, checkNewVersion: false })
    // New keys can take a moment to become valid.
    for (let i = 0; ; i++) {
      try {
        await client.query(1)
        break
      } catch (e) {
        if (!(e instanceof faunadb.errors.Unauthorized) || i >= 20) throw e
        await sleep(250)
      }
    }
    return {
      client,
      dispose: async () => {
        await client.close()
        if (process.env.FQL_EMU_DIFF_KEEP === '1') console.log(`  kept database ${name}`)
        else await root.query(q.Delete(q.Database(name)))
        await root.close()
      },
    }
  } catch (e) {
    if (created) await root.query(q.Delete(q.Database(name))).catch(() => undefined)
    // Fall back to the secret's own database, which must be empty.
    const empty = await root
      .query<boolean>(
        q.And(q.IsEmpty(q.Collections()), q.IsEmpty(q.Indexes()), q.IsEmpty(q.Functions()), q.IsEmpty(q.Roles())),
      )
      .catch(() => false)
    if (!empty) {
      await root.close()
      throw new Error(
        `cannot create a child database (${e instanceof Error ? e.message : String(e)}) and the ` +
          'FAUNA_SECRET database is not empty; use an admin secret, or a secret for an empty database',
      )
    }
    return {
      client: root,
      dispose: async () => {
        for (const set of [q.Indexes(), q.Functions(), q.Roles(), q.Collections()]) {
          await root.query(q.Map(q.Paginate(set, { size: 100_000 }), q.Lambda('r', q.Delete(q.Var('r')))))
        }
        await root.close()
      },
    }
  }
}

// ---------------------------------------------------------------------------

interface Tally {
  passed: number
  skipped: number
  todo: number
  mismatches: number
}

async function runSuite(file: string, endpoint: string, secret: string, tally: Tally): Promise<void> {
  const label = basename(file)
  const suite = JSON.parse(readFileSync(file, 'utf8')) as Suite
  console.log(`\n${label}: ${suite.description ?? ''}`)

  const base = seedEmulator(suite, 0)
  const variant = seedEmulator(suite, 86_400_000_000 * 1000 + 1)
  await runSetup(base.client(), suite.setup ?? [], `${label} (emulator)`)
  await runSetup(variant.client(), suite.setup ?? [], `${label} (emulator variant)`)

  const real = await openRealDb(endpoint, secret, label.replace(/\.json$/, ''))
  try {
    await runSetup(real.client, [...schemaQueries(suite.schema), ...(suite.setup ?? [])], `${label} (real)`)
    for (const c of suite.cases ?? []) {
      const name = c.name ?? '<unnamed>'
      const code = c.error?.code
      if (typeof code === 'string' && EMULATOR_ONLY_CODES.has(code)) {
        tally.skipped++
        continue
      }
      const emuAnswer = await run(base.fork().client(), c.query)
      const variantAnswer = await run(variant.fork().client(), c.query)
      const realAnswer = await run(real.client, c.query)
      if (compare(emuAnswer, variantAnswer).length > 0) {
        // Pinned-clock or seeded-id answers cannot be reproduced on a real server.
        console.log(`  skip  ${name} (answer depends on the clock or id seed)`)
        tally.skipped++
        continue
      }

      const problems = [...compare(emuAnswer, realAnswer), ...checkExpectation(c, realAnswer)]

      if (problems.length === 0) {
        tally.passed++
      } else if (c.todo) {
        tally.todo++
        console.log(`  todo  ${name} (${c.todo})\n          ${problems.join('\n          ')}`)
      } else {
        tally.mismatches++
        console.log(`  FAIL  ${name}\n          ${problems.join('\n          ')}`)
      }
    }
  } finally {
    await real.dispose()
  }
}

async function main(): Promise<number> {
  const endpoint = process.env.FAUNA_ENDPOINT
  const secret = process.env.FAUNA_SECRET
  if (!endpoint || !secret) {
    console.log('differential: FAUNA_ENDPOINT and FAUNA_SECRET are not set; nothing to compare against. Skipping.')
    return 0
  }
  const dir = process.env.FQL_EMU_CASES_DIR ?? fileURLToPath(new URL('../../conformance/cases/', import.meta.url))
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

  const tally: Tally = { passed: 0, skipped: 0, todo: 0, mismatches: 0 }
  for (const f of files) {
    try {
      await runSuite(f, endpoint, secret, tally)
    } catch (e) {
      console.error(`differential: ${basename(f)}: ${e instanceof Error ? e.message : String(e)}`)
      return 2
    }
  }
  console.log(
    `\ndifferential: ${tally.passed} agree, ${tally.mismatches} mismatched, ${tally.todo} todo, ${tally.skipped} skipped`,
  )
  return tally.mismatches > 0 ? 1 : 0
}

main().then(
  (code) => process.exit(code),
  (e: unknown) => {
    console.error(e)
    process.exit(2)
  },
)
