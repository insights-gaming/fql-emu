# Architecture

## Flow of a query

```
faunadb Client.query(expr)
  └─ query.wrap(expr) → JSON  (driver, unchanged)
  └─ client._http.execute({body})  ← replaced by the emulator adapter (js/src)
        └─ wasm FqlEmulator.query(body) → {status, body}
              └─ fql_emu_core::Emulator::query(json)
                    1. clone Database (working copy); ts = clock.next()
                    2. eval(expr) with Ctx{backend: &mut working, registry, ts}
                    3. Ok  → commit working; 200 {"resource": encode(v)}
                       Err → discard;        4xx {"errors": [...]}
  └─ json.parseJSON(body) → values.Ref / FaunaTime / ...   (driver, unchanged)
  └─ errors.FaunaHTTPError.raiseForStatusCode(requestResult) (driver, unchanged)
```

## Crates

The language and the storage are separate crates:

- **`fql-lang`** is the FQL v4 language with no storage: the wire codec,
  values, errors (codes, descriptions, positions), ordering, key
  permissions (`Mode`), the static effect analysis, the interpreter and form
  registry, and every form that does not need a database (`let`, `if`,
  `map`, `select`, `concat`, `time_add`, ...: the `basic`, `logic`,
  `numeric`, `string`, `time` and `collection` modules). It does not depend
  on `fql-emu-core` and could put the language in front of any storage.
- **`fql-emu-core`** is the emulator: the in-memory `store::Database`, the
  storage-bound forms (`eval/sets.rs`: `Match`, `Paginate`, set algebra;
  `eval/read.rs`: `Get`, `Exists`, `CurrentIdentity`, ...;
  `eval/write.rs`: `Create`, `Update`, `CreateCollection`, ...), and the
  `Emulator` API (transactions, forks, schema bundles). It re-exports
  `fql_lang::{value, wire, error, env, ordering, Mode}` so its public paths
  are unchanged.

The two meet at the `fql_lang::Backend` trait, the only storage the
language forms touch:

```rust
pub trait Backend: Sized {
    fn mode(&self) -> Mode;                         // key permissions
    fn identity(&self) -> Option<&Ref>;             // CurrentIdentity()
    fn new_id(&mut self) -> String;                 // NewId()
    fn exists_by_ref(&self, r: &Ref) -> bool;       // IsDoc, `invalid ref` errors
    fn function_body(&self, name: &str) -> Option<&Lambda>;  // Call
    fn materialize(ctx: &mut Ctx<'_, Self>, set: &SetExpr, pos: &Position) -> R<Vec<Value>>;
}
```

`Ctx<B>` carries `backend: &mut B` and the `registry: &Registry<B>` that
dispatches forms; handlers are `fn(&mut Ctx<B>, &Env, &Args, &Position)`.
The language forms are generic over `B`; a backend's own forms are
written against its concrete type, so the emulator's read/write/set forms
use `Ctx<Database>` and reach into the store directly. `Database`
implements `Backend` (`materialize` is the brute-force set engine below),
and the emulator's registry is `fql_lang::eval::language_forms()` plus
`sets::register`, `read::register` and `write::register`, built once.

## Interpreter

There is no separate AST. `eval::eval` (in `fql-lang`) walks the query
JSON:

- scalars and arrays are literals (`1` → Long, `1.0` → Double);
- an object is dispatched through the `Ctx`'s `Registry` by its **exact key
  set** (like Fauna's `MapRouter`): `{"let","in"}` → `basic::let_`,
  `{"match","index"}` → `sets::match_index`, ...;
- otherwise a single `@`-key object is a wire escape (`@ref`, `@ts`, `@set`,
  `@query`, `@obj`, `@bytes`, `@date`);
- otherwise it is an "invalid expression / No form/function found" error.
  (Literal objects must be sent as `{"object": {...}}`; the driver does that.)

Handlers get the *unevaluated* arguments (`Args`) so lazy forms (`if`,
`let`, `lambda`, `and`/`or`) can control evaluation. Argument positions
follow the server: an error in argument `k` of a form at position `p` is at
`p ++ [k]`, and varargs elements at `p ++ [k, i]`.

Forms that are recognised but not emulated are registered as stubs
(`Registry::stub`) and answer `not emulated`. `Registry::stub_with` gives
a stub a handler for the checks the server makes first: the emulator uses
it for `Insert`, `Remove`, `CreateDatabase`, `CreateKey` and
`CreateAccessProvider`, which a read-only key denies before they would be
reported as not emulated.

`Lambda` values are closures over the `Env` (a persistent linked list).
Stored UDF bodies are closures over the empty env. `MAX_STACK_DEPTH` = 200.

## Store

`store::Database` is plain `BTreeMap`/`IndexMap` data. A fork is a clone.
Each writable query clones the database and commits the clone on success;
this is O(size of fixture) per query, which is fine by design (small
fixtures, hard cap). If this ever matters, switch to persistent maps (`im`),
not to a real storage engine.

A read-only query (`Mode::ReadOnly`, `Mode::ReadOnlyPureCalls`) skips the
copy and evaluates against the database in place (`Emulator::transact`).
That is sound because every mutation in `eval/` sits behind
`deny_if_read_only`, which fails before anything is staged; the only state
a read can move is the clock and the `NewId()` counter, and a failed query
restores the counter, so it ends exactly as a discarded copy would. The
conformance runner checks after every case of a `read_only` suite that the
stored data is unchanged, in case a new write path forgets its check.

Schema objects (collections, indexes, functions, roles) are parsed into
typed definitions at create time and rendered back into the document shape
`Get` returns (`render_collection` etc.).

## Sets and indexes (brute force)

A set value is an unevaluated `SetExpr`. Materialising it:

- `Documents(coll)`: all docs in the collection, ordered by ref.
- `Match(index, terms)`: for every document in the index's source
  collections, evaluate the index's term and value extractors
  (`Index.scala: TermConfig`), expand array fields one entry per element
  (cartesian across fields), keep entries whose terms equal the match terms,
  sort by `(values..., ref)` using `ordering::compare` (with `reverse`
  flags), and yield the document ref when the index has no `values`, the
  bare value when it has one, or an array of the values when it has
  several. The ref is only part of the sort key and the cursor
  (`Entry.key`), never appended to the yielded value.
- `Union`/`Intersection`/`Difference`/`Distinct` are set operations on the
  sorted entry lists keyed by the full entry key.
- `Range` filters on the entry key, `Reverse` reverses, `Filter`/`Join` map
  over the materialised entries.
- `Paginate` slices the materialised list by `size` (default 64, max
  100 000) and `after`/`before` cursors; cursors are the entry keys.

Unique constraints are checked on write by materialising the unique index
for the new document's terms+values and looking for a different ref.

## Determinism

- `Clock::Fixed(ts)` pins `Now()`, `Time("now")` and every written `ts`.
- `Clock::Auto` advances 1 µs per transaction and never reads the wall clock.
- `NewId()` is a counter from a settable seed.

## Errors

`error::FaunaError` mirrors `fauna.ast.Error` codes and
`RenderError.encodeErrors` descriptions. Emulator-specific failures use codes
that do not exist in Fauna (`not emulated`, `emulator limit exceeded`) so
tests cannot accidentally pass against them.

## What is deliberately not emulated

temporal reads (`At`, `ts`), `Events`/history, child databases/scoped refs,
keys/tokens/credentials/`Login`/`Identify`, roles and ABAC enforcement
(beyond the server-readonly simulation of read-only forks),
access providers / JWT, TTL expiry, streaming, `Insert`/`Remove`,
`MoveDatabase`, rate limits, metrics headers.
