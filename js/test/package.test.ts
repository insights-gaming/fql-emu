// The built package (dist/) loads from both module systems. Runs after
// `npm run build`; skipped when dist/ has not been built.
import { existsSync } from 'node:fs'
import { createRequire } from 'node:module'
import faunadb from 'faunadb'
import { describe, expect, it } from 'vitest'

const dist = new URL('../dist/', import.meta.url)
const built = existsSync(new URL('index.cjs', dist)) && existsSync(new URL('index.js', dist))

type Pkg = typeof import('../src/index.js')

describe.skipIf(!built)('dist', () => {
  it('CommonJS build', async () => {
    const pkg = createRequire(import.meta.url)('../dist/index.cjs') as Pkg
    const client = pkg.createEmulator({ now: 1 }).client()
    expect(client).toBeInstanceOf(faunadb.Client)
    expect(await client.query(faunadb.query.Add(1, 2))).toBe(3)
    expect(client.getLastTxnTime()).toBe(1)
  })

  it('ES module build', async () => {
    const pkg = (await import(new URL('index.js', dist).href)) as Pkg
    expect(await pkg.createEmulator().query(faunadb.query.Concat(['a', 'b']))).toBe('ab')
    expect(pkg.Emulator.supportedForms().length).toBeGreaterThan(0)
  })

  it('ships type declarations for both', () => {
    expect(existsSync(new URL('index.d.ts', dist))).toBe(true)
    expect(existsSync(new URL('index.d.cts', dist))).toBe(true)
  })
})
