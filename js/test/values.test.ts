// Every value type survives driver serialisation -> emulator -> driver parseJSON.
import { describe, expect, it } from 'vitest'
import { createEmulator } from '../src/index.js'
import { errors, q, rawValue, setRef, values } from './helpers.js'

const emu = createEmulator()
const usersRef = new values.Ref('users', values.Native.COLLECTIONS)

describe('value round-trip', () => {
  it('scalars and arrays', async () => {
    expect(await emu.query(1)).toBe(1)
    expect(await emu.query(1.5)).toBe(1.5)
    expect(await emu.query('s')).toBe('s')
    expect(await emu.query(true)).toBe(true)
    expect(await emu.query(null as unknown as string)).toBe(null)
    expect(await emu.query([1, 'a', null, [2]])).toEqual([1, 'a', null, [2]])
  })

  it('plain objects (the driver wraps them in {object: ...})', async () => {
    expect(await emu.query({ a: 1, b: { c: [true] } })).toEqual({ a: 1, b: { c: [true] } })
  })

  it('values.Ref with a collection', async () => {
    const ref = new values.Ref('123', usersRef)
    const out = await emu.query<InstanceType<typeof values.Ref>>(ref)
    expect(out).toBeInstanceOf(values.Ref)
    expect(out.id).toBe('123')
    expect(out.collection).toBeInstanceOf(values.Ref)
    expect(out.collection?.id).toBe('users')
    expect(out.collection?.collection?.id).toBe('collections')
    expect(out).toEqual(ref)
    expect(await emu.query(q.Ref(q.Collection('users'), '123'))).toEqual(ref)
  })

  it('native and schema refs', async () => {
    expect(await emu.query(q.Collections())).toEqual(values.Native.COLLECTIONS)
    expect(await emu.query(q.Collection('users'))).toEqual(usersRef)
    expect(await emu.query(q.Index('by_x'))).toEqual(new values.Ref('by_x', values.Native.INDEXES))
    expect(await emu.query(q.Function('f'))).toEqual(new values.Ref('f', values.Native.FUNCTIONS))
  })

  it('FaunaTime keeps nanoseconds', async () => {
    const t = new values.FaunaTime('2024-01-01T00:00:00.123456789Z')
    const out = await emu.query<InstanceType<typeof values.FaunaTime>>(t)
    expect(out).toBeInstanceOf(values.FaunaTime)
    expect(rawValue(out)).toBe('2024-01-01T00:00:00.123456789Z')
    expect(out.date.toISOString()).toBe('2024-01-01T00:00:00.123Z')
  })

  it('FaunaDate', async () => {
    const out = await emu.query<InstanceType<typeof values.FaunaDate>>(new values.FaunaDate('2024-02-29'))
    expect(out).toBeInstanceOf(values.FaunaDate)
    expect(rawValue(out)).toBe('2024-02-29')
    expect(await emu.query(q.Date('2024-01-02'))).toEqual(new values.FaunaDate('2024-01-02'))
  })

  it('SetRef', async () => {
    const set = setRef({ match: new values.Ref('by_x', values.Native.INDEXES), terms: ['a'] })
    const out = await emu.query(set)
    expect(out).toBeInstanceOf(values.SetRef)
    expect(out).toEqual(set)
    const docs = setRef({ documents: usersRef })
    expect(await emu.query(docs)).toEqual(docs)
  })

  it('Bytes', async () => {
    const out = await emu.query<InstanceType<typeof values.Bytes>>(new values.Bytes(new Uint8Array([1, 2, 3])))
    expect(out).toBeInstanceOf(values.Bytes)
    expect(Array.from(rawValue(out) as Uint8Array)).toEqual([1, 2, 3])
    expect(JSON.stringify(out)).toBe('{"@bytes":"AQID"}')
    expect(await emu.query(q.Bytes('AQID'))).toEqual(new values.Bytes('AQID'))
  })

  it('Query (lambdas render as @query)', async () => {
    const out = await emu.query(q.Query(q.Lambda('x', q.Add(q.Var('x'), 1))))
    expect(out).toBeInstanceOf(values.Query)
    expect(out).toEqual(new values.Query({ lambda: 'x', expr: { add: [{ var: 'x' }, 1] }, api_version: '4' }))
    // A bare lambda is not a form on the server: in a Let binding it is
    // "No form/function found" (only Map/Filter/... parse inline lambdas).
    await expect(emu.query(q.Let({ f: q.Lambda('y', q.Var('y')) }, q.Var('f')))).rejects.toBeInstanceOf(errors.BadRequest)
  })

  it('@obj-escaped objects', async () => {
    // Keys starting with `@` force the server to answer {"@obj": {...}};
    // parseJSON unwraps it. (A key that is itself a driver escape, e.g.
    // `@ref`, cannot round-trip: the driver's reviver runs inner-first.)
    const out = await emu.query({ '@foo': 1, plain: 'x' })
    expect(out).toEqual({ '@foo': 1, plain: 'x' })
  })

  it('nested special values', async () => {
    const out = await emu.query({
      when: new values.FaunaTime('2024-01-01T00:00:00Z'),
      list: [new values.Ref('1', usersRef), new values.FaunaDate('2024-01-01')],
    })
    expect(out).toEqual({
      when: new values.FaunaTime('2024-01-01T00:00:00.000000000Z'),
      list: [new values.Ref('1', usersRef), new values.FaunaDate('2024-01-01')],
    })
  })
})
