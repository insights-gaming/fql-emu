//! # fql-lang
//!
//! The FQL v4 query language, independent of any storage: the wire format
//! the drivers speak, the value model, Fauna's error codes and positions,
//! the cross-type ordering, and an interpreter for every form that does
//! not need a database.
//!
//! Semantics are ported from the open-sourced Fauna server
//! (<https://github.com/fauna/faunadb>, `ext/model/src/main/scala/ast`).
//! Error codes, descriptions and positions are byte-identical to the
//! server's; module docs point at the specific Scala files.
//!
//! ## Contents
//!
//! - [`wire`]: decoding of the driver's JSON (`@ref`, `@ts`, `@date`,
//!   `@set`, `@bytes`, `@query`, `@obj`) and encoding of results.
//! - [`value`]: [`Value`](value::Value), [`Ref`](value::Ref), lambdas
//!   (closures over an [`Env`](env::Env)), unevaluated sets
//!   ([`SetExpr`](value::SetExpr)), pages, timestamps.
//! - [`error`]: [`FaunaError`](error::FaunaError) with the server's codes
//!   and descriptions, [`Position`](error::Position), HTTP status.
//! - [`ordering`]: the server's total order across types (ICU root
//!   collation for strings).
//! - [`eval`]: the interpreter ([`eval::eval`]), the form
//!   [`Registry`](eval::Registry) and the storage-independent forms
//!   ([`eval::language_forms`]).
//! - [`effect`]: the static read/write effect of an expression
//!   (`Expression.maxEffect`).
//! - [`Mode`]: the key permissions a query runs under.
//! - [`Backend`]: the small storage interface the language forms need.
//!
//! ## Using it
//!
//! Implement [`Backend`] for your storage, start from
//! [`language_forms`](eval::language_forms) and register your own
//! storage-bound forms (`get`, `create`, `match`, `paginate`, ...) on the
//! [`Registry`](eval::Registry), then evaluate a query:
//!
//! ```ignore
//! let mut registry = fql_lang::eval::language_forms::<MyStore>();
//! registry.add("get", &["get"], &["ts"], my_get);
//! let mut ctx = fql_lang::eval::Ctx::new(&mut store, &registry, ts);
//! let value = fql_lang::eval::eval(&mut ctx, &Env::empty(), &query_json, &Position::root())?;
//! let response = fql_lang::wire::encode(&value);
//! ```
//!
//! Handlers receive the unevaluated arguments ([`Args`](eval::Args)) and
//! the [`Ctx`](eval::Ctx), whose `backend` field is your storage type, so
//! storage forms are ordinary functions over `Ctx<MyStore>`. The
//! `fql-emu-core` crate is a complete example (an in-memory emulator).
//! `tests/no_storage.rs` runs queries with no storage at all.

pub mod backend;
pub mod effect;
pub mod env;
pub mod error;
pub mod eval;
pub mod mode;
pub mod ordering;
pub mod value;
pub mod wire;

pub use backend::Backend;
pub use mode::Mode;
