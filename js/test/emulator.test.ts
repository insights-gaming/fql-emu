// The Emulator API: clock, ids, forks, schema, limits, clients.
import faunadb from 'faunadb'
import { describe, expect, it } from 'vitest'
import { createEmulator, Emulator, SchemaLoadError } from '../src/index.js'
import { errors, errorsOf, q, rejection, values } from './helpers.js'

const JAN_1_2024 = 1_704_067_200_000_000

describe('client()', () => {
  it('is a real faunadb.Client', () => {
    const client = createEmulator().client()
    expect(client).toBeInstanceOf(faunadb.Client)
    expect(typeof client.paginate).toBe('function')
  })

  it('ping answers like the server', async () => {
    expect(await createEmulator().client().ping()).toBe('Scope write is OK')
  })

  it('client-level and per-query observers see every request', async () => {
    const seen: Array<[string, number]> = []
    const client = createEmulator().client({ observer: (r) => seen.push(['client', r.statusCode]) })
    await client.query(1, { observer: (r) => seen.push(['query', r.statusCode]) })
    await rejection(client.query(q.Abort('x')))
    expect(seen).toEqual([
      ['client', 200],
      ['query', 200],
      ['client', 400],
    ])
  })

  it('observer receives the client and the parsed result', async () => {
    const client = createEmulator().client()
    let got: { value: unknown; client: unknown } | undefined
    await client.query(q.Collection('users'), {
      observer: (r, c) => (got = { value: (r.responseContent as { resource: unknown }).resource, client: c }),
    })
    expect(got?.client).toBe(client)
    expect(got?.value).toEqual(new values.Ref('users', values.Native.COLLECTIONS))
  })

  it('close() resolves and later queries fail with ClientClosed', async () => {
    const emu = createEmulator()
    const client = emu.client()
    await client.query(1)
    await expect(client.close()).resolves.toBeUndefined()
    await expect(client.query(1)).rejects.toBeInstanceOf(errors.ClientClosed)
    // Other clients of the same emulator are unaffected.
    expect(await emu.client().query(2)).toBe(2)
  })

  it('streaming is rejected explicitly', async () => {
    const client = createEmulator().client()
    const http = (client as unknown as { _http: { execute(o: object): Promise<unknown> } })._http
    await expect(http.execute({ method: 'POST', path: 'stream', body: '{}', streamConsumer: {} })).rejects.toThrow(
      /streaming is not emulated/,
    )
  })
})

describe('clock', () => {
  it('setNow pins Now() and the txn time; getLastTxnTime follows x-txn-time', async () => {
    const emu = createEmulator()
    emu.setNow(new Date('2024-01-01T00:00:00Z'))
    const client = emu.client()
    expect(client.getLastTxnTime()).toBeNull()
    expect(await client.query(q.Now())).toEqual(new values.FaunaTime('2024-01-01T00:00:00.000000000Z'))
    expect(client.getLastTxnTime()).toBe(JAN_1_2024)
    expect(await client.query(q.ToMicros(q.Now()))).toBe(JAN_1_2024)
    expect(client.getLastTxnTime()).toBe(JAN_1_2024)
  })

  it('accepts microseconds and the constructor option', async () => {
    expect(await createEmulator({ now: 1_000_000 }).query(q.Now())).toEqual(
      new values.FaunaTime('1970-01-01T00:00:01.000000000Z'),
    )
    const emu = createEmulator()
    emu.setNow(JAN_1_2024 + 1)
    expect(await emu.query(q.ToMicros(q.Now()))).toBe(JAN_1_2024 + 1)
  })

  it('failed queries still report a txn time', async () => {
    const emu = createEmulator({ now: JAN_1_2024 })
    const client = emu.client()
    await rejection(client.query(q.Abort('x')))
    expect(client.getLastTxnTime()).toBe(JAN_1_2024)
  })

  it('the auto clock advances 1µs per transaction; setNow(null) resumes after the pinned time', async () => {
    const emu = createEmulator()
    const client = emu.client()
    const a = await client.query<number>(q.ToMicros(q.Now()))
    const b = await client.query<number>(q.ToMicros(q.Now()))
    expect(b).toBe(a + 1)
    expect(client.getLastTxnTime()).toBe(b)

    emu.setNow(new Date('2030-01-01T00:00:00Z'))
    const pinned = await client.query<number>(q.ToMicros(q.Now()))
    emu.setNow(null)
    expect(await client.query(q.ToMicros(q.Now()))).toBe(pinned + 1)
    expect(await client.query(q.ToMicros(q.Now()))).toBe(pinned + 2)
  })

  it('syncLastTxnTime only moves forward', () => {
    const client = createEmulator().client()
    client.syncLastTxnTime(10)
    client.syncLastTxnTime(5)
    expect(client.getLastTxnTime()).toBe(10)
  })

  it('rejects non-integer microseconds', () => {
    expect(() => createEmulator().setNow(1.5)).toThrow(RangeError)
    expect(() => createEmulator().setNow(new Date('nope'))).toThrow(RangeError)
  })
})

