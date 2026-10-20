# @insights/fql-emu

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

## Usage

```ts
import faunadb from 'faunadb'
import { createEmulator } from '@insights/fql-emu'

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
| `createEmulator({ maxDocuments?, now?, nextId? })` | a fresh, empty database |
| `emu.client(opts?)` | a new `faunadb.Client` on this emulator. `opts` go to the `Client` constructor (`observer`, `queryTimeout`, ...); `secret` and connection options are ignored |
| `emu.query(expr, options?)` | `client().query` on an internal client |
| `emu.fork()` | an independent copy (documents, schema, clock, id seed, limits) |
| `emu.loadSchema(bundle)` | load collections / roles / functions / indexes; throws `SchemaLoadError` (with `.errors`) listing the Fauna errors |
| `emu.setNow(date \| micros \| null)` | pin `Now()` and every written `ts`; `null` returns to the auto clock |
| `emu.setNextId(n \| bigint)` | seed for `NewId()` (use a `bigint` above 2^53, e.g. snowflake-sized ids) |
| `emu.setIdentity(ref \| null)` | what `CurrentIdentity()` returns |
| `emu.setMaxDocuments(n)` | change the document cap |
| `emu.stats()` | `{ collections, indexes, functions, documents, max_documents }` |
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
import type { Emulator } from '@insights/fql-emu'

const state = vi.hoisted(() => ({ emu: undefined as unknown as Emulator }))
vi.mock('./faunaClient', async () => {
  const { mockFaunaClient } = await import('@insights/fql-emu')
  return mockFaunaClient(() => state.emu)
})

const { createEmulator } = await import('@insights/fql-emu')
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
admin), TTL expiry, streaming. `Emulator.supportedForms()` lists what is
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
