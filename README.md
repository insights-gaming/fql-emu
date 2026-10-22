# fql-emu

An **FQL v4 query emulator** for running large numbers of tiny query tests
without a `fauna/faunadb` container. Written in Rust, shipped as wasm, with a
drop-in shim for the `faunadb` JS driver's `Client.query`.

> **This is not a database. Do not benchmark it.**
>
> Indexes are brute-force scans. Every writable transaction clones the whole
> store.
> There is a hard cap on the number of documents (default 5 000) and the
> emulator throws `emulator limit exceeded` when a test seeds more. It exists
> so that query *logic* can be tested quickly, in parallel, deterministically,
> and with free database forks. E2E runs stay on the real container.

## What it does

- Interprets the exact JSON the `faunadb` driver would POST (`{"let": ...}`),
  and returns the exact JSON the v4 HTTP API would answer, so the driver's own
  serialisation, `parseJSON`, `values.*` classes and error classes are used
  unchanged.
- Ports semantics from the open-sourced Fauna server
  ([fauna/faunadb](https://github.com/fauna/faunadb), Apache-2.0),
  `ext/model/src/main/scala/ast`: value ordering, error codes and
  descriptions, error positions, `Select` defaults, numeric promotion,
  pagination cursors, index term/value extraction, set algebra.
- Deterministic: inject the transaction clock (`Now()`) and the `NewId()`
  seed. `Now()` is fixed for the whole transaction.
- Transactions are atomic: a query that errors leaves no writes behind.
- Forks are `Clone`: seed once, fork per test.
- **Read-only forks** (`fork('read-only')`) simulate a `server-readonly`
  key: every write, every `Call` and listing roles/keys/databases answer
  Fauna's `permission denied` (403) exactly where the server would, while
  reads keep working. A fresh emulator is always writable; the seeded base
  is forked once, and that one read-only copy can be shared by every test
  that only reads, since nothing can change it. Read-only queries also run
  against the store in place, without the per-query copy, so their cost no
  longer grows with the fixture. `fork('read-only-pure-calls')`
  is the same but lets tests call UDFs that do not write.
- Loads a **schema bundle**: one JSON file listing the collections, indexes,
  functions and roles a test database should start with, each written as
  the params object of the corresponding `Create*` call. Seed it once and
  fork per test; see `docs/schema-bundle.md`.
- **Fails loudly** on anything it does not emulate (`not emulated` error code):
  temporal queries (`At`, `Events`, `ts` arguments), child databases and
  scoped refs, keys/tokens/auth (`Login`, `CreateKey`, ...), TTL expiry,
  streaming, roles/ABAC enforcement (every query runs as admin, or as a
  server-readonly key in read-only mode).

## Layout

```
crates/fql-lang/            # the FQL v4 language, independent of any storage (reusable)
  src/value.rs              # value model (Ref, Time, Date, Set, Lambda, Page, ...)
  src/ordering.rs           # Fauna cross-type total order (ICU root collation for strings)
  src/wire.rs               # @ref/@ts/@date/@set/@bytes/@query/@obj codec
  src/error.rs              # error codes, descriptions, positions, HTTP status
  src/mode.rs, effect.rs    # key permissions; static write-effect analysis
  src/backend.rs            # the `Backend` trait: the storage surface the language forms need
  src/eval/                 # interpreter, form registry, storage-independent forms
  tests/no_storage.rs       # runs queries against a trivial backend with no storage
crates/fql-emu-core/        # the emulator (pure Rust, no I/O): fql-lang + an in-memory store
  src/store.rs              # documents, collections, indexes, functions; fork = clone; caps
  src/eval/                 # `Backend` for the store; sets/indexes, reads, writes
  tests/conformance.rs      # runs conformance/cases/*.json
crates/fql-emu-wasm/        # wasm-bindgen wrapper
js/                         # @insights-gaming/fql-emu: Client.query shim + vitest tests
conformance/cases/*.json    # wire-format cases ported from Fauna's own specs
scripts/build-wasm.sh       # builds js/wasm/ (uses nix if wasm-pack is absent)
docs/                       # fql-v4-spec.md (semantics + source links), architecture, schema bundle format
```

`fql-lang` is the query language on its own: parse and evaluate the driver's
JSON, with Fauna's exact errors, against whatever storage implements its
`Backend` trait. The emulator is one such storage; another project can reuse
`fql-lang` to put the same language in front of a real database (see
`crates/fql-lang/README.md`).

## Build and test

```sh
cargo test                         # Rust unit + conformance tests
bash scripts/build-wasm.sh         # release wasm -> js/wasm/
cd js && npm install && npm test   # driver shim tests against the wasm build
```

The wasm artifact in `js/wasm/` is a build output. CI rebuilds it from the
crate and fails if the committed/published artifact is stale, so TypeScript
contributors never need a Rust toolchain.

## Install

The package is published to GitHub Packages as `@insights-gaming/fql-emu`.
Point the scope at the GitHub registry once (a token with `read:packages`
is required):

```ini
# .npmrc
@insights-gaming:registry=https://npm.pkg.github.com
//npm.pkg.github.com/:_authToken=${GITHUB_TOKEN}
```

```sh
npm install --save-dev @insights-gaming/fql-emu faunadb
```

## JS usage

```ts
import faunadb from 'faunadb'
import { createEmulator } from '@insights-gaming/fql-emu'

const emu = createEmulator()                 // fresh, empty database
emu.loadSchema(schemaBundle)                 // collections, indexes, functions, roles (docs/schema-bundle.md)
emu.setNow(new Date('2024-01-01T00:00:00Z')) // Now() is pinned until you change it

const client = emu.client()                  // a real faunadb.Client whose HTTP layer is the emulator
await client.query(q.Create(q.Collection('users'), { data: { name: 'ada' } }))

const fork = emu.fork()                      // independent copy; the base is untouched

const shared = emu.fork('read-only')         // a server-readonly key: writes and Call → permission denied;
                                             // one copy can be shared by every test that only reads
```

In vitest, replace the module that exports your `faunaClient` with
`emu.client()` (15 test files in the app already `vi.mock` that module as
`{ faunaClient: { query } }`).

## Spec

`docs/fql-v4-spec.md` is the implementer-facing description of FQL **v4**
semantics (not FQL v10 / FQL X), with links into the Fauna source for every
section. Read it before touching the interpreter.

## Conformance

`conformance/cases/*.json` hold about 1 650 wire-format query/expectation
pairs ported from Fauna's own API specs
(`ext/api/src/test/scala/queries/*Spec.scala`) and from probing a real
`fauna/faunadb` container. They run in `cargo test` against the emulator,
and `cd js && npm run differential` replays them against a real container
(`FAUNA_ENDPOINT`/`FAUNA_SECRET`), classifying every mismatch; the latest
report is `conformance/DIFFERENTIAL.md`. Cases marked `todo` are the known
gaps listed in `docs/gaps.md`. Every fixed fidelity bug should add a case.

Coverage: 218 of the 233 v4 forms are implemented; the 15 stubs are auth,
keys, child databases, history/temporal reads and `At`, all of which fail
with a `not emulated` error.

## Licence

MIT for this code. Semantics are ported (not copied) from Fauna's
Apache-2.0 source; see `OSS_NOTICE` if any text is lifted verbatim.
