# Known fidelity gaps

Everything here is a place where the emulator knowingly differs from the
v4 server, or refuses to answer. The contract is: **where it cannot match
the server exactly it fails with `not emulated` rather than guessing.**
Items marked *approximation* are the exceptions; keep them in mind when a
test depends on them, and prefer running such tests on the container.

Verify anything in doubt with `npm run differential` against a real
`fauna/faunadb` container (see `js/README.md`).

## Not emulated (always an error)

- Temporal reads: `At`, `Events`, `Insert`, `Remove`, and the `ts`
  argument of `Get`/`Exists`/`Paginate` when it differs from `Now()`;
  `Paginate` with `events: true` or `sources: true`.
- Child databases and scoped refs (`scope` arguments, `database` in refs),
  `CreateDatabase`, `MoveDatabase`.
- Keys, tokens, credentials and auth: `CreateKey`, `KeyFromSecret`,
  `Login`, `Logout`, `Identify`, `CurrentToken`, `HasCurrentToken`,
  `IssueAccessJWT`, access providers. `CurrentIdentity()` returns whatever
  the test set with `setIdentity`.
- Iterating the schema sets `Credentials()`; `Databases()`, `Keys()`,
  `Tokens()`, `AccessProviders()` are always empty.
- Index `transform` (e.g. `casefold`) and impure index bindings.
- Regex constructs the `regex` crate cannot express: lookaround,
  backreferences, possessive quantifiers, `\G`, comments mode; `$`/`\Z`
  before a trailing line terminator; multiline anchors around `\r` or
  U+2028/U+2029 (detected per input).
- `Format` `%@` for collection/document refs, sets and lambdas.
- `ToNumber` with non-ASCII digits.
- Times and dates beyond chrono's range (about ±262 143 years; the server
  allows ±10⁹).
- `Take`/`Drop` on a page produced by `Map`/`Filter` when the cursor would
  move (the server uses the page's pre-`Map` values, which `Value::Page`
  does not keep).
- Inputs on which the server itself throws an internal error (bad `$`
  references in regex replacements, `TimeDiff` overflow, overflowing
  `start`/`length`).
- Roles and ABAC are never enforced: every query runs as admin and
  `permission denied` is never raised.
- TTL expiry never happens (documents keep their `ttl` field).
- Rate limits, metrics headers, streaming.

## Approximations (may differ from the server)

- **Error positions for lambdas held in variables**: body errors are
  reported at the call site, because a stored `Lambda` records where it
  was parsed, not where a `Var` referenced it.
- **`Filter`/`Join` lambda errors on sets** are reported at the consuming
  call (`Paginate`, `Count`, ...) plus `filter`/`with`, because `SetExpr`
  does not record where the set was built.
- **Cursor tie-breaks**: when several input sets of `Union`/`Intersection`
  contain entries with identical values, the ref inside a cursor may be a
  different one than the server picks. The data returned is the same.
  `Join` ordering on ties is approximate.
- **Ref ordering** inside nested values compares the collection *name*
  as the tie-break where the server compares its internal collection id.
- **Schema set order** (`Collections()`, `Indexes()`, ...) is creation
  order.
- **`ts` inside index bindings** is the real document `ts`; the server
  sees a placeholder that indexes as `null` in a term.
- **Purity of `Filter`/`Join` lambdas** is a static scan of the body, not
  the server's effect analysis.
- **Title case** uses a simplified UAX#29 word segmenter (the ICU4X
  dictionary data would add megabytes to the wasm); Latin text matches.
- **`CaseFold` NFKCCaseFold** is built from NFKC + full case folding +
  default-ignorable removal, iterated to a fixed point.
- **`\b`** uses Rust's Unicode word boundary, close to but not identical
  to Java's.
- **Double rendering** (`ToString`, `Format`) uses shortest-digit output
  like JDK 19+; the server ran JDK 17, which prints extra digits for a few
  rare values. Trig results use the platform libm and may differ by an ulp.
- **`Range` rendering** echoes `from`/`to` as given; `Collections()` used
  as a set renders as `documents(...)`.
- **Server quirks ported as-is** (so tests see what production sees):
  `Union` returns a group of equal-valued entries in reverse document
  order, which can make `size: 1` paging loop; `before: null` on a values
  index skips entries whose first value is null; `Range` compares the
  document before the values; a single cursor value of `0`, `null` or a ref
  is reinterpreted on `Reverse` sets.

## Transaction semantics that are emulated (and easy to trip over)

Verified against the container:

- Schema objects created in a transaction are **not visible by name** in
  that transaction: `Do(CreateCollection({name: "c"}), Create(Collection("c"), ...))`
  fails with `invalid ref`, as does `Get(Collection("c"))`, `Match(Index(..))`
  on a just-created index, and `CreateIndex` sourcing a just-created
  collection (`validation failed` / `Cannot read reference.`).
- Documents written in a transaction **are** visible to later reads in it.
- `Now()` is constant within a transaction.
- A transaction with write effect answers HTTP 201.
