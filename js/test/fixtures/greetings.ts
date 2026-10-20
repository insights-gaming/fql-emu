// Stand-in for app code that imports the shared client.
import faunadb from 'faunadb'
import { faunaClient } from './faunaClient.js'

const q = faunadb.query

export async function greet(name: string): Promise<string> {
  return faunaClient.query<string>(q.Call(q.Function('greet'), name))
}

export async function ensureCollection(name: string): Promise<boolean> {
  return faunaClient.query<boolean>(
    q.If(q.Exists(q.Collection(name)), false, q.Do(q.CreateCollection({ name }), true)),
  )
}
