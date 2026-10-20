// The documented `vi.mock` pattern: swap the app's `faunaClient` module for
// the emulator, with a fresh fork per test.
import faunadb from 'faunadb'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { Emulator } from '../src/index.js'
import { mockFaunaClient } from '../src/index.js'

const state = vi.hoisted(() => ({ emu: undefined as unknown as Emulator }))
vi.mock('./fixtures/faunaClient.js', async () => {
  const { mockFaunaClient } = await import('../src/index.js')
  return mockFaunaClient(() => state.emu)
})

const { createEmulator } = await import('../src/index.js')
const { faunaClient } = await import('./fixtures/faunaClient.js')
const { ensureCollection, greet } = await import('./fixtures/greetings.js')

const base = createEmulator()
base.loadSchema({
  functions: [{ name: 'greet', body: { '@query': { lambda: 'n', expr: { concat: ['hi ', { var: 'n' }] } } } }],
})

beforeEach(() => {
  state.emu = base.fork()
})

describe('vi.mock(faunaClient)', () => {
  it('app code runs against the emulator', async () => {
    expect(await greet('ada')).toBe('hi ada')
    expect(faunaClient).toBeInstanceOf(faunadb.Client)
  })

  it('writes in one test are visible within it ...', async () => {
    expect(await ensureCollection('users')).toBe(true)
    expect(await ensureCollection('users')).toBe(false)
  })

  it('... and gone in the next (fresh fork)', async () => {
    expect(await ensureCollection('users')).toBe(true)
  })

  it('the module also exposes a query shortcut', () => {
    const mod = mockFaunaClient(base)
    expect(mod.faunaClient).toBeInstanceOf(faunadb.Client)
    return expect(mod.query(faunadb.query.Add(1, 1))).resolves.toBe(2)
  })
})
