# Architecture

## Flow of a query

```
faunadb Client.query(expr)
  └─ query.wrap(expr) → JSON  (driver, unchanged)
  └─ client._http.execute({body})  ← replaced by the emulator adapter (js/src)
        └─ wasm FqlEmulator.query(body) → {status, body}
              └─ fql_emu_core::Emulator::query(json)
                    1. clone Database (working copy); ts = clock.next()
                    2. eval(expr) with Ctx{db: &mut working, ts}
                    3. Ok  → commit working; 200 {"resource": encode(v)}
                       Err → discard;        4xx {"errors": [...]}
  └─ json.parseJSON(body) → values.Ref / FaunaTime / ...   (driver, unchanged)
  └─ errors.FaunaHTTPError.raiseForStatusCode(requestResult) (driver, unchanged)
```

## Interpreter

There is no separate AST. `eval::eval` walks the query JSON:

- scalars and arrays are literals (`1` → Long, `1.0` → Double);
- an object is dispatched through `eval::forms::Registry` by its **exact key
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

`Lambda` values are closures over the `Env` (a persistent linked list).
Stored UDF bodies are closures over the empty env. `MAX_STACK_DEPTH` = 200.

## Store

`store::Database` is plain `BTreeMap`/`IndexMap` data. A fork is a clone.
Each query clones the database and commits the clone on success; this is
O(size of fixture) per query, which is fine by design (small fixtures, hard
cap). If this ever matters, switch to persistent maps (`im`), not to a real
storage engine.

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