describe('ids', () => {
  it('setNextId makes NewId() deterministic', async () => {
    const run = async () => {
      const emu = createEmulator({ nextId: 1000 })
      return [await emu.query(q.NewId()), await emu.query(q.NewId())]
    }
    expect(await run()).toEqual(['1000', '1001'])
    expect(await run()).toEqual(await run())
  })

  it('accepts bigints for snowflake-sized ids', async () => {
    const emu = createEmulator()
    emu.setNextId(300_000_000_000_000_123n)
    expect(await emu.query(q.NewId())).toBe('300000000000000123')
    expect(() => emu.setNextId(300_000_000_000_000_123)).toThrow(RangeError)
    expect(() => emu.setNextId(-1)).toThrow(RangeError)
  })

  it('forks continue the id sequence independently', async () => {
    const base = createEmulator({ nextId: 1 })
    const a = base.fork()
    const b = base.fork()
    expect(await a.query(q.NewId())).toBe('1')
    expect(await a.query(q.NewId())).toBe('2')
    expect(await b.query(q.NewId())).toBe('1')
    expect(await base.query(q.NewId())).toBe('1')
  })
})

describe('setIdentity', () => {
  it('controls CurrentIdentity()', async () => {
    const emu = createEmulator()
    const err = await rejection(emu.query(q.CurrentIdentity()))
    expect(err.message).toBe('missing identity')

    const me = new values.Ref('42', new values.Ref('users', values.Native.COLLECTIONS))
    emu.setIdentity(me)
    expect(await emu.query(q.CurrentIdentity())).toEqual(me)
    expect(await emu.query(q.HasCurrentIdentity())).toBe(true)

    emu.setIdentity(null)
    expect(await emu.query(q.HasCurrentIdentity())).toBe(false)
  })
})

describe('fork()', () => {
  it('is isolated in both directions', async () => {
    const base = createEmulator()
    await base.query(q.CreateCollection({ name: 'shared' }))
    const fork = base.fork()
    await fork.query(q.CreateCollection({ name: 'only_fork' }))
    await base.query(q.CreateCollection({ name: 'only_base' }))

    expect(await fork.query(q.Exists(q.Collection('shared')))).toBe(true)
    expect(await fork.query(q.Exists(q.Collection('only_base')))).toBe(false)
    expect(await base.query(q.Exists(q.Collection('only_fork')))).toBe(false)
    expect(base.stats().collections).toBe(2)
    expect(fork.stats().collections).toBe(2)
  })

  it("a fork's clients are independent of the base's clients", async () => {
    const base = createEmulator({ now: JAN_1_2024 })
    const baseClient = base.client()
    await baseClient.query(1)
    const fork = base.fork()
    fork.setNow(JAN_1_2024 + 5)
    const forkClient = fork.client()
    await forkClient.query(1)
    expect(baseClient.getLastTxnTime()).toBe(JAN_1_2024)
    expect(forkClient.getLastTxnTime()).toBe(JAN_1_2024 + 5)
    expect(await baseClient.query(q.ToMicros(q.Now()))).toBe(JAN_1_2024)
  })

  it('inherits the pinned clock and limits', async () => {
    const base = createEmulator({ now: JAN_1_2024, maxDocuments: 7 })
    const fork = base.fork()
    expect(await fork.query(q.ToMicros(q.Now()))).toBe(JAN_1_2024)
    expect(fork.stats().max_documents).toBe(7)
  })

  it('50 forks run concurrently without interfering', async () => {
    const base = createEmulator()
    base.loadSchema({ collections: [{ name: 'seed' }] })
    const results = await Promise.all(
      Array.from({ length: 50 }, async (_, i) => {
        const fork = base.fork()
        fork.setNow(JAN_1_2024 + i)
        fork.setNextId(i * 1000)
        const client = fork.client()
        await client.query(q.CreateCollection({ name: `c${i}` }))
        const [id, now, others] = await Promise.all([
          client.query<string>(q.NewId()),
          client.query<number>(q.ToMicros(q.Now())),
          client.query<boolean[]>([q.Exists(q.Collection(`c${(i + 1) % 50}`)), q.Exists(q.Collection('seed'))]),
        ])
        return { i, id, now, others, stats: fork.stats(), txn: client.getLastTxnTime() }
      }),
    )
    for (const r of results) {
      expect(r.id).toBe(String(r.i * 1000))
      expect(r.now).toBe(JAN_1_2024 + r.i)
      expect(r.txn).toBe(JAN_1_2024 + r.i)
      expect(r.others).toEqual([false, true])
      expect(r.stats.collections).toBe(2)
    }
    expect(base.stats().collections).toBe(1)
  })
})

