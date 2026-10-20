//! Sets: `Match`, `Documents`, `Singleton`, `Union`, `Intersection`,
//! `Difference`, `Distinct`, `Range`, `Join`, set `Filter`/`Reverse`, and
//! the brute-force set engine behind `Paginate` and the fold functions.
//!
//! Spec: ext/model/src/main/scala/ast/functions/SetFunctions.scala,
//! ext/model/src/main/scala/set/*.scala (set algebra),
//! ext/model/src/main/scala/doc/Index.scala (term extraction: `TermConfig`,
//! `legacyDataExtractor`), ext/storage/src/main/scala/index/IndexTerm.scala
//! (ordering) and ext/model/src/main/scala/ast/functions/ReadAdaptor.scala
//! (`paginate`, `PaginateArray`).
//!
//! There are no index data structures. `Match` scans every document in
//! the index's source collections, evaluates the index's terms/values
//! for each one, filters on the terms and sorts by (values..., ref).
//! That is the whole point: this is for small fixtures.

use super::forms::Registry;
use super::Ctx;
use crate::error::{one, FaunaError, Position, R};
use crate::value::{SetExpr, Value};

pub fn register(r: &mut Registry) {
    r.stub("match_index", &["match", "index"], &[]);
    r.stub("match_terms", &["match", "terms"], &[]);
    r.stub("match", &["match"], &[]);
    r.stub("documents", &["documents"], &[]);
    r.stub("singleton", &["singleton"], &[]);
    r.stub("events", &["events"], &[]);
    r.stub("union", &["union"], &[]);
    r.stub("intersection", &["intersection"], &[]);
    r.stub("difference", &["difference"], &[]);
    r.stub("distinct", &["distinct"], &[]);
    r.stub("range", &["range", "from", "to"], &[]);
    r.stub("join", &["join", "with"], &[]);
}

/// One materialised element of a set together with the sort key the
/// server would store in the index (`values..., ref`).
#[derive(Clone, Debug)]
pub struct Entry {
    /// Index values (the `values` of the index), used for ordering and as
    /// the page cursor. For `Documents()` and term-only indexes this is
    /// just `[ref]`.
    pub key: Vec<Value>,
    /// What the set yields: a ref, or an array when the index has values.
    pub value: Value,
}

/// Materialise a set in iteration order: the values it yields (a ref, or
/// `[values..., ref]` for an index with values).
pub fn materialize(ctx: &mut Ctx, set: &SetExpr, pos: &Position) -> R<Vec<Value>> {
    Ok(materialize_entries(ctx, set, pos)?.into_iter().map(|e| e.value).collect())
}

/// One index entry for one document, after multi-value (array) expansion:
/// the server emits one entry per combination of term/value elements.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexEntry {
    pub terms: Vec<Value>,
    pub values: Vec<Value>,
}

/// Compute the index entries a document contributes to `index`
/// (`Index.scala`: `TermConfig.extractor`, `legacyDataExtractor`,
/// bindings via `SourceConfig.fields`). Used by `Match` and by the
/// unique-constraint check in `write.rs`. A document that is not in the
/// index's sources contributes no entries.
pub fn index_entries(
    _ctx: &mut Ctx,
    _index: &crate::store::IndexDef,
    _collection: &str,
    _id: &str,
    _doc: &crate::store::Document,
    pos: &Position,
) -> R<Vec<IndexEntry>> {
    one(FaunaError::not_emulated("index entry extraction", pos))
}

/// Materialise a set as ordered entries (with sort keys, for `Paginate`
/// cursors and `Range`).
pub fn materialize_entries(_ctx: &mut Ctx, _set: &SetExpr, pos: &Position) -> R<Vec<Entry>> {
    one(FaunaError::not_emulated("set materialisation", pos))
}
