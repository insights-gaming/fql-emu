import faunadb from 'faunadb'
import { expect } from 'vitest'

export const { query: q, values, errors } = faunadb

/** Await a rejected query and return the driver error. */
export async function rejection(p: Promise<unknown>): Promise<faunadb.errors.FaunaHTTPError> {
  try {
    await p
  } catch (e) {
    expect(e).toBeInstanceOf(errors.FaunaHTTPError)
    return e as faunadb.errors.FaunaHTTPError
  }
  throw new Error('expected the query to fail')
}

export type ErrorJson = { code: string; description: string; position: Array<string | number>; cause?: ErrorJson[] }

/** `e.errors()` typed the way the app reads it. */
export function errorsOf(e: faunadb.errors.FaunaHTTPError): ErrorJson[] {
  return e.errors() as ErrorJson[]
}

/** `.value` of a driver value class (present at runtime, missing from the typings). */
export function rawValue(v: unknown): unknown {
  return (v as { value: unknown }).value
}

/** `new values.SetRef(...)` with the object argument the typings leave out. */
export function setRef(expr: object): InstanceType<typeof values.SetRef> {
  return new values.SetRef(expr as unknown as string)
}
