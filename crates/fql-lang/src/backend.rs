//! The storage interface the language layer needs.
//!
//! The storage-independent forms (`let`, `map`, `select`, `concat`,
//! `time_add`, ...) only touch storage in a handful of places: `NewId()`,
//! `IsDoc`, `CurrentIdentity()`, `Call` of a stored function, the
//! permission checks of the key, the `invalid ref` errors the server's
//! casts raise for refs that do not resolve, and iterating a set (`Count`,
//! `Select` over a set, `Reduce`, ...). [`Backend`] is exactly that
//! surface. Everything else that touches storage (`Get`, `Create`,
//! `Match`, `Paginate`, ...) is a form the backend registers itself, with
//! full access to its own type through [`Ctx::backend`].

use crate::error::{Position, R};
use crate::eval::Ctx;
use crate::mode::Mode;
use crate::value::{Lambda, Ref, SetExpr, Value};

/// A storage engine the interpreter runs against. One value of the
/// implementing type is the state a transaction reads and writes; the
/// interpreter borrows it mutably for the whole query (see [`Ctx`]).
pub trait Backend: Sized {
    /// Key permissions the query runs under.
    fn mode(&self) -> Mode;

    /// `CurrentIdentity()`: the identity document of the token that sent
    /// the query, if any.
    fn identity(&self) -> Option<&Ref>;

    /// `NewId()`: a fresh document id.
    fn new_id(&mut self) -> String;

    /// Whether the schema object or document named by `r` exists.
    ///
    /// Used by `IsDoc` (for document refs) and for the server's
    /// unresolved-ref errors: a ref to a collection, index, function or
    /// role is checked as is, and a document ref by its collection
    /// (`Ref::collection(coll)`). On the server a schema object's *name*
    /// keeps resolving for the rest of the transaction that deletes it, so
    /// a backend that stages deletions should answer schema refs from the
    /// state at the start of the transaction (`Get` is what reports such
    /// an object as gone).
    fn exists_by_ref(&self, r: &Ref) -> bool;

    /// The body of the stored user-defined function `name` (a closure over
    /// the empty environment), or `None` if there is no such function.
    fn function_body(&self, name: &str) -> Option<&Lambda>;

    /// Materialise a set in iteration order: the values it yields. Takes
    /// the whole context because index bindings and `Join`/`Filter`
    /// targets are lambdas the interpreter has to apply.
    fn materialize(ctx: &mut Ctx<'_, Self>, set: &SetExpr, pos: &Position) -> R<Vec<Value>>;
}
