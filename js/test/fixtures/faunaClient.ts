// Stand-in for the app's module: a real client pointed at a real endpoint.
// Tests replace it with `vi.mock`; if they did not, queries would try the network.
import faunadb from 'faunadb'

export const faunaClient = new faunadb.Client({ secret: 'real-secret', domain: 'fauna.invalid', checkNewVersion: false })
