# Schema bundle format

A **schema bundle** is a single JSON document that describes the schema a
test database should start with: its collections, indexes, user-defined
functions and roles. Loading it is equivalent to running the corresponding
`CreateCollection` / `CreateIndex` / `CreateFunction` / `CreateRole`
queries one after another, but without writing those queries in every test
suite. The usual pattern is to load the bundle into one emulator, then
`fork()` it per test so each test starts from the same schema with no
documents.

Any tool that already knows your schema (a migration script, a deploy step,
a hand-written file checked into the repo) can produce one: it only has to
serialise the params it would otherwise send to Fauna.

`Emulator::load_schema` / `emu.loadSchema()` takes one JSON document:

```json
{
  "collections": [ <CreateCollection params>, ... ],
  "roles":       [ <CreateRole params>, ... ],
  "functions":   [ <CreateFunction params>, ... ],
  "indexes":     [ <CreateIndex params>, ... ]
}
```

Each entry is **exactly the params object** you would pass to the
corresponding `Create*` function, serialised in wire format (what
`JSON.stringify(q.wrap(params))` produces, minus the `object` wrapper):

- refs are `{"@ref": {"id": "users", "collection": {"@ref": {"id": "collections"}}}}`
- lambdas (function bodies, index bindings) are
  `{"@query": {"lambda": ..., "expr": ..., "api_version": "4"}}`. **Always
  include `"api_version": "4"`**: on the real server a `@query` without it
  is stored as an API 2.12 lambda (and rendered back as such), which is what
  the driver's `q.Query(q.Lambda(..))` avoids by sending the `query` form
- plain nested objects are plain JSON objects (no `{"object": ...}` wrapper)

Sections load in the order collections → roles → functions → indexes, so
indexes can reference collections and functions can reference roles.

Example:

```json
{
  "collections": [{ "name": "users", "history_days": 0 }],
  "indexes": [
    {
      "name": "users_by_email",
      "source": { "@ref": { "id": "users", "collection": { "@ref": { "id": "collections" } } } },
      "terms": [{ "field": ["data", "email"] }],
      "unique": true
    }
  ],
  "functions": [
    {
      "name": "greet",
      "body": { "@query": { "lambda": "name", "expr": { "concat": ["hi ", { "var": "name" }] }, "api_version": "4" } }
    }
  ]
}
```

Loading is **not transactional across entries**: each entry is committed
before the next one is created, and loading stops at the first entry Fauna
would reject, leaving the earlier ones in place. `loadSchema` throws a
`SchemaLoadError` whose `.errors` holds that entry's Fauna errors, with
positions like `["indexes", 2, "source"]` pointing at the offending entry.