describe('loadSchema', () => {
  // The example from docs/schema-bundle.md.
  const bundle = {
    collections: [{ name: 'users', history_days: 0 }],
    indexes: [
      {
        name: 'users_by_email',
        source: { '@ref': { id: 'users', collection: { '@ref': { id: 'collections' } } } },
        terms: [{ field: ['data', 'email'] }],
        unique: true,
      },
    ],
    functions: [
      {
        name: 'greet_1a2b3c',
        body: { '@query': { lambda: 'name', expr: { concat: ['hi ', { var: 'name' }] } } },
      },
    ],
  }

  it('loads the documented example', async () => {
    const emu = createEmulator()
    emu.loadSchema(bundle)
    expect(emu.stats()).toMatchObject({ collections: 1, indexes: 1, functions: 1, documents: 0 })
    expect(await emu.query(q.Call(q.Function('greet_1a2b3c'), 'ada'))).toBe('hi ada')
    const users = await emu.query<{ name: string; history_days: number }>(q.Get(q.Collection('users')))
    expect(users).toMatchObject({ name: 'users', history_days: 0 })
    const idx = await emu.query<{ name: string; unique: boolean; source: unknown }>(q.Get(q.Index('users_by_email')))
    expect(idx).toMatchObject({ name: 'users_by_email', unique: true })
    expect(idx.source).toEqual(new values.Ref('users', values.Native.COLLECTIONS))
  })

  it('accepts driver values and JSON strings', async () => {
    const emu = createEmulator()
    emu.loadSchema(JSON.stringify({ collections: [{ name: 'a' }] }))
    emu.loadSchema({
      indexes: [{ name: 'a_all', source: new values.Ref('a', values.Native.COLLECTIONS) }],
      functions: [{ name: 'q', body: new values.Query({ lambda: 'x', expr: { var: 'x' } }) }],
    })
    expect(await emu.query(q.Call('q', 5))).toBe(5)
    expect(emu.stats()).toMatchObject({ collections: 1, indexes: 1, functions: 1 })
  })

  it('throws a descriptive SchemaLoadError', () => {
    const emu = createEmulator()
    emu.loadSchema({ collections: [{ name: 'users' }] })
    let err: unknown
    try {
      emu.loadSchema({ collections: [{ name: 'users' }] })
    } catch (e) {
      err = e
    }
    expect(err).toBeInstanceOf(SchemaLoadError)
    expect(err).toBeInstanceOf(Error)
    const e = err as SchemaLoadError
    expect(e.errors[0].code).toBe('instance already exists')
    expect(e.message).toMatch(/loadSchema failed/)
    expect(e.message).toMatch(/instance already exists/)
  })

  it('reports invalid JSON', () => {
    expect(() => createEmulator().loadSchema('{')).toThrow(SchemaLoadError)
  })
})

describe('limits and stats', () => {
  it('maxDocuments is applied and reported', () => {
    expect(createEmulator().stats().max_documents).toBe(5000)
    const emu = createEmulator({ maxDocuments: 2 })
    expect(emu.stats()).toEqual({ collections: 0, indexes: 0, functions: 0, documents: 0, max_documents: 2 })
    expect(() => emu.setMaxDocuments(-1)).toThrow(RangeError)
  })

  // `Create` is being implemented in parallel; this activates once it lands.
  const createImplemented = Emulator.supportedForms().some((f) => f.name === 'create' && f.implemented)
  it.skipIf(!createImplemented)('exceeding maxDocuments fails with `emulator limit exceeded`', async () => {
    const emu = createEmulator({ maxDocuments: 2 })
    emu.loadSchema({ collections: [{ name: 'users' }] })
    await emu.query(q.Create(q.Collection('users'), { data: {} }))
    await emu.query(q.Create(q.Collection('users'), { data: {} }))
    const e = await rejection(emu.query(q.Create(q.Collection('users'), { data: {} })))
    expect(errorsOf(e)[0].code).toBe('emulator limit exceeded')
    expect(emu.stats().documents).toBe(2)
  })
})

describe('free()', () => {
  it('releases a fork without affecting the base', async () => {
    const base = createEmulator()
    base.fork().free()
    expect(await base.query(1)).toBe(1)
  })
})
