// Emulator errors surface as the driver's own error classes, with the
// response body the app inspects.
import faunadb from 'faunadb'
import { describe, expect, it } from 'vitest'
import { createEmulator } from '../src/index.js'
import { errors, errorsOf, q, rejection } from './helpers.js'

const emu = createEmulator()
emu.loadSchema({
  collections: [{ name: 'users' }],
  functions: [{ name: 'fail', body: { '@query': { lambda: 'msg', expr: { abort: { var: 'msg' } } } } }],
})

describe('error mapping', () => {
  it('invalid argument -> BadRequest with the position intact', async () => {
    const e = await rejection(emu.query(q.Add(1, 'two')))
    expect(e).toBeInstanceOf(errors.BadRequest)
    expect(e.name).toBe('BadRequest')
    expect(e.message).toBe('invalid argument')
    expect(e.description).toBe('Number expected, String provided.')
    expect(errorsOf(e)[0].position).toEqual(['add', 1])
    expect(e.requestResult.statusCode).toBe(400)
  })

  it('nested positions follow the server (form path ++ argument key)', async () => {
    const e = await rejection(emu.query(q.Let({ x: q.If(1, 'a', 'b') }, q.Var('x'))))
    expect(errorsOf(e)[0]).toMatchObject({ code: 'invalid argument', position: ['let', 0, 'x', 'if'] })
  })

  it('instance not found -> NotFound', async () => {
    const e = await rejection(emu.query(q.Get(q.Ref(q.Collection('users'), '1'))))
    expect(e).toBeInstanceOf(errors.NotFound)
    expect(e.message).toBe('instance not found')
    expect(e.description).toBe('Document not found.')
    expect(errorsOf(e)[0].position).toEqual([])
    expect(e.requestResult.statusCode).toBe(404)
  })

  it('value not found -> NotFound', async () => {
    const e = await rejection(emu.query(q.Select(['a', 'b'], { a: 1 })))
    expect(e).toBeInstanceOf(errors.NotFound)
    expect(errorsOf(e)[0]).toEqual({ code: 'value not found', description: 'Value not found at path [a,b].', position: ['from'] })
  })

  it('call error carries the cause', async () => {
    const e = await rejection(emu.query(q.Call(q.Function('fail'), 'boom')))
    expect(e).toBeInstanceOf(errors.BadRequest)
    expect(e.message).toBe('call error')
    expect(e.description).toBe('Calling the function resulted in an error.')
    const [err] = errorsOf(e)
    expect(err.cause).toEqual([{ code: 'transaction aborted', description: 'boom', position: ['expr'] }])
    // The pattern the app uses for unique violations inside UDFs.
    expect(errorsOf(e)[0].cause?.[0].code).toBe('transaction aborted')
  })

  it('transaction aborted', async () => {
    const e = await rejection(emu.query(q.Abort('stop right there')))
    expect(e).toBeInstanceOf(errors.BadRequest)
    expect(e.message).toBe('transaction aborted')
    expect(e.description).toBe('stop right there')
  })

  it('invalid ref for an unknown function', async () => {
    const e = await rejection(emu.query(q.Call(q.Function('missing'), 1)))
    expect(errorsOf(e)[0]).toEqual({
      code: 'invalid ref',
      description: "Ref refers to undefined function 'missing'",
      position: ['call'],
    })
  })

  it('instance already exists', async () => {
    const e = await rejection(emu.query(q.CreateCollection({ name: 'users' })))
    expect(e).toBeInstanceOf(errors.BadRequest)
    expect(e.message).toBe('instance already exists')
  })

  it('requestResult.responseContent is the parsed body', async () => {
    const e = await rejection(emu.query(q.Abort('x')))
    const rr = e.requestResult
    expect(rr.responseContent).toEqual({ errors: [{ code: 'transaction aborted', description: 'x', position: [] }] })
    expect(rr.responseContent.errors[0].code).toBe('transaction aborted')
    expect(JSON.parse(rr.responseRaw)).toEqual(rr.responseContent)
    expect(rr.method).toBe('POST')
    expect(JSON.parse(rr.requestRaw)).toEqual({ abort: 'x' })
    expect((rr.responseHeaders as Record<string, string>)['x-txn-time']).toMatch(/^\d+$/)
  })

  it('successful requestResult carries the resource', async () => {
    let seen: faunadb.RequestResult | undefined
    await emu.query(q.Add(1, 2), { observer: (r) => (seen = r) })
    expect(seen?.statusCode).toBe(200)
    expect(seen?.responseContent).toEqual({ resource: 3 })
  })

  it('malformed bodies are a 400, not a crash', async () => {
    const client = emu.client()
    const http = (client as unknown as { _http: { execute(o: object): Promise<{ status: number; body: string }> } })._http
    const res = await http.execute({ method: 'POST', path: '', body: '{' })
    expect(res.status).toBe(400)
    expect(JSON.parse(res.body).errors[0].code).toBe('invalid expression')
  })
})
