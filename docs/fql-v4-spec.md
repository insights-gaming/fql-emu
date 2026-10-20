# FQL v4 semantics reference (for emulator implementers)

This document describes the behaviour the emulator must reproduce. It covers
**FQL v4** only: the JSON-expression language spoken by the `faunadb` drivers
(`X-FaunaDB-API-Version: 4`) with `q.Let`, `q.Match`, `q.Paginate`, and so on.
It does **not** cover FQL v10 / "FQL X" (`fauna` driver, `users.where(...)`
syntax). The two share a server but nothing else that matters here.

Every section links to the open-sourced server
([fauna/faunadb](https://github.com/fauna/faunadb), Apache-2.0). Links are
pinned to commit [`4f8f8be`](https://github.com/fauna/faunadb/tree/4f8f8be39012f13f2a7b778ef05952afb01fd493)
(2025-05-03) so line numbers stay valid. The v4 implementation lives under
[`ext/model/src/main/scala/ast/`](https://github.com/fauna/faunadb/tree/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast);
the v10 interpreter is the sibling
[`runtime/fql2/`](https://github.com/fauna/faunadb/tree/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/runtime/fql2)
directory and should be ignored (anything named `FQL2` or `fql2` is v10).
Fauna's own API tests under
[`ext/api/src/test/scala/queries/`](https://github.com/fauna/faunadb/tree/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/api/src/test/scala/queries)
are the best source of concrete expected outputs.

Where this document says "the emulator", it describes a decision made for
`fql-emu` that narrows the server's behaviour (usually: fail loudly).

---

## 1. Transport

A query is one HTTP request: `POST /` with a JSON body that *is* the
expression. The driver sets `X-FaunaDB-API-Version: 4`, `X-Last-Seen-Txn`,
`X-Query-Timeout` and `Authorization: Bearer <secret>`.

Responses:

| Outcome | Status | Body |
|---|---|---|
| success | 200 | `{"resource": <value>}` |
| failure | 400 / 403 / 404 | `{"errors": [<error>, ...]}` |

The status is derived from the *worst* error in the list: parse errors rank
above invalid-argument errors above permission errors above the rest; then a
not-found error maps to 404, permission denied to 403, everything else to 400
([`Error.statusCode`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/Error.scala#L14-L25)).
The response carries `X-Txn-Time` (transaction time in microseconds), which the
driver feeds back as `X-Last-Seen-Txn`.

One request is one transaction: all writes commit or none do, and every read
observes the same snapshot. Within the transaction a query sees its own
earlier writes.

Versions: v4 is `APIVersion.V4` (ordinal 6). Several functions are gated
with `introducedOn`/`removedOn`
([`APIVersion.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/atoms/src/main/scala/APIVersion.scala)).
For example, in v4 `Select` has a `default` argument, `And`/`Or` short-circuit,
`ContainsPath`/`ContainsField`/`ContainsValue` replace `Contains`, and
`First`/`Last`/`CurrentIdentity`/`HasCurrentIdentity` exist. Error
descriptions say "collection"/"document" rather than "class"/"instance"
(≥ v2.7).

## 2. Wire format and value model

Spec: [`Expression.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/Expression.scala)
(`Literal` hierarchy), [`Parser.scala` `EscapesParser`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/Parser.scala#L46-L240)
(decoding), [`Renderable.scala` `LiteralEncoder`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/Renderable.scala#L507-L760)
(encoding). Driver side: [`_json.js`](https://github.com/fauna/faunadb-js/blob/main/src/_json.js) and
[`values.js`](https://github.com/fauna/faunadb-js/blob/main/src/values.js).

| Type (`Type.name`) | JSON in | JSON out | Notes |
|---|---|---|---|
| Null | `null` | `null` | |
| Boolean | `true` | `true` | |
| Integer (`LongL`) | `1` | `1` | 64-bit signed; integer JSON literal |
| Double (`DoubleL`) | `1.0`, `1e3` | `1.5` | any JSON number with fraction/exponent. `1` and `1.0` are different types but `Equals` |
| String | `"a"` | `"a"` | |
| Bytes | `{"@bytes": "<base64>"}` | same | |
| Time | `{"@ts": "1970-01-01T00:00:00.000000001Z"}` | same, always 9 fractional digits, UTC | nanosecond precision; input accepts any ISO-8601 instant |
| Date | `{"@date": "2020-01-02"}` | same | |
| Ref | `{"@ref": {"id": "1", "collection": {"@ref": {"id": "users", "collection": {"@ref": {"id": "collections"}}}}}}` | same | native collections (`collections`, `indexes`, `functions`, `roles`, `databases`, `keys`, `tokens`, `credentials`, `access_providers`) have no parent. `"class"` is accepted as a synonym of `"collection"` on input. `"database"` scopes a ref to a child database. Legacy string form `"collections/users/1"` is also accepted |
| Set | `{"@set": {"match": <ref>, "terms": [...]}}` | same | an unevaluated set; forms mirror §7 |
| Lambda (`@query`) | `{"@query": {"lambda": "x", "expr": {...}}}` | `{"@query": {"lambda": ..., "expr": ..., "api_version": "4"}}` | a closed lambda (no free variables); how UDF bodies are stored |
| Array | `[...]` | `[...]` | elements are *expressions* on input |
| Object | `{"object": {...}}` on input | plain object, or `{"@obj": {...}}` if any key starts with `@` | a bare object in a query is a **form**, never a literal; the driver wraps literal objects with `object` |
| Page | (not expressible) | `{"before": [...]?, "after": [...]?, "data": [...]}` | only produced by `Paginate` |
| Document (`VersionL`) | (not expressible) | `{"ref": <ref>, "ts": <micros>, "data": {...}, ...}` | `ts` is a Long of microseconds, not a `@ts` |

Object key order is preserved, and duplicate keys keep the first occurrence
([`Literal.JSONDecoder.readObjectStart`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/Expression.scala#L84-L100)).
Integers outside the signed 64-bit range are a parse error.

Not modelled by the emulator (always a `not emulated` error if they would
matter): `@uuid` (v10 only in practice), scoped refs with `database`, event
literals (`{"ts", "action", "document"}`), `ElemL` (`sources: true`).

## 3. Expressions and evaluation

Spec: [`QueryParser`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/Parser.scala#L352-L510)
and the form table that follows it (lines 511–1425);
[`EvalContext.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/EvalContext.scala).

- A scalar is itself. An array evaluates each element.
- An object is dispatched by its **exact set of keys** (`MapRouter`). Each
  function registers one key set per combination of its optional arguments,
  e.g. `select`/`from`, `select`/`from`/`default`, `select`/`from`/`all`,
  `select`/`from`/`all`/`default`. An unknown key set is
  `invalid expression: No form/function found, or invalid argument keys: { a, b }.`
- Single-key objects whose key starts with `@` are value escapes (§2).
- Argument values are evaluated, then cast with
  [`Casts`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/Casts.scala).
  A failed cast is `invalid argument: <Expected> expected, <Provided> provided.`
  at the argument's position.
- **Varargs** (`Casts.ZeroOrMore`/`OneOrMore`): the argument may be an array
  (elements at positions `[k, i]`) or a single value (at `[k]`).
  `OneOrMore` with an empty array is `invalid argument: Non-empty array expected.`
  ([`FunctionHelpers.toElemsIter`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/package.scala)).
- Errors from independent sub-expressions accumulate: a query can return
  several errors.

### Special forms (lazy)

| Form | Keys | Semantics |
|---|---|---|
| Var | `var` | lookup; unbound → `invalid expression: Variable 'x' is not defined.` (a *parse* error) |
| Let | `let`, `in` | `let` is an array of single-key objects (v4 drivers) or an object (older); bindings are sequential, each sees the previous. Positions: `["let", i, "name"]` |
| Lambda | `lambda`, `expr` | parameter pattern is `"_"`, `"x"`, or a nested array of those; arrays destructure. Closes over the enclosing `Let` scope. Arity mismatch on application: `invalid argument: Lambda expects an array with N elements. Array contains M.` |
| If | `if`, `then`, `else` | condition must be Boolean; only one branch is evaluated |
| Do | `do` | evaluates in order, returns the last; empty → `invalid expression: Expression list cannot be empty.` |
| Object | `object` | literal object whose values are expressions |
| And / Or | `and` / `or` | short-circuit left to right (v3+); each operand must be Boolean |
| Call | `call`, `arguments?` | calls a UDF by name or `Function` ref with the evaluated arguments (an array binds a multi-parameter lambda); any error inside becomes `call error: Calling the function resulted in an error.` with the inner errors in `cause` |
| At | `at`, `expr` | temporal read at a snapshot. **Not emulated.** |
| Query | `query` | wraps a lambda into a `@query` value |
| Map/Foreach/Filter/Reduce | see §8 | lambda argument may be an inline `lambda` form or a lambda value |

Lambda and UDF call depth is capped at 200
([`EvalContext.MaxStackDepth`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/EvalContext.scala#L16));
exceeding it is `stack overflow: Call stack reached the maximum limit of 200.`

### Error positions

An error carries `position`, the path from the query root to the offending
argument: form keys and array indices, e.g. `["let", 0, "x", "select"]`
([`Position.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/Position.scala)).
The Scala idiom `pos at "from"` means "append `"from"`". Some functions
report at the form itself (`Abort`, `Equals` type errors on elements),
others at a named argument (`Select` not-found is at `from`; `Match` unknown
index is at `index`). When in doubt, read the function's `apply` and copy the
`pos at` chain exactly.

## 4. Errors

Spec: [`Error.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/Error.scala)
(codes), [`RenderError.encodeErrors`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/Renderable.scala#L975-L1190)
(descriptions), [`Error.toJSValue`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/Error.scala#L32-L60)
(shape).

```json
{ "code": "invalid argument",
  "description": "String expected, Integer provided.",
  "position": ["concat", 1],
  "failures": [ { "field": ["name"], "code": "value required", "description": "Required value not found." } ],
  "cause": [ { "code": "...", "description": "...", "position": [...] } ] }
```

`failures` only appears for `validation failed`; `cause` only for
`call error`. Codes the drivers and applications branch on:

| code | when | status |
|---|---|---|
| `invalid expression` | parse errors: bad form, bad escape, unbound variable, empty `do` | 400 |
| `invalid argument` | every `Casts` failure and the `InvalidArgumentError` family (divide by zero, bad regex, bounds, bad cursor, bad time string, lambda arity, ...) | 400 |
| `invalid ref` | `UnresolvedRefError`: `Ref refers to undefined collection 'x'` (also index/function/role/database) | 400 |
| `value not found` | `Select` without default: `Value not found at path [a,b].` | 404 |
| `instance not found` | `Document not found.` / `Collection not found.` / `Index not found.` / `Set not found.` / `Index 'x' is not active.` | 404 |
| `instance already exists` | `Create` with an explicit ref that exists; `CreateCollection` of an existing name (`Collection already exists.`) | 400 |
| `instance not unique` | unique index violation on write | 400 |
| `validation failed` | schema/document validation, with `failures` | 400 |
| `transaction aborted` | `Abort(msg)`; description is `msg` | 400 |
| `call error` | a UDF body failed; see `cause` | 400 |
| `stack overflow` | depth > 200 | 400 |
| `permission denied` | ABAC / key role. **The emulator runs as admin and never raises it.** | 403 |
| `missing identity` | `CurrentIdentity()` without an identity | 400 |
| `feature not available` | server-side flags | 400 |

Validation `failures[].code` strings come from
[`ValidationException.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/storage/src/main/scala/doc/ValidationException.scala):
`value required`, `invalid field`, `invalid type`, `duplicate value`,
`bounds error`, `invalid reference`, `reserved name`, ... with descriptions
from the second `encodeErrors` overload
([Renderable.scala#L1195](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/Renderable.scala#L1195-L1251)).

The `Type` names used in descriptions are fixed strings
([`Type.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/Type.scala)):
`Integer`, `Double`, `Number`, `Boolean`, `String`, `Null`, `Bytes`, `Object`,
`Array`, `Page`, `Array or Page`, `Array, Set, or Page`, `Lambda`, `Ref`,
`Collection Ref`, `Index Ref`, `Database Ref`, `Key Ref`, `User Function Ref`,
`Role Ref`, `Set`, `Time`, `Date`, `Temporal`, `Time Unit`, `Identifier`,
`Scalar`, `Any`, `Field/Value`. Multi-type expectations render as
`A or B expected, C provided.` / `A, B, or C expected, D provided.`

Emulator-only codes (never produced by Fauna, so no test can pass by
accident): `not emulated`, `emulator limit exceeded`.

## 5. Ordering and equality

**Equality** (`Equals`, `ContainsValue`, `Distinct` on arrays): structural,
except numbers compare numerically across Long/Double (`Equals(1, 1.0)` is
true) ([`EqualsFunction`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/functions/BasicFunctions.scala#L30-L60)).

**Ordering** (`Lt`/`Lte`/`Gt`/`Gte`, `Min`/`Max`, index sort order, `Range`,
set algebra): the server encodes values as CBOR and compares bytes with a
custom comparator
([`CBOROrdering.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/codex/src/main/scala/cbor/CBOROrdering.scala));
term values are normalised first (object keys sorted)
([`IndexTerm.normalizedValue`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/storage/src/main/scala/index/IndexTerm.scala#L40-L50)).
The resulting total order, lowest first:

1. **Numbers**: Long and Double compared numerically with each other.
2. **Bytes**: lexicographic, then by length.
3. **Strings**: ICU collation in the server's default locale (root), with a
   binary tie-break so distinct strings never compare equal. So `"a" < "B"`
   (alphabetical), unlike code-point order.
4. **Arrays**: element-wise, then by length.
5. **Objects**: by sorted (key, value) pairs.
6. **Tagged values** by CBOR tag: Refs (13) < Times (16) < Dates (17) <
   Lambdas (18). Refs order by their internal document id (collection then
   id; ids minted by `NewId` are increasing).
7. **Simple values**: `false` < `true` < `null`. `null` is the maximum
   (`IndexTerm.MaxValue`), so documents missing an indexed value sort last.

Index fields marked `reverse: true` flip that field's comparison.

## 6. Documents, collections and writes

Spec: [`WriteFunctions.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/functions/WriteFunctions.scala),
[`WriteAdaptor.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/functions/WriteAdaptor.scala)
(`create` ≈ L1502, `update` L1566, `delete` L1662, unique constraints L43–66),
[`Data.patch`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/storage/src/main/scala/doc/Data.scala#L66-L95),
[`ReadFunctions.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/functions/ReadFunctions.scala),
tests: [`WriteFunctionsSpec.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/api/src/test/scala/queries/WriteFunctionsSpec.scala).

- A document is `{ref, ts, data, ttl?, credentials?, delegates?}`. `ts` is
  the transaction time (µs) of its last write. `credentials` is write-only.
- `Create(Collection("c"), {data})` mints an id with `NewId()`;
  `Create(Ref(Collection("c"), "123"), {data})` uses that id and fails with
  `instance already exists` if taken. Creating into a collection that does
  not exist is `invalid ref`. Creating into a native collection
  (`Create(Collections(), ...)`) is `invalid argument`.
- `Update(ref, {data, ...})` **patches**: objects merge recursively, a field
  set to `null` is removed, arrays and scalars replace wholesale
  (`Data.patch`). `Replace(ref, {data})` replaces the whole `data`. Both bump
  `ts` and return the new document. Missing ref → `instance not found` at
  `["update"]` / `["replace"]`.
- `Delete(ref)` returns the deleted document; subsequent `Exists` is false.
- `Get(ref)` returns the document; `Get(set)` returns the first element's
  document; `Exists(ref|set)`.
- **Unique indexes** are checked on every write: if another document has an
  entry with identical terms+values in a unique index, the write fails with
  `instance not unique`. Creating two schema objects with the same name
  fails with `instance already exists` (the schema name indexes are unique).
- Schema objects are documents too: `Get(Collection("c"))` returns
  `{ref, ts, history_days, name, data?, ttl_days?}`; indexes return
  `{ref, ts, active, serialized, name, source, terms?, values?, unique?, partitions, data?}`;
  functions `{ref, ts, name, body, role?, data?}`. `history_days` defaults to
  30; `partitions` is 1 for unique or term-less indexes, else 8.
- Reserved names (`events`, `sets`, `self`, `documents`, `_`, and the native
  collection names) fail validation.
- `NewId()` returns a string of a 64-bit snowflake id. `Now()` /
  `Time("now")` is the transaction snapshot time and is constant for the
  whole query.
- TTL: `ttl` is stored and returned, but expiry is a background task.
  **The emulator never expires documents.**
- History (`Events`, `Insert`, `Remove`, `At`, the `ts` argument of
  `Get`/`Exists`/`Paginate`): **not emulated**.

## 7. Indexes and sets

Spec: [`Index.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/doc/Index.scala)
(`TermConfig.extractor` L82, `legacyDataExtractor` L160–205, `Index.apply` L296–345),
[`SetFunctions.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/functions/SetFunctions.scala),
[`set/`](https://github.com/fauna/faunadb/tree/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/set)
(`IndexSet`, `DocumentsSet`, `UnionSet`, `IntersectionSet`, `DifferenceSet`,
`DistinctSet`, `RangeSet`, `JoinSet`, `LambdaFilterSet`, `ReverseSet`),
[`IndexValue.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/storage/src/main/scala/index/IndexValue.scala),
tests: `MatchFunctionSpec`, `IndexSpec`, `UnionFunctionSpec`,
`IntersectionFunctionSpec`, `DifferenceFunctionSpec`, `DistinctFunctionSpec`,
`RangeFunctionSpec`, `JoinFunctionSpec`, `ReverseFunctionSpec`,
`DocumentsFunctionSpec`, `SingletonFunctionSpec` in the queries test dir.

### Index definition (`CreateIndex` params)

```json
{ "name": "users_by_email",
  "source": <collection ref> | "_" | [ { "collection": <ref> | "_", "fields": { "<binding>": <@query lambda> } }, ... ],
  "terms":  [ { "field": ["data", "email"] } | { "binding": "name" }, "reverse"?: bool ],
  "values": [ ...same shape... ],
  "unique": false, "serialized": true, "data": {...} }
```

Index **entries** are computed per document at write time
([`TermConfig.extractor`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/doc/Index.scala#L82-L90)):

- `field` paths are `Select`ed from the document object (`["ref"]`, `["ts"]`,
  `["data", ...]`); a missing path yields `null`.
- `binding` evaluates the named source lambda with the document as argument.
- An **array** value expands to one entry per element (`select_as_index`
  semantics); several array-valued fields produce the cartesian product.
  Nested arrays flatten; objects inside arrays become `null`.
- Entries are sorted by `(values..., ref)` with per-field `reverse`, ties
  broken by ref.

### Sets

Sets are lazy values; `Paginate` and the fold functions materialise them.

| Function | Yields |
|---|---|
| `Match(Index("i"), terms...)` | entries whose terms equal the given terms (type-sensitive: `1` does not match `"1"`; `1` and `1.0` are different index terms). What each element *is* depends on the index's `values` ([`IndexTuple.coveredValues`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/storage/src/main/scala/index/IndexTuple.scala#L48-L53), [`Literal.fromIndexTerms`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/Expression.scala#L128-L135)): no `values` → the document ref; exactly one value → that value as a scalar; several → an array of the values in order. The ref is **not** appended; add `{"field": ["ref"]}` to `values` to get it. Unknown index name → `invalid ref: Ref refers to undefined index 'i'` at `["index"]` |
| `Documents(Collection("c"))` | every document's ref, ordered by ref |
| `Singleton(ref)` | a one-element set |
| `Union(a, b, ...)` | ordered merge keeping duplicates from different sets collapsed by key |
| `Intersection(a, b, ...)` | elements present in all |
| `Difference(a, b, ...)` | elements of `a` not in any other |
| `Distinct(set)` | de-duplicated by value (useful when the index has values but no ref) |
| `Range(set, from, to)` | elements whose key is between `from` and `to` **inclusive**; `from`/`to` may be a prefix of a multi-value key, or `[]` for unbounded |
| `Join(set, lambda\|index)` | for each element, the set returned by the lambda (or `Match(index, element)`), concatenated |
| `Filter(lambda, set)` | lazy filter; the lambda must return Boolean |
| `Reverse(set)` | reversed order |
| `Events(ref\|set)` | history. **Not emulated.** |

Union/Intersection/Difference also accept plain arrays and then return arrays.

### Pagination

Spec: [`PaginateFunction.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/functions/PaginateFunction.scala)
(the doc comment describes direction semantics),
[`ReadAdaptor.paginate` / `PaginateImpl`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/functions/ReadAdaptor.scala#L653-L860),
[`CursorParser`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/Parser.scala#L1463-L1640).

- `Paginate(set, {size?, after?, before?, cursor?, ts?, events?, sources?})`.
  `size` defaults to **64**, max **100 000**.
- Pages are always returned in ascending order. `after` paginates forward
  and is **inclusive**; `before` paginates backward and is **exclusive**.
  `cursor` is `{"after": c}` or `{"before": c}` (anything else is
  `invalid argument: Invalid cursor object. Expected an object with either 'before' or 'after' key in it.`).
- A cursor is the entry's values followed by its ref
  ([`IndexTuple.cursor`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/storage/src/main/scala/index/IndexTuple.scala#L40-L46)), so for `Documents()` it is `[ref]` and for an index with values `v1, v2` it is `[v1, v2, ref]`, even though the page elements themselves do not include the ref. A *prefix* cursor (fewer
  elements than the key) is allowed and bounds on the prefix.
- The server over-fetches `size + 1` to decide whether to emit a cursor:
  `after` is set when more elements follow; `before` is echoed from the
  request. Walking forward with `after` then backward with `before` must
  visit the same pages.
- `ts`, `events`, `sources`: **not emulated**.

## 8. Arrays, pages and folds

Spec: [`ArrayFunctions.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/functions/ArrayFunctions.scala),
[`AbstractFoldFunction.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/functions/AbstractFoldFunction.scala),
[`ReduceFunctions.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/functions/ReduceFunctions.scala),
tests: `ArrayFunctionsSpec`, `FilterFunctionSpec`, `ReduceFunctionsSpec`.

- `Map`, `Foreach`, `Filter`, `Reduce` and the aggregates (`Count`, `Sum`,
  `Mean`, `Max`, `Min`, `Any`, `All`) accept an Array, a Page or a Set
  (`Casts.Iterable`, error type name `Array, Set, or Page`). On a Page the
  result is a Page with the same `before`/`after`. On a Set they iterate the
  whole set (there is no implicit page size).
- `Filter`'s lambda must return Boolean, otherwise `invalid argument`.
- `Reduce(lambda, initial, collection)` folds left.
- `Sum`/`Mean` require numbers; `Sum` of Longs is a Long, any Double makes
  a Double; `Mean` is always a Double; empty `Mean` is an error.
  `Max`/`Min` use the cross-type ordering of §5.
- `Take(n, coll)` / `Drop(n, coll)` on arrays and pages; negative `n` on
  `Take` takes from the end (v4 `TakeFunction2`); on a page `Take` adjusts
  `after` and `Drop` adjusts `before`.
- `Prepend(elems, coll)`, `Append(elems, coll)`, `IsEmpty`, `IsNonEmpty`,
  `Reverse`, `First`, `Last`. `First`/`Last` accept an Array or a Set
  ([`ReadFunctions.scala#L65`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/functions/ReadFunctions.scala#L65-L95)); on an empty array the error is `invalid argument: Non-empty array expected.` and on an empty set `Non-empty set expected.`, both at `["first"]` / `["last"]`.
- `ToArray(object)` → `[[k, v], ...]`; `ToObject([[k, v], ...])`; a document
  converts to its field pairs.

## 9. Scalars

Spec and tests:
[`StringFunctions.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/functions/StringFunctions.scala) / `StringFunctionsSpec`,
[`NumericFunctions.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/functions/NumericFunctions.scala) / `NumericFunctionsSpec`,
[`TrigFunctions.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/functions/TrigFunctions.scala) / `TrigFunctionsSpec`,
[`TimeFunctions.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/functions/TimeFunctions.scala) / `TimeFunctionsSpec`,
[`ConversionFunctions.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/functions/ConversionFunctions.scala) / `ConversionFunctionsSpec`,
[`TypeCheckFunctions.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/functions/TypeCheckFunctions.scala) / `TypeCheckFunctionsSpec`,
[`MidstUnit.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/lang/src/main/scala/MidstUnit.scala) (time units).

- **Numbers**: `Add`/`Subtract`/`Multiply`/`Divide`/`Modulo` are varargs
  folds; all-Long stays Long (JVM wrapping on overflow, integer division),
  any Double promotes. Division or modulo by zero is
  `invalid argument: Illegal division by zero.` Trig functions return
  Double. Bitwise ops require Longs.
- **Strings** follow the JVM: `Format` is `String.format`, regex functions
  are `java.util.regex`, `Casefold` uses `java.text.Normalizer` (+ ICU case
  fold) with normalizers `NFKCCaseFold` (default), `NFC`, `NFD`, `NFKC`,
  `NFKD`. v4 names are snake_case (`lower_case`, `find_str_regex`); the
  older un-underscored spellings are still accepted. Invalid regex →
  `invalid argument: Search pattern /x/ is not a valid regular expression.`
- **Time**: `Time("now")` = transaction time; `Time(iso)` parses any
  ISO-8601 instant (`Cannot cast 'x' to a time.` otherwise); `Date("YYYY-MM-DD")`;
  `Epoch(n, unit)`; `ToMicros`/`ToMillis`/`ToSeconds`; components
  (`Second` … `Year`, `DayOfWeek` is ISO 1–7); `TimeAdd`/`TimeSubtract`/
  `TimeDiff` on Time or Date with units `day(s)`, `half day(s)`, `hour(s)`,
  `minute(s)`, `second(s)`, `millisecond(s)`, `microsecond(s)`,
  `nanosecond(s)` (mixing Time and Date is `Incompatible time arguments.`,
  unknown unit is `Invalid time unit 'x'.`). All arithmetic is UTC.
- **Conversions**: `ToString` renders Long/Double/Boolean/null/Time/Date
  (Double via Java `Double.toString`, so `1.0`, `1.0E10`); `ToNumber`/
  `ToDouble`/`ToInteger` parse strings with `NumberFormat` (locale-insensitive
  enough: `"1,000"` parses!), `ToInteger(1.9)` truncates; `ToTime(long)` treats
  the number as **milliseconds**; `ToDate(time)` truncates to the UTC day.
  Failures are `invalid argument: Cannot cast <Type> to <Type>.`
- **Type checks** (`IsNumber`, `IsString`, `IsRef`, `IsDoc`, ...) return
  Booleans; `IsDoc` is true for a document ref that exists or a document
  object.

## 10. Auth, keys, roles, databases

[`AuthFunctions.scala`](https://github.com/fauna/faunadb/blob/4f8f8be39012f13f2a7b778ef05952afb01fd493/ext/model/src/main/scala/ast/functions/AuthFunctions.scala):
`CurrentIdentity()` returns the identity ref of the calling token or
`missing identity`; `HasCurrentIdentity()`; `Login`, `Logout`, `Identify`,
`KeyFromSecret`, `CreateKey`, `CreateDatabase`, `CreateAccessProvider`,
`MoveDatabase`, roles/ABAC evaluation.

**The emulator** stores roles as inert documents, runs every query with
admin privileges, lets the test set what `CurrentIdentity()` returns, and
reports everything else as `not emulated`.

## 11. Conformance workflow

1. Find the function in `Parser.scala`'s form table to get its exact key
   set, optional arguments and casts.
2. Read its `apply` for the result type and the `pos at` chain.
3. Pull expected values and messages from the matching `*Spec.scala` test.
4. Encode the case in wire format in `conformance/cases/*.json`
   (format in `crates/fql-emu-core/tests/conformance.rs`).
5. When a fidelity bug is found against the real container, add the case
   first, then fix.
