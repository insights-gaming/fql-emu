# fql-lang

The **FQL v4 query language** without a database: the wire format the
`faunadb` drivers speak, the value model, Fauna's error codes and positions,
the server's cross-type ordering, and an interpreter for every form that
does not need storage. Plug in a storage engine by implementing one small
trait (`Backend`) and registering your own storage forms (`get`, `create`,
`match`, `paginate`, ...).

It is the language layer of [fql-emu](../../README.md), an in-memory FQL v4
emulator for tests (`crates/fql-emu-core`). That crate shows a complete
backend. `fql-lang` itself knows nothing about the emulator's store, so
another project can reuse it to put FQL v4 in front of its own storage.

## Fidelity

Semantics are ported from the open-sourced Fauna server
([fauna/faunadb](https://github.com/fauna/faunadb), Apache-2.0),
`ext/model/src/main/scala/ast`. Error codes, descriptions and positions are
byte-identical to the server's, checked by fql-emu's conformance suite
(about 1 700 cases ported from Fauna's API specs and replayed against a
real `fauna/faunadb` container). Module docs point at the Scala files they
port.

Anything the language layer cannot do faithfully fails with the error code
`not emulated` rather than approximating. Two of those cases sit in the
language forms themselves: iterating a native collection (`Collections()`,
...) and `Take`/`Drop` on a page whose cursors would have to move.

## What it contains

| Module | Contents |
| --- | --- |
| `wire` | decoding the driver's JSON (`@ref`, `@ts`, `@date`, `@set`, `@bytes`, `@query`, `@obj`) and encoding results |
| `value` | `Value`, `Ref`, `Timestamp`, `Lambda` (a closure over an `Env`), `SetExpr` (an unevaluated set), pages |
| `error` | `FaunaError` with the server's codes and descriptions, `Position`, HTTP status of an error list |
| `ordering` | the server's total order across types (ICU root collation for strings) |
| `mode` | `Mode`: the key permissions a query runs under |
| `effect` | static read/write effect of an expression (`Expression.maxEffect`) |
| `backend` | the `Backend` trait |
| `eval` | `eval`, `Ctx`, the form `Registry`, lambdas and `Call`, and the storage-independent forms (`basic`, `logic`, `numeric`, `string`, `time`, `collection`) |

The interpreter evaluates the driver's JSON directly; there is no separate
AST. Scalars and arrays are literals, an object is a form when its exact key
set matches one in the registry (like the server's `MapRouter`), a single
`@` key is a wire escape, and anything else is an `invalid expression`.

## Implementing `Backend`

```rust
pub trait Backend: Sized {
    /// Key permissions the query runs under.
    fn mode(&self) -> Mode;
    /// `CurrentIdentity()`.
    fn identity(&self) -> Option<&Ref>;
    /// `NewId()`.
    fn new_id(&mut self) -> String;
    /// Whether the schema object or document named by `r` exists.
    fn exists_by_ref(&self, r: &Ref) -> bool;
    /// The stored user-defined function's body (a closure over the empty env).
    fn function_body(&self, name: &str) -> Option<&Lambda>;
    /// Materialise a set in iteration order (the values it yields).
    fn materialize(ctx: &mut Ctx<'_, Self>, set: &SetExpr, pos: &Position) -> R<Vec<Value>>;
}
```

- `mode`: `ReadWrite`, or `ReadOnly` for a server-readonly key (writes,
  `Call` and listing roles/keys/databases are denied with `permission
  denied` where the server denies them). `ReadOnlyPureCalls` is an emulator
  extension of the server-readonly key that still allows `Call` of
  functions whose bodies do not write; it has no server equivalent.
- `exists_by_ref` backs `IsDoc` (document refs) and the server's `invalid
  ref` errors: refs to collections, indexes, functions and roles are asked
  as is, and a document ref is checked by its collection
  (`Ref::collection(coll)`). On the server, a schema object's name keeps
  resolving for the rest of the transaction that deletes it.
- `function_body` backs `Call`; the interpreter applies the lambda, wraps
  errors in `call error` and enforces the stack depth limit.
- `materialize` is called whenever a language form needs a set's elements
  (`Count`, `Select` on a set, `Reduce`, `IsEmpty`, ...). It receives the
  context because sets contain lambdas (`Filter`, `Join`, index bindings)
  that only the interpreter can apply. A backend without sets can return
  an error.

A value of your backend type is the state one transaction reads and
writes; the interpreter borrows it mutably for the whole query.

## Registering storage forms

`eval::language_forms()` returns a `Registry` with the storage-independent
forms. Add your own on top; a handler receives the context (whose `backend`
field is your concrete type), the environment, the *unevaluated* arguments
and the form's position:

```rust
use fql_lang::error::{one, FaunaError, Position, R};
use fql_lang::eval::{self, language_forms, Args, Ctx, Registry};
use fql_lang::{env::Env, value::Value, wire};

fn get(ctx: &mut Ctx<MyStore>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let r = args.eval(ctx, env, "get", pos)?;      // errors inside are at ["get", ...]
    // Report unresolvable schema refs like the server's cast does:
    if let Some(e) = eval::collection::unresolved_ref(ctx, &r, &pos.key("get")) {
        return one(e);
    }
    match &r {
        Value::Ref(r) => ctx.backend.read(r).ok_or_else(|| vec![FaunaError::document_not_found(pos)]),
        other => one(FaunaError::invalid_argument(&[fql_lang::value::Type::Ref], other.rtype(), &pos.key("get"))),
    }
}

let mut registry: Registry<MyStore> = language_forms();
// name, required keys, optional keys, handler
registry.add("get", &["get"], &["ts"], get);
registry.add("create", &["create"], &["params"], create);
registry.add("match", &["match", "index"], &[], match_index);
registry.add("paginate", &["paginate"], &["after", "before", "cursor", "ts", "size", "events", "sources"], paginate);
registry.stub("insert", &["insert", "ts", "action"], &["params"]);   // recognised, answers `not emulated`

let mut ctx = Ctx::new(&mut store, &registry, transaction_ts);
let result = eval::eval(&mut ctx, &Env::empty(), &query_json, &Position::root());
// Ok(v)    => {"resource": wire::encode(&v)}, status 201 if ctx.wrote else 200
// Err(es)  => {"errors": es.iter().map(|e| e.to_json())}, status error::status_code(&es)
```

The first registered form whose key set matches wins, and a form matches
when the object has every required key and nothing outside required ∪
optional. Set `ctx.wrote = true` in write forms (the server answers 201 for
a transaction that wrote), and call `eval::deny_if_read_only(ctx, pos)` at
the position where the server checks the permission. The static effect
analysis (`effect`, used for `Join` targets, set filters and
`ReadOnlyPureCalls`) rates forms by registered name, so register storage
forms under the server's names (`get`, `create`, `update`, `call`, ...).

`tests/no_storage.rs` is a runnable example: a backend with no documents,
the language forms and one storage form. `crates/fql-emu-core` is a full
one: `Database` implements `Backend`, and `eval/sets.rs`, `eval/read.rs`
and `eval/write.rs` register the set, read and write forms.

## Licence

MIT. Semantics are ported (not copied) from Fauna's Apache-2.0 source.
