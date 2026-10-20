// The forms implemented today, driven through the real driver builders.
import { beforeEach, describe, expect, it } from 'vitest'
import { createEmulator, Emulator } from '../src/index.js'
import { errorsOf, q, rejection, values } from './helpers.js'

let emu: Emulator
beforeEach(() => {
  emu = createEmulator({ now: new Date('2024-01-01T00:00:00Z') })
})

describe('basic forms', () => {
  it('Let / Var (sequential bindings)', async () => {
    expect(await emu.query(q.Let({ a: 1, b: q.Add(q.Var('a'), 1) }, q.Var('b')))).toBe(2)
    expect(await emu.query(q.Let([{ a: 'x' }, { b: q.Concat([q.Var('a'), 'y']) }], q.Var('b')))).toBe('xy')
  })

  it('Select with paths and defaults', async () => {
    const obj = { a: [{ b: 7 }] }
    expect(await emu.query(q.Select(['a', 0, 'b'], obj))).toBe(7)
    expect(await emu.query(q.Select('a', { a: 1 }))).toBe(1)
    expect(await emu.query(q.Select(['x'], {}, 'd'))).toBe('d')
    expect(await emu.query(q.Select(['x'], {}, null))).toBe(null)
  })

  it('If', async () => {
    expect(await emu.query(q.If(true, 'yes', 'no'))).toBe('yes')
    expect(await emu.query(q.If(q.Equals(1, 2), 'yes', 'no'))).toBe('no')
  })

  it('Do returns the last value', async () => {
    expect(await emu.query(q.Do(1, 2, 3))).toBe(3)
  })

  it('Lambda applied through a UDF', async () => {
    await emu.query(q.CreateFunction({ name: 'double', body: q.Query(q.Lambda('x', q.Add(q.Var('x'), q.Var('x')))) }))
    expect(await emu.query(q.Call(q.Function('double'), 21))).toBe(42)
    expect(await emu.query(q.Call('double', 2))).toBe(4)
  })

  it('Call with multiple arguments binds an array pattern', async () => {
    await emu.query(
      q.CreateFunction({ name: 'join', body: q.Query(q.Lambda(['a', 'b'], q.Concat([q.Var('a'), q.Var('b')], '-'))) }),
    )
    expect(await emu.query(q.Call(q.Function('join'), 'x', 'y'))).toBe('x-y')
  })

  it('Concat', async () => {
    expect(await emu.query(q.Concat(['a', 'b']))).toBe('ab')
    expect(await emu.query(q.Concat(['a', 'b'], ', '))).toBe('a, b')
  })

  it('Add promotes to double', async () => {
    expect(await emu.query(q.Add(1, 2, 3))).toBe(6)
    expect(await emu.query(q.Add(1, 1.5))).toBe(2.5)
  })

  it('Equals', async () => {
    expect(await emu.query(q.Equals(1, 1.0))).toBe(true)
    expect(await emu.query(q.Equals('a', 'a', 'a'))).toBe(true)
    expect(await emu.query(q.Equals({ a: 1 }, { a: 2 }))).toBe(false)
  })

  it('Now and Time', async () => {
    expect(await emu.query(q.Now())).toEqual(new values.FaunaTime('2024-01-01T00:00:00.000000000Z'))
    expect(await emu.query(q.Time('now'))).toEqual(new values.FaunaTime('2024-01-01T00:00:00.000000000Z'))
    expect(await emu.query(q.Time('2020-01-01T10:00:00.5Z'))).toEqual(
      new values.FaunaTime('2020-01-01T10:00:00.500000000Z'),
    )
    expect(await emu.query(q.ToMicros(q.Now()))).toBe(1_704_067_200_000_000)
  })

  it('Collection / CreateCollection / Get / Exists', async () => {
    expect(await emu.query(q.Exists(q.Collection('users')))).toBe(false)
    const created = await emu.query<{ ref: unknown; name: string; ts: number; history_days: number }>(
      q.CreateCollection({ name: 'users' }),
    )
    expect(created).toEqual({
      ref: new values.Ref('users', values.Native.COLLECTIONS),
      ts: 1_704_067_200_000_000,
      history_days: 30,
      name: 'users',
    })
    expect(await emu.query(q.Get(q.Collection('users')))).toEqual(created)
    expect(await emu.query(q.Exists(q.Collection('users')))).toBe(true)
    expect(await emu.query(q.Exists(q.Ref(q.Collection('users'), '1')))).toBe(false)
  })

  it('CreateFunction / Get returns the body as a Query', async () => {
    await emu.query(q.CreateFunction({ name: 'id', body: q.Query(q.Lambda('x', q.Var('x'))) }))
    const fn = await emu.query<{ ref: unknown; body: unknown; name: string }>(q.Get(q.Function('id')))
    expect(fn.ref).toEqual(new values.Ref('id', values.Native.FUNCTIONS))
    expect(fn.name).toBe('id')
    expect(fn.body).toEqual(new values.Query({ lambda: 'x', expr: { var: 'x' }, api_version: '4' }))
  })

  it('writes in a failed query are rolled back', async () => {
    await rejection(emu.query(q.Do(q.CreateCollection({ name: 'a' }), q.Abort('no'))))
    expect(await emu.query(q.Exists(q.Collection('a')))).toBe(false)
  })
})

describe('forms that are not emulated yet', () => {
  it('fail loudly with a `not emulated` error', async () => {
    // CreateKey is deliberately never emulated (keys/auth are out of scope).
    const e = await rejection(emu.query(q.CreateKey({ role: 'admin' })))
    expect(e.message).toBe('not emulated')
    const [err] = errorsOf(e)
    expect(err.code).toBe('not emulated')
    expect(err.description).toMatch(/does not implement the `create_key` function/)
    expect(err.position).toEqual([])
  })

  it('every stub is listed by supportedForms()', () => {
    const forms = Emulator.supportedForms()
    expect(forms).toContainEqual({ name: 'create_key', implemented: false })
    expect(forms).toContainEqual({ name: 'let', implemented: true })
    for (const f of forms) expect(f.name).toMatch(/^[a-z_]+$/)
  })
})
