# Schema bundle format

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
      "name": "greet_1a2b3c",
      "body": { "@query": { "lambda": "name", "expr": { "concat": ["hi ", { "var": "name" }] }, "api_version": "4" } }
    }
  ]
}
```

The intended producer is the existing codegen: it already links `.fql`
bodies under their hashed names and reads indexes from the `.ndjson` files,
so emitting this bundle is a matter of serialising what it already has.
