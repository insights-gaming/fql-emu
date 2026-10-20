# @insights-gaming/fql-emu

Run `faunadb` (FQL v4) queries against an in-process emulator instead of a
`fauna/faunadb` container. The emulator is Rust compiled to wasm; this package
puts it underneath a **real** `faunadb.Client` by swapping the client's HTTP
layer. The driver's own `q.*` builders, serialisation, `parseJSON`, `values.*`
classes, observers and error classes all run unchanged.

> **This is not a database. Do not benchmark it.**
>
> Indexes are brute-force scans, every query clones the whole store, and there
> is a hard cap on documents (default 5 000; exceeding it fails with
> `emulator limit exceeded`). It exists so query *logic* can be tested quickly,
> deterministically and in parallel. E2E runs stay on the real container.

## Install

Published to GitHub Packages. Add to `.npmrc` (token needs `read:packages`):

```ini
@insights-gaming:registry=https://npm.pkg.github.com
//npm.pkg.github.com/:_authToken=${GITHUB_TOKEN}
```

```sh
npm install --save-dev @insights-gaming/fql-emu faunadb
```

Releases are published by the `publish` workflow when a `v*` tag is pushed
(or run it manually); `prepack` builds `dist/` and the committed `wasm/` is
shipped as-is.

## Usage

```ts
import faunadb from 'faunadb'
import { createEmulator } from '@insights-gaming/fql-emu'

const q = faunadb.query

const emu = createEmulator({ now: new Date('2024-01-01T00:00:00Z') })
emu.loadSchema(schemaBundle) // see docs/schema-bundle.md

const client = emu.client()  // instanceof faunadb.Client
await client.query(q.CreateCollection({ name: 'users' }))
await emu.query(q.Exists(q.Collection('users'))) // shortcut on an internal client
```

API:

| | |
|---|---|
| `createEmulator({ maxDocuments?, now?, nextId? })` | a fresh, empty, writable database |
| `emu.client(opts?)` | a new `faunadb.Client` on this emulator. `opts` go to the `Client` constructor (`observer`, `queryTimeout`, ...); `secret` and connection options are ignored |
| `emu.query(expr, options?)` | `client().query` on an internal client |
| `emu.fork(mode?)` | an independent copy (documents, schema, clock, id seed, limits); `mode` is `'read-write'` or `'read-only'` and defaults to the parent's (see below) |
| `emu.mode` | the mode the emulator was forked with |
| `emu.loadSchema(bundle)` | load collections / roles / functions / indexes; throws `SchemaLoadError` (with `.errors`) listing the Fauna errors |
| `emu.setNow(date \| micros \| null)` | pin `Now()` and every written `ts`; `null` returns to the auto clock |
| `emu.setNextId(n \| bigint)` | seed for `NewId()` (use a `bigint` above 2^53, e.g. snowflake-sized ids) |
| `emu.setIdentity(ref \| null)` | what `CurrentIdentity()` returns |
| `emu.setMaxDocuments(n)` | change the document cap |
| `emu.stats()` | `{ collections, indexes, functions, documents, max_documents, mode }` |
| `emu.free()` | release the wasm memory now (otherwise it is freed on GC) |
| `Emulator.supportedForms()` | `[{ name, implemented }]` for every recognised form |
| `mockFaunaClient(emu \| () => emu)` | a `{ faunaClient, query }` module for `vi.mock` |

Clients behave like the driver's: `getLastTxnTime()` follows the `x-txn-time`
header (the transaction time in µs), `close()` makes later queries reject with
`errors.ClientClosed`, `ping()` answers `"Scope write is OK"`. Streaming
(`client.stream`) is rejected.

## Replacing the app's `faunaClient` in vitest

If your code imports `{ faunaClient }` from a module, mock that module.
Passing a getter lets every test get a fresh fork without re-mocking:

```ts
import { beforeEach, vi } from 'vitest'
import type { Emulator } from '@insights-gaming/fql-emu'

const state = vi.hoisted(() => ({ emu: undefined as unknown as Emulator }))
vi.mock('./faunaClient', async () => {
  const { mockFaunaClient } = await import('@insights-gaming/fql-emu')
  return mockFaunaClient(() => state.emu)
})

const { createEmulator } = await import('@insights-gaming/fql-emu')
const base = createEmulator({ now: new Date('2024-01-01T00:00:00Z') })
base.loadSchema(schemaBundle)

beforeEach(() => {
  state.emu = base.fork() // each test starts from the seeded state
})
```

`vi.mock` factories are hoisted above imports, so anything they use must come
from `vi.hoisted` or a dynamic `import()`, as above. With a single emulator,
`vi.mock('./faunaClient', () => mockFaunaClient(emu))` works as long as `emu`
comes from `vi.hoisted`. `test/vimock.test.ts` is a working example.

## Modes and read-only forks

A fresh emulator is always writable. The mode is chosen when forking and
cannot be changed afterwards:

- `fork('read-write')` (the default for a writable parent): a server key.
- `fork('read-only')`: a `server-readonly` key. Every write (`Create`,
  `Update`, `Delete`, `CreateCollection`, ...), every `Call`, and listing
  `Roles()` / `Keys()` / `Databases()` is rejected with Fauna's own
  `permission denied` error (HTTP 403, `errors.PermissionDenied` in the
  driver) at the position the real server reports, after the same argument
  checks. Reads, `NewId()` and `Now()` keep working, and lazy branches are
  untouched. `loadSchema` is refused too: load the schema into the base
  before forking. The exact rules and their sources are in
  `docs/fql-v4-spec.md` §10.

