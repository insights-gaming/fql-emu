# fql-emu

An **FQL v4 query emulator** for running large numbers of tiny query tests
without a `fauna/faunadb` container. Written in Rust, shipped as wasm, with a
drop-in shim for the `faunadb` JS driver's `Client.query`.

> **This is not a database. Do not benchmark it.**
>
> Indexes are brute-force scans. Every transaction clones the whole store.
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
- Loads a schema bundle (collections, indexes, functions, roles) in the same
  wire format as `CreateCollection` / `CreateIndex` / `CreateFunction`
  params, so a codegen step can emit it.
- **Fails loudly** on anything it does not emulate (`not emulated` error code):
  temporal queries (`At`, `Events`, `ts` arguments), child databases and
  scoped refs, keys/tokens/auth (`Login`, `CreateKey`, ...), TTL expiry,
  streaming, roles/ABAC enforcement (every query runs as admin).

## Layout

```
crates/fql-emu-core/        # the emulator (pure Rust, no I/O)
  src/value.rs              # value model (Ref, Time, Date, Set, Lambda, Page, ...)
  src/ordering.rs           # Fauna cross-type total order (ICU root collation for strings)
  src/wire.rs               # @ref/@ts/@date/@set/@bytes/@query/@obj codec
  src/error.rs              # error codes, descriptions, positions, HTTP status
  src/store.rs              # documents, collections, indexes, functions; fork = clone; caps
  src/eval/                 # the interpreter; one module per function family
  tests/conformance.rs      # runs conformance/cases/*.json
crates/fql-emu-wasm/        # wasm-bindgen wrapper
js/                         # @insights/fql-emu: Client.query shim + vitest tests
conformance/cases/*.json    # wire-format cases ported from Fauna's own specs
scripts/build-wasm.sh       # builds js/wasm/ (uses nix if wasm-pack is absent)
docs/                       # fql-v4-spec.md (semantics + source links), architecture, schema bundle format
```

## Build and test

```sh
cargo test                         # Rust unit + conformance tests
bash scripts/build-wasm.sh         # release wasm -> js/wasm/
cd js && npm install && npm test   # driver shim tests against the wasm build
```

The wasm artifact in `js/wasm/` is a build output. CI rebuilds it from the
crate and fails if the committed/published artifact is stale, so TypeScript
contributors never need a Rust toolchain.

## JS usage

```ts
import faunadb from 'faunadb'
import { createEmulator } from '@insights/fql-emu'

const emu = createEmulator()                 // fresh, empty database
emu.loadSchema(schemaBundle)                 // collections, indexes, UDFs under their hashed names
emu.setNow(new Date('2024-01-01T00:00:00Z')) // Now() is pinned until you change it

const client = emu.client()                  // a real faunadb.Client whose HTTP layer is the emulator
await client.query(q.Create(q.Collection('users'), { data: { name: 'ada' } }))

const fork = emu.fork()                      // independent copy; the base is untouched
```

In vitest, replace the module that exports your `faunaClient` with
`emu.client()` (15 test files in the app already `vi.mock` that module as
`{ faunaClient: { query } }`).

## Spec

`docs/fql-v4-spec.md` is the implementer-facing description of FQL **v4**
semantics (not FQL v10 / FQL X), with links into the Fauna source for every
section. Read it before touching the interpreter.

## Conformance

`conformance/cases/*.json` hold wire-format query/expectation pairs ported
from Fauna's own API specs (`ext/api/src/test/scala/queries/*Spec.scala`).
They run in `cargo test` against the emulator, and the same files can be
replayed against a real `fauna/faunadb` container by the differential runner
to catch drift. Every fixed fidelity bug should add a case.

## Licence

MIT for this code. Semantics are ported (not copied) from Fauna's
Apache-2.0 source; see `OSS_NOTICE` if any text is lifted verbatim.