`fork()` without a mode inherits the parent's, so a read-only fork's forks
are read-only unless asked otherwise (`ro.fork('read-write')`).

Two reasons to use a read-only fork:

- The code under test runs with a read-only key in production, and you want
  the same failures in tests.
- A suite that only reads can share **one** fork instead of forking per
  test: nothing can change a read-only instance, so there is nothing to
  isolate, and the per-test copy is skipped entirely.

```ts
const base = createEmulator()
base.loadSchema(bundle)
await base.query(q.Create(q.Collection('users'), { data: { name: 'ada' } }))
const shared = base.fork('read-only')

// Every test can use shared.client() directly ...
await expect(shared.query(q.Create(q.Collection('users'), {}))).rejects.toBeInstanceOf(errors.PermissionDenied)
// ... and a test that needs to write takes its own writable copy.
const writable = base.fork()
```

## Determinism

- **Clock.** `setNow(...)` (or `createEmulator({ now })`) pins `Now()`,
  `Time("now")`, every written `ts` and the txn time. Without it, an auto clock
  starts at 2020-09-13T12:26:40Z and advances 1 µs per query; it never reads
  the wall clock. `setNow(null)` resumes the auto clock after the latest time
  seen.
- **Ids.** `setNextId(n)` (or `{ nextId }`) seeds `NewId()` and generated
  document ids; they count up from there.
- **Transactions.** Each query is atomic: if it errors, none of its writes
  are kept.
- **Forks.** `fork()` copies everything, including the clock and the id seed,
  so forks taken from one seeded base answer identically. Forks and their
  clients share nothing, so `Promise.all` over many forks is safe. Queries run
  synchronously inside `client.query`, so they apply in call order.

## The `not emulated` contract

Anything the emulator does not implement fails loudly instead of guessing.
The driver throws `errors.BadRequest` with `message === 'not emulated'` and a
description naming the function, for example:

```json
{ "code": "not emulated", "description": "The FQL emulator does not implement the `create_key` function. Run this test against the fauna/faunadb container instead.", "position": [] }
```

`not emulated` and `emulator limit exceeded` are not Fauna error codes, so
code that handles real Fauna errors cannot accidentally pass because of them.
Out of scope by design: temporal reads (`At`, `ts`, `Events`), child
databases, keys/tokens/`Login`, role/ABAC enforcement (every query runs as
admin, or as a server-readonly key in read-only mode), TTL expiry, streaming. `Emulator.supportedForms()` lists what is
implemented right now.

## Differential testing against real Fauna

```sh
FAUNA_ENDPOINT=http://localhost:8443 FAUNA_SECRET=secret npm run differential [-- basic]
```

This replays every `conformance/cases/*.json` suite against a real endpoint
and against the emulator, classifies every case, prints per-suite counts and
the unclassified mismatches, and (for a full run) writes
`conformance/DIFFERENTIAL.md`. It exits 1 only on unclassified mismatches,
and 0 with a notice when the env vars are unset. It is not part of `npm test`.
Node 18+; the whole corpus takes about 10 s against a local container.

- **Isolation.** Like the Rust harness, every case sees a pristine copy of
  the suite's state: it runs in a fresh child database seeded with `schema`,
  `documents` and `setup` (pure, read-only cases share a seeded database).
  The secret must be able to create databases (an admin key, or the
  container's root key `secret`).
- **Fixtures.** `documents` are replayed with `Create(Ref(Collection(c), id), ...)`,
  one transaction per distinct `ts`, so relative timestamps survive but
  absolute ones do not.
- **Numbers.** Case files and both answers are handled as lossless JSON
  (`src/ljson.ts`): `9223372036854775807` stays exact and `1` != `1.0`.
- **Normalisation.** `ts` fields, `@ts` values and snowflake ids are
  masked. Cases whose emulator answer changes when the clock, the id seed or
  fixture timestamps move are skipped with that reason.

Each case ends up as one of:

| | |
|---|---|
| agree | emulator, server and the case expectation agree |
| todo | the case has a `todo` (an emulator bug; by convention `"emulator: <reason>"`), the expectation matches the server and only the emulator differs |
| skipped | expects `not emulated` / `emulator limit exceeded`, depends on the clock, ids or fixture timestamps, or has `"skip_differential": "<reason>"` |
| mismatch | anything else, **including an expectation the server disagrees with**: fix the expectation (edit the JSON textually so `2.0` stays `2.0`), and mark the case `todo` if the emulator then fails it |

`--probe <suite> '<query>' ...` runs raw wire-format queries in one database
seeded like `<suite>`, on both sides, and prints both answers; useful when
classifying a mismatch. The header of `src/differential.ts` lists the other
environment knobs.

## Building

The wasm loader in `wasm/` is a build output (gitignored), produced from
`crates/fql-emu-wasm`:

```sh
bash ../scripts/build-wasm.sh          # release build -> js/wasm/ (uses nix if wasm-pack is missing)
bash ../scripts/build-wasm.sh --dev    # faster, unoptimised
npm install
npm run build      # dist/index.js (ESM), dist/index.cjs (CJS), .d.ts / .d.cts
npm test           # vitest against the wasm in js/wasm/
npm run typecheck
```

`bash ../scripts/check-wasm-fresh.sh [dir]` rebuilds in release mode and
byte-compares the result with `dir` (default `js/wasm`). The build is
reproducible across machines when the toolchain matches; CI pins
rustc 1.95.0, wasm-pack 0.15.0 and binaryen 132.
